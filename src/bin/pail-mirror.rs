//! pail-mirror: watch a Git repository on GitHub, forward every commit on a branch into a
//! pail store, and prove each forwarded commit reproduces the Git commit byte for byte.
//!
//! Proof per commit, using only data read back from the pail store:
//!   1. every file's bytes → Git blob id (SHA-1 of "blob <len>\0<bytes>")
//!   2. the manifest (paths, kinds, blob ids) → Git tree id
//!   3. the pail commit (author, committer, dates, message, Git parents, extra headers)
//!      plus the rebuilt tree id → Git commit id
//! The rebuilt commit id must equal the Git commit id. SHA-1 covers every byte of every
//! file, every path and mode, and the commit metadata, so a match is a byte-for-byte model.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use pail::gitsrc::{Git, RawCommit, commit_bytes, kind_for_mode, object_id, tree_id};
use pail::hash::Hash;
use pail::manifest::Entry;
use pail::repo::{CommitOptions, CommitRequest, Delta, FileInput, Repo, Upload};
use pail::store::Store;
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "pail-mirror",
    about = "Mirror a GitHub branch into pail and prove every commit byte for byte"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Watch GitHub and forward new commits to pail (runs until stopped unless --once).
    Run(RunArgs),
    /// Re-prove an already mirrored pail branch against Git, from the store alone.
    Verify(VerifyArgs),
    /// How many small packs the tip keeps its files in, at the newest commits (measuring).
    Packs(PacksArgs),
}

#[derive(clap::Args)]
struct PacksArgs {
    /// pail store URL.
    #[arg(long)]
    store: String,
    #[arg(long, default_value = "main")]
    branch: String,
    /// Commits to look back over, newest first.
    #[arg(short = 'n', default_value_t = 500)]
    n: usize,
    /// Report every this many commits.
    #[arg(long, default_value_t = 50)]
    every: usize,
}

#[derive(clap::Args, Clone)]
struct Common {
    /// Local bare mirror of the GitHub repository (created if missing).
    #[arg(long)]
    git_dir: PathBuf,
    /// pail store URL (s3://, gs://, file://).
    #[arg(long)]
    store: String,
    /// pail branch that receives the commits.
    #[arg(long, default_value = "main")]
    pail_branch: String,
    /// Where the harness keeps its logs and blob-id cache.
    #[arg(long)]
    state_dir: PathBuf,
}

#[derive(clap::Args)]
struct RunArgs {
    #[command(flatten)]
    common: Common,
    /// GitHub repository, e.g. ArchAstro/firstlanding.
    #[arg(long)]
    github: String,
    /// Branch to watch on GitHub.
    #[arg(long, default_value = "main")]
    branch: String,
    /// Existing local clone to borrow objects from when creating the mirror.
    #[arg(long)]
    reference: Option<PathBuf>,
    /// First commit to import when the pail branch is empty (default: tip~N with --backfill N).
    #[arg(long)]
    start: Option<String>,
    /// Commits of history to replay before following live pushes (when --start is not given).
    #[arg(long, default_value_t = 50)]
    backfill: usize,
    /// Seconds between GitHub fetches.
    #[arg(long, default_value_t = 30)]
    poll: u64,
    /// Exit once caught up with GitHub.
    #[arg(long)]
    once: bool,
    /// Stop after forwarding this many commits.
    #[arg(long)]
    max_commits: Option<usize>,
    /// Full cold checkout comparison every N commits (0 = only at start and exit).
    #[arg(long, default_value_t = 25)]
    deep_every: usize,
    /// Serve a live dashboard, e.g. 127.0.0.1:8790.
    #[arg(long)]
    serve: Option<String>,
    /// Store the files of up to this many pending commits in one pack, in path order,
    /// instead of a pack per commit (fewer, larger packs; a cold checkout reads fewer
    /// objects). 1 = a pack per commit.
    #[arg(long, default_value_t = 1)]
    pack_batch: usize,
    /// Commit as `pail commit` does (the files and the commit go up in one wave, as
    /// `Worktree::commit` sends them), for measuring the commit path.
    #[arg(long)]
    client_flow: bool,
    /// With --client-flow: relocate a bounded part of the base's small packs into each
    /// commit's pack (amortized compaction, `pail::amortize`), reading the bytes from Git as
    /// a clean working copy would hold them.
    #[arg(long)]
    amortize: bool,
    /// With --amortize: only every other commit (even `seq`) relocates, to compare the two
    /// kinds of commit under the same network and the same store.
    #[arg(long)]
    alternate: bool,
}

#[derive(clap::Args)]
struct VerifyArgs {
    #[command(flatten)]
    common: Common,
    /// Number of commits to verify, newest first (default: all).
    #[arg(short = 'n')]
    n: Option<usize>,
}

// ---------------------------------------------------------------------------------------
// Dashboard state

#[derive(Serialize, Clone, Default)]
struct Record {
    seq: usize,
    git: String,
    pail: String,
    subject: String,
    author: String,
    committed_at: i64,
    mirrored_at: u64,
    changed: usize,
    files: u64,
    bytes_up: u64,
    puts: u64,
    gets: u64,
    import_ms: u128,
    verify_ms: u128,
    contents_checked: usize,
    tree_match: bool,
    commit_match: bool,
    attempts: u32,
    note: String,
}

#[derive(Serialize, Clone, Default)]
struct DeepCheck {
    at: u64,
    git: String,
    files: usize,
    bytes: u64,
    ms: u128,
    ok: bool,
    detail: String,
}

#[derive(Serialize, Clone, Default)]
struct Totals {
    commits: usize,
    proven: usize,
    failed: usize,
    changed_files: usize,
    bytes_up: u64,
    puts: u64,
    gets: u64,
    contents_checked: usize,
}

#[derive(Serialize, Clone, Default)]
struct Dash {
    github: String,
    branch: String,
    store: String,
    pail_branch: String,
    started_at: u64,
    phase: String,
    github_tip: String,
    github_tip_subject: String,
    last_fetch_at: u64,
    next_fetch_at: u64,
    pail_head: String,
    mirrored_git: String,
    behind: usize,
    repo_files: u64,
    repo_bytes: u64,
    totals: Totals,
    recent: VecDeque<Record>,
    deep_checks: Vec<DeepCheck>,
    errors: VecDeque<String>,
}

type Shared = Arc<Mutex<Dash>>;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn set_phase(d: &Shared, p: impl Into<String>) {
    let p = p.into();
    eprintln!("· {p}");
    d.lock().unwrap().phase = p;
}

fn push_error(d: &Shared, e: String) {
    eprintln!("! {e}");
    let mut g = d.lock().unwrap();
    g.errors.push_front(format!("{} {e}", now()));
    g.errors.truncate(20);
}

// ---------------------------------------------------------------------------------------
// Blob id cache: pail content hash → Git blob id, learned by reading content back from pail.

struct BlobIds {
    map: HashMap<Hash, String>,
    file: std::fs::File,
}

impl BlobIds {
    fn open(state: &Path) -> Result<BlobIds> {
        std::fs::create_dir_all(state)?;
        let path = state.join("blob-ids.tsv");
        let mut map = HashMap::new();
        if let Ok(s) = std::fs::read_to_string(&path) {
            for l in s.lines() {
                if let Some((h, o)) = l.split_once('\t') {
                    if let Ok(h) = Hash::from_hex(h) {
                        map.insert(h, o.to_string());
                    }
                }
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(BlobIds { map, file })
    }

    fn insert(&mut self, h: Hash, oid: String) -> Result<()> {
        writeln!(self.file, "{}\t{oid}", h.hex())?;
        self.map.insert(h, oid);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Import

fn meta_for(sha: &str, raw: &RawCommit) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("git.commit".into(), sha.to_string());
    m.insert("git.parents".into(), raw.parents.join(" "));
    m.insert("git.tree".into(), raw.tree.clone());
    if !raw.extra_headers.is_empty() {
        match std::str::from_utf8(&raw.extra_headers) {
            Ok(s) => m.insert("git.extra_headers".into(), s.to_string()),
            Err(_) => m.insert(
                "git.extra_headers_hex".into(),
                hex::encode(&raw.extra_headers),
            ),
        };
    }
    if std::str::from_utf8(&raw.message).is_err() {
        m.insert("git.message_hex".into(), hex::encode(&raw.message));
    }
    m
}

fn request(
    branch: &str,
    base: Option<Hash>,
    delta: Delta,
    sha: &str,
    raw: &RawCommit,
) -> CommitRequest {
    CommitRequest {
        branch: branch.to_string(),
        base,
        delta,
        author: raw.author.clone(),
        author_date: raw.author_date.clone(),
        committer: raw.committer.clone(),
        committer_date: raw.committer_date.clone(),
        message: String::from_utf8_lossy(&raw.message).into_owned(),
        meta: meta_for(sha, raw),
    }
}

/// Store file contents in memory-bounded batches.
async fn write_batched(
    repo: &Repo,
    git: &Git,
    items: Vec<(String, String, String)>,
    reuse: &HashMap<Hash, pail::manifest::Loc>,
) -> Result<Vec<Entry>> {
    let mut out = vec![];
    let mut batch: Vec<FileInput> = vec![];
    let mut size = 0usize;
    for (path, mode, oid) in items {
        let data = git.cat(&oid)?;
        size += data.len();
        batch.push(FileInput {
            path,
            kind: kind_for_mode(&mode)?,
            data: Bytes::from(data),
        });
        if size >= 256 << 20 {
            out.extend(repo.write_files(&batch, reuse).await?);
            batch.clear();
            size = 0;
        }
    }
    out.extend(repo.write_files(&batch, reuse).await?);
    Ok(out)
}

/// Import a commit's full tree as the first commit of the pail branch.
async fn import_full(
    repo: &Repo,
    git: &Git,
    branch: &str,
    sha: &str,
) -> Result<(Hash, usize, u32)> {
    let raw = git.commit_info(sha)?;
    let items: Vec<_> = git
        .ls_tree(sha)?
        .into_iter()
        .map(|t| (t.path, t.mode, t.oid))
        .collect();
    let n = items.len();
    let entries = write_batched(repo, git, items, &HashMap::new()).await?;
    let delta: Delta = entries
        .into_iter()
        .map(|e| (e.path.clone(), Some(e)))
        .collect();
    let out = repo.commit(request(branch, None, delta, sha, &raw)).await?;
    Ok((out.commit, n, out.attempts))
}

/// Import one commit as a delta against its first parent.
async fn import_delta(
    repo: &Repo,
    git: &Git,
    branch: &str,
    base: Hash,
    sha: &str,
) -> Result<(Hash, usize, u32)> {
    let raw = git.commit_info(sha)?;
    let parent = raw.parents.first().context("commit has no parent")?;
    let changes = git.diff_tree(parent, sha)?;
    let mut delta: Delta = BTreeMap::new();
    let mut items = vec![];
    for c in &changes {
        if c.deleted {
            delta.insert(c.path.clone(), None);
        } else {
            items.push((c.path.clone(), c.new_mode.clone(), c.new_oid.clone()));
        }
    }
    // Moves and reverts reuse bytes already in the store.
    let base_root = repo.root_of(Some(base)).await?;
    let reuse: HashMap<Hash, pail::manifest::Loc> = repo
        .entries(base_root)
        .await?
        .into_iter()
        .map(|e| (e.hash, e.loc))
        .collect();
    for e in write_batched(repo, git, items, &reuse).await? {
        delta.insert(e.path.clone(), Some(e));
    }
    let out = repo
        .commit(request(branch, Some(base), delta, sha, &raw))
        .await?;
    Ok((out.commit, changes.len(), out.attempts))
}

/// Import one commit as `Worktree::commit` would send it (`--client-flow`): the new files and,
/// when `amortize`, a bounded relocation of the base's small packs go in one pack uploaded
/// with the commit. Git supplies the bytes a clean working copy would hold.
async fn import_client_flow(
    repo: &Repo,
    git: &Git,
    branch: &str,
    base: Hash,
    sha: &str,
    amortize: Option<&HashMap<Hash, String>>,
) -> Result<(Hash, usize, u32, usize)> {
    let raw = git.commit_info(sha)?;
    let parent = raw.parents.first().context("commit has no parent")?;
    let changes = git.diff_tree(parent, sha)?;
    let mut delta: Delta = BTreeMap::new();
    let mut inputs: Vec<FileInput> = vec![];
    for c in &changes {
        if c.deleted {
            delta.insert(c.path.clone(), None);
        } else {
            inputs.push(FileInput {
                path: c.path.clone(),
                kind: kind_for_mode(&c.new_mode)?,
                data: Bytes::from(git.cat(&c.new_oid)?),
            });
        }
    }
    let t0 = Instant::now();
    let base_root = repo.root_of(Some(base)).await?;
    let entries = repo.entries(base_root).await?;
    let reuse: HashMap<Hash, pail::manifest::Loc> =
        entries.iter().map(|e| (e.hash, e.loc.clone())).collect();
    let t_entries = t0.elapsed();
    let mut moving = vec![];
    if let Some(ids) = amortize {
        let mut settings = pail::amortize::Settings::default();
        if let Some(n) = std::env::var("PAIL_MIRROR_MINPACKS")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            settings.min_packs = n;
        }
        let leaves = pail::amortize::Leaves::of(&repo.leaf_refs(base_root).await?.0);
        let own: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        let cands = pail::amortize::candidates(&settings, entries.iter(), &own, &leaves);
        if !cands.is_empty() {
            let budget = settings.budget(pail::amortize::new_bytes(
                inputs.iter().map(|i| i.data.len() as u64),
            ));
            let touched = pail::amortize::touched_leaves(&leaves, own.iter().copied());
            moving = pail::amortize::gather_with(&cands, &leaves, budget, touched, |e| {
                let data = git.cat(ids.get(&e.hash)?).ok()?;
                (Hash::of(&data) == e.hash).then(|| Bytes::from(data))
            });
        }
    }
    let t_plan = t0.elapsed();
    let (new_entries, relocs, uploads) = repo.prepare_files_moving(&inputs, &reuse, &moving);
    let t_prep = t0.elapsed();
    for e in new_entries {
        delta.insert(e.path.clone(), Some(e));
    }
    let moved = relocs.len();
    let tc = Instant::now();
    let out = repo
        .commit_opts(
            request(branch, Some(base), delta, sha, &raw),
            CommitOptions {
                uploads,
                relocs,
                ..Default::default()
            },
        )
        .await?;
    if std::env::var_os("PAIL_MIRROR_TIMING").is_some() {
        eprintln!(
            "  timing: entries {:?} plan {:?} prepare {:?} commit {:?} moved {moved}",
            t_entries,
            t_plan - t_entries,
            t_prep - t_plan,
            tc.elapsed()
        );
    }
    Ok((out.commit, changes.len(), out.attempts, moved))
}

/// The files of the next few commits, packed together (`--pack-batch`): a blob of a commit
/// is in the pack of the batch's first, so the batch writes one pack in path order, not one
/// per commit.
#[derive(Default)]
struct PackBatch {
    /// The commits whose files it holds.
    covers: HashSet<String>,
    /// Each distinct blob (Git id, mode): its size, content hash and place.
    files: HashMap<(String, String), (u64, Hash, pail::manifest::Loc)>,
    /// The pack(s) to store with the first of those commits.
    uploads: Vec<Upload>,
}

/// Bytes of blobs one batch holds in memory.
const BATCH_BYTES: usize = 256 << 20;

/// Pack the files of `shas` (at most `n`, as many as fit in memory), which are applied in
/// order on `base`.
async fn prepare_batch(
    repo: &Repo,
    git: &Git,
    base: Hash,
    shas: &[String],
    n: usize,
) -> Result<PackBatch> {
    let mut covers = HashSet::new();
    let mut keys: Vec<(String, String)> = vec![];
    let mut inputs: Vec<FileInput> = vec![];
    let mut seen = HashSet::new();
    let mut bytes = 0usize;
    for sha in shas.iter().take(n) {
        if bytes >= BATCH_BYTES {
            break;
        }
        let raw = git.commit_info(sha)?;
        let parent = raw.parents.first().context("commit has no parent")?;
        for c in git.diff_tree(parent, sha)? {
            let key = (c.new_oid.clone(), c.new_mode.clone());
            if !c.deleted && seen.insert(key.clone()) {
                let data = git.cat(&c.new_oid)?;
                bytes += data.len();
                inputs.push(FileInput {
                    path: c.path.clone(),
                    kind: kind_for_mode(&c.new_mode)?,
                    data: Bytes::from(data),
                });
                keys.push(key);
            }
        }
        covers.insert(sha.clone());
    }
    // Moves and reverts reuse bytes already in the store.
    let base_root = repo.root_of(Some(base)).await?;
    let reuse: HashMap<Hash, pail::manifest::Loc> = repo
        .entries(base_root)
        .await?
        .into_iter()
        .map(|e| (e.hash, e.loc))
        .collect();
    let (entries, uploads) = repo.prepare_files(&inputs, &reuse);
    let files = keys
        .into_iter()
        .zip(entries)
        .map(|(key, e)| (key, (e.size, e.hash, e.loc)))
        .collect();
    Ok(PackBatch {
        covers,
        files,
        uploads,
    })
}

/// Import one commit of a batch: its delta points into the batch's pack, which goes up with
/// the first commit.
async fn import_batched(
    repo: &Repo,
    git: &Git,
    branch: &str,
    base: Hash,
    sha: &str,
    batch: &mut PackBatch,
) -> Result<(Hash, usize, u32)> {
    let raw = git.commit_info(sha)?;
    let parent = raw.parents.first().context("commit has no parent")?;
    let changes = git.diff_tree(parent, sha)?;
    let mut delta: Delta = BTreeMap::new();
    for c in &changes {
        if c.deleted {
            delta.insert(c.path.clone(), None);
            continue;
        }
        let (size, hash, loc) = batch
            .files
            .get(&(c.new_oid.clone(), c.new_mode.clone()))
            .context("a file of the commit is not in its batch")?;
        delta.insert(
            c.path.clone(),
            Some(Entry {
                path: c.path.clone(),
                kind: kind_for_mode(&c.new_mode)?,
                size: *size,
                hash: *hash,
                loc: loc.clone(),
            }),
        );
    }
    let uploads = std::mem::take(&mut batch.uploads);
    let out = repo
        .commit_opts(
            request(branch, Some(base), delta, sha, &raw),
            CommitOptions {
                uploads,
                ..Default::default()
            },
        )
        .await?;
    Ok((out.commit, changes.len(), out.attempts))
}

// ---------------------------------------------------------------------------------------
// Proof

struct Proof {
    tree_match: bool,
    commit_match: bool,
    /// The commit's recorded change list equals the manifest diff from its parent.
    changes_match: bool,
    rebuilt_commit: String,
    contents_checked: usize,
    files: usize,
}

/// Rebuild the Git commit id from the pail store alone (no local caches).
async fn prove(
    verifier: &Repo,
    ids: &mut BlobIds,
    pail: Hash,
    git_sha: &str,
    git_tree: &str,
) -> Result<Proof> {
    let pc = verifier.commit_obj(&pail).await?;
    let entries = verifier.entries(pc.root).await?;
    let mut seen = HashSet::new();
    let missing: Vec<Entry> = entries
        .iter()
        .filter(|e| !ids.map.contains_key(&e.hash) && seen.insert(e.hash))
        .cloned()
        .collect();
    let checked = missing.len();
    let mut start = 0;
    while start < missing.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < missing.len() && (end == start || bytes < 256 << 20) {
            bytes += missing[end].size;
            end += 1;
        }
        let data = verifier.read_files(&missing[start..end]).await?;
        for (e, d) in missing[start..end].iter().zip(data) {
            ids.insert(e.hash, object_id("blob", &d))?;
        }
        start = end;
    }
    let files: Vec<(String, pail::manifest::Kind, String)> = entries
        .iter()
        .map(|e| (e.path.clone(), e.kind, ids.map[&e.hash].clone()))
        .collect();
    let tree = tree_id(&files)?;
    let message = match pc.meta.get("git.message_hex") {
        Some(h) => hex::decode(h)?,
        None => pc.message.clone().into_bytes(),
    };
    let extra_headers = match (
        pc.meta.get("git.extra_headers"),
        pc.meta.get("git.extra_headers_hex"),
    ) {
        (Some(s), _) => s.clone().into_bytes(),
        (_, Some(h)) => hex::decode(h)?,
        _ => vec![],
    };
    let parents: Vec<String> = pc
        .meta
        .get("git.parents")
        .map(|p| p.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let rebuilt = RawCommit {
        tree: tree.clone(),
        parents,
        author: pc.author.clone(),
        author_date: pc.author_date.clone(),
        committer: pc.committer.clone(),
        committer_date: pc.committer_date.clone(),
        extra_headers,
        message,
    };
    let rebuilt_commit = object_id("commit", &commit_bytes(&rebuilt));
    // The change list must be exactly the manifest diff from the parent (old side: kind, size,
    // hash; new side: those plus location).
    let changes_match = match pc.change_list() {
        None => true,
        Some(listed) => {
            let parent_root = verifier.root_of(pc.parent).await?;
            let actual = verifier.diff(parent_root, pc.root).await?;
            let key = |e: &Option<Entry>, loc: bool| {
                e.as_ref().map(|e| {
                    (
                        e.kind as u8,
                        e.size,
                        e.hash,
                        if loc {
                            format!("{:?}", e.loc)
                        } else {
                            String::new()
                        },
                    )
                })
            };
            listed.len() == actual.len()
                && listed.iter().zip(&actual).all(|(l, a)| {
                    l.path == a.path
                        && key(&l.old, false) == key(&a.old, false)
                        && key(&l.new, true) == key(&a.new, true)
                })
        }
    };
    Ok(Proof {
        // A recorded git.tree must be the tree rebuilt from stored content.
        tree_match: tree == git_tree && pc.meta.get("git.tree").is_none_or(|t| *t == tree),
        commit_match: rebuilt_commit == git_sha,
        changes_match,
        rebuilt_commit,
        contents_checked: checked,
        files: entries.len(),
    })
}

/// Cold checkout through the `pail` CLI compared file by file with Git's raw objects.
async fn deep_check(
    git: &Git,
    store: &str,
    branch: &str,
    git_sha: &str,
    state: &Path,
) -> Result<DeepCheck> {
    let t = Instant::now();
    let work = tempfile::tempdir_in(state)?;
    let pail_dir = work.path().join("pail");
    let bin = std::env::current_exe()?.with_file_name("pail");
    let out = std::process::Command::new(&bin)
        .args([
            "clone",
            store,
            pail_dir.to_str().unwrap(),
            "--branch",
            branch,
        ])
        .output()
        .with_context(|| format!("running {}", bin.display()))?;
    if !out.status.success() {
        bail!(
            "pail clone failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let tree = git.ls_tree(git_sha)?;
    let mut problems = vec![];
    let mut bytes = 0u64;
    let mut expected: HashSet<String> = HashSet::new();
    for t in &tree {
        expected.insert(t.path.clone());
        let want = git.cat(&t.oid)?;
        bytes += want.len() as u64;
        let p = pail_dir.join(&t.path);
        let got = match pail::worktree::read_disk(&p)? {
            Some((kind, data)) => {
                if kind != kind_for_mode(&t.mode)? {
                    problems.push(format!("{}: kind {kind:?} vs mode {}", t.path, t.mode));
                }
                data
            }
            None => {
                problems.push(format!("{}: missing", t.path));
                continue;
            }
        };
        if got != want {
            problems.push(format!("{}: bytes differ", t.path));
        }
    }
    for ent in walk(&pail_dir)? {
        if !expected.contains(&ent) {
            problems.push(format!("{ent}: extra file"));
        }
    }
    Ok(DeepCheck {
        at: now(),
        git: git_sha.to_string(),
        files: tree.len(),
        bytes,
        ms: t.elapsed().as_millis(),
        ok: problems.is_empty(),
        detail: if problems.is_empty() {
            "identical".into()
        } else {
            problems
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        },
    })
}

fn walk(root: &Path) -> Result<Vec<String>> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d)? {
            let ent = ent?;
            let ft = ent.file_type()?;
            let p = ent.path();
            if p.file_name().is_some_and(|n| n == ".pail") {
                continue;
            }
            if ft.is_dir() {
                stack.push(p);
            } else {
                out.push(p.strip_prefix(root)?.to_string_lossy().into_owned());
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Run loop

fn ensure_mirror(dir: &Path, github: &str, branch: &str, reference: Option<&Path>) -> Result<()> {
    if dir.join("HEAD").exists() {
        return Ok(());
    }
    let url = format!("https://github.com/{github}.git");
    let mut cmd = std::process::Command::new("git");
    cmd.args(["clone", "--bare", "--quiet"]);
    if let Some(r) = reference {
        cmd.arg("--reference-if-able").arg(r);
    }
    let st = cmd.arg(&url).arg(dir).status()?;
    anyhow::ensure!(st.success(), "git clone {url} failed");
    let st = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "config",
            "remote.origin.fetch",
            &format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
        ])
        .status()?;
    anyhow::ensure!(st.success());
    Ok(())
}

fn fetch(dir: &Path) -> Result<()> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["fetch", "--quiet", "origin"])
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "git fetch: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

fn append_log(state: &Path, rec: &impl Serialize) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.join("mirror.jsonl"))?;
    writeln!(f, "{}", serde_json::to_string(rec)?)?;
    Ok(())
}

fn committed_at(raw: &RawCommit) -> i64 {
    raw.committer_date
        .split(' ')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

async fn run(args: RunArgs) -> Result<()> {
    let c = &args.common;
    std::fs::create_dir_all(&c.state_dir)?;
    let dash: Shared = Arc::new(Mutex::new(Dash {
        github: args.github.clone(),
        branch: args.branch.clone(),
        store: c.store.clone(),
        pail_branch: c.pail_branch.clone(),
        started_at: now(),
        ..Default::default()
    }));
    if let Some(addr) = &args.serve {
        serve(addr, dash.clone())?;
        eprintln!("dashboard: http://{addr}/");
    }
    set_phase(&dash, "preparing GitHub mirror");
    ensure_mirror(
        &c.git_dir,
        &args.github,
        &args.branch,
        args.reference.as_deref(),
    )?;
    fetch(&c.git_dir)?;
    let git = Git::open(&c.git_dir)?;
    let tip_ref = format!("refs/remotes/origin/{}", args.branch);
    let repo = Repo::remote(&c.store)?;
    // The verifier has no disk cache: everything it proves was read from the store.
    let verifier = Repo::with_store(Store::open(&c.store)?, None);
    let mut ids = BlobIds::open(&c.state_dir)?;

    let mut forwarded = 0usize;
    let mut seq = 0usize;
    let mut since_deep = 0usize;
    let mut needs_deep = true;

    let (mut head, mut mirrored) = match repo.read_ref(&c.pail_branch).await? {
        Some((h, _)) => (h, mirrored_sha(&repo, h).await?),
        None => {
            let start = match &args.start {
                Some(s) => git.rev_parse(s)?,
                None => git.rev_parse(&format!("{tip_ref}~{}", args.backfill))?,
            };
            set_phase(&dash, format!("importing full tree of {}", &start[..10]));
            let t = Instant::now();
            let before = repo.store.stats.snapshot();
            let (h, n, attempts) = import_full(&repo, &git, &c.pail_branch, &start).await?;
            let import_ms = t.elapsed().as_millis();
            let raw = git.commit_info(&start)?;
            set_phase(&dash, format!("proving {}", &start[..10]));
            let t = Instant::now();
            let p = prove(&verifier, &mut ids, h, &start, &raw.tree).await?;
            let after = repo.store.stats.snapshot();
            let rec = Record {
                seq,
                git: start.clone(),
                pail: h.hex(),
                subject: format!(
                    "[full import] {}",
                    String::from_utf8_lossy(&raw.message)
                        .lines()
                        .next()
                        .unwrap_or("")
                ),
                author: raw.author.clone(),
                committed_at: committed_at(&raw),
                mirrored_at: now(),
                changed: n,
                files: p.files as u64,
                bytes_up: after.3 - before.3,
                puts: after.1 - before.1,
                gets: after.0 - before.0,
                import_ms,
                verify_ms: t.elapsed().as_millis(),
                contents_checked: p.contents_checked,
                tree_match: p.tree_match && p.changes_match,
                commit_match: p.commit_match,
                attempts,
                note: if !p.changes_match {
                    "change list differs from the manifest diff".to_string()
                } else if p.commit_match {
                    String::new()
                } else {
                    format!("rebuilt {}", p.rebuilt_commit)
                },
            };
            record(&dash, &c.state_dir, rec)?;
            seq += 1;
            (h, start)
        }
    };

    loop {
        set_phase(&dash, "fetching from GitHub");
        if let Err(e) = fetch(&c.git_dir) {
            push_error(&dash, format!("fetch failed: {e:#}"));
        }
        let tip = git.rev_parse(&tip_ref)?;
        let pending = git.first_parent_range(&mirrored, &tip)?;
        {
            let mut g = dash.lock().unwrap();
            g.github_tip = tip.clone();
            g.github_tip_subject = git
                .commit_info(&tip)
                .map(|r| {
                    String::from_utf8_lossy(&r.message)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .unwrap_or_default();
            g.last_fetch_at = now();
            g.behind = pending.len();
        }
        if let Some(first) = pending.first() {
            let raw = git.commit_info(first)?;
            if raw.parents.first() != Some(&mirrored) {
                bail!(
                    "GitHub history no longer descends from {} (force push?); refusing to rewrite pail history",
                    &mirrored[..10]
                );
            }
        }
        let mut batch = PackBatch::default();
        for (i, sha) in pending.iter().enumerate() {
            set_phase(&dash, format!("forwarding {}", &sha[..10]));
            let step = async {
                let raw = git.commit_info(sha)?;
                let before = repo.store.stats.snapshot();
                let t = Instant::now();
                let (h, changed, attempts) = if args.pack_batch > 1 {
                    if !batch.covers.contains(sha) {
                        batch = prepare_batch(&repo, &git, head, &pending[i..], args.pack_batch)
                            .await?;
                    }
                    import_batched(&repo, &git, &c.pail_branch, head, sha, &mut batch).await?
                } else if args.client_flow {
                    let ids_map =
                        (args.amortize && (!args.alternate || seq % 2 == 0)).then_some(&ids.map);
                    let (h, n, a, moved) =
                        import_client_flow(&repo, &git, &c.pail_branch, head, sha, ids_map).await?;
                    eprintln!("  relocated {moved} files");
                    (h, n, a)
                } else {
                    import_delta(&repo, &git, &c.pail_branch, head, sha).await?
                };
                let import_ms = t.elapsed().as_millis();
                let after = repo.store.stats.snapshot();
                let t = Instant::now();
                let p = prove(&verifier, &mut ids, h, sha, &raw.tree).await?;
                anyhow::Ok((
                    h,
                    Record {
                        seq,
                        git: sha.clone(),
                        pail: h.hex(),
                        subject: String::from_utf8_lossy(&raw.message)
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string(),
                        author: raw.author.clone(),
                        committed_at: committed_at(&raw),
                        mirrored_at: now(),
                        changed,
                        files: p.files as u64,
                        bytes_up: after.3 - before.3,
                        puts: after.1 - before.1,
                        gets: after.0 - before.0,
                        import_ms,
                        verify_ms: t.elapsed().as_millis(),
                        contents_checked: p.contents_checked,
                        tree_match: p.tree_match && p.changes_match,
                        commit_match: p.commit_match,
                        attempts,
                        note: if !p.changes_match {
                            "change list differs from the manifest diff".to_string()
                        } else if p.commit_match {
                            String::new()
                        } else {
                            format!("rebuilt {}", p.rebuilt_commit)
                        },
                    },
                ))
            };
            let (h, rec) = match step.await {
                Ok(v) => v,
                Err(e) => {
                    // Credentials expire, networks drop: wait and resume from what the store
                    // says was mirrored, rather than exiting. Proof failures are not errors
                    // here; they are recorded below.
                    push_error(
                        &dash,
                        format!("{}: {e:#}; retrying after the next poll", &sha[..10]),
                    );
                    eprintln!("  {}: {e:#}; retrying after the next poll", &sha[..10]);
                    // Its pack may not have been stored: the next poll packs afresh.
                    batch = PackBatch::default();
                    if let Ok(Some((h, _))) = repo.read_ref(&c.pail_branch).await
                        && let Ok(g) = mirrored_sha(&repo, h).await
                    {
                        (head, mirrored) = (h, g);
                    }
                    break;
                }
            };
            let ok = rec.commit_match && rec.tree_match;
            record(&dash, &c.state_dir, rec)?;
            if !ok {
                push_error(
                    &dash,
                    format!("{} did not reproduce byte for byte", &sha[..10]),
                );
            }
            head = h;
            mirrored = sha.clone();
            seq += 1;
            forwarded += 1;
            since_deep += 1;
            dash.lock().unwrap().behind -= 1;
            if args.deep_every > 0 && since_deep >= args.deep_every {
                needs_deep = true;
            }
            if args.max_commits.is_some_and(|m| forwarded >= m) {
                break;
            }
        }
        if needs_deep {
            set_phase(
                &dash,
                format!("deep check: cold pail clone vs git {}", &mirrored[..10]),
            );
            match deep_check(&git, &c.store, &c.pail_branch, &mirrored, &c.state_dir).await {
                Ok(d) => {
                    eprintln!(
                        "  deep check {}: {} files, {}",
                        &d.git[..10],
                        d.files,
                        d.detail
                    );
                    append_log(&c.state_dir, &serde_json::json!({"deep_check": &d}))?;
                    dash.lock().unwrap().deep_checks.insert(0, d);
                }
                Err(e) => push_error(&dash, format!("deep check failed: {e:#}")),
            }
            needs_deep = false;
            since_deep = 0;
        }
        let done = args.max_commits.is_some_and(|m| forwarded >= m);
        if args.once || done {
            set_phase(&dash, "finished");
            let g = dash.lock().unwrap();
            eprintln!(
                "forwarded {} commit(s); proven byte for byte: {}/{}; failures: {}",
                g.totals.commits, g.totals.proven, g.totals.commits, g.totals.failed
            );
            if g.totals.failed > 0 || g.deep_checks.iter().any(|d| !d.ok) {
                bail!("some commits did not reproduce");
            }
            return Ok(());
        }
        set_phase(&dash, "watching GitHub");
        dash.lock().unwrap().next_fetch_at = now() + args.poll;
        tokio::time::sleep(Duration::from_secs(args.poll)).await;
    }
}

fn record(dash: &Shared, state: &Path, rec: Record) -> Result<()> {
    eprintln!(
        "  {} → pail {}  {:>4} changed  import {:>5}ms  prove {:>5}ms  tree {} commit {}  {}",
        &rec.git[..10],
        &rec.pail[..12],
        rec.changed,
        rec.import_ms,
        rec.verify_ms,
        if rec.tree_match { "✓" } else { "✗" },
        if rec.commit_match { "✓" } else { "✗" },
        rec.subject.chars().take(60).collect::<String>()
    );
    append_log(state, &rec)?;
    let mut g = dash.lock().unwrap();
    g.totals.commits += 1;
    if rec.commit_match && rec.tree_match {
        g.totals.proven += 1;
    } else {
        g.totals.failed += 1;
    }
    g.totals.changed_files += rec.changed;
    g.totals.bytes_up += rec.bytes_up;
    g.totals.puts += rec.puts;
    g.totals.gets += rec.gets;
    g.totals.contents_checked += rec.contents_checked;
    g.pail_head = rec.pail.clone();
    g.mirrored_git = rec.git.clone();
    g.repo_files = rec.files;
    g.recent.push_front(rec);
    g.recent.truncate(500);
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Verify an existing mirror end to end.

/// The Git commit the pail branch at `head` has mirrored: the newest non-maintenance
/// commit's. A repack copies its parent's `git.*` meta, but resume does not rely on that: new
/// commits parent on the repack, and the history below it is what was mirrored.
async fn mirrored_sha(repo: &Repo, head: Hash) -> Result<String> {
    let (_, pc) = pail::repack::skip_maintenance(repo, head).await?;
    pc.meta
        .get("git.commit")
        .cloned()
        .context("pail branch was not created by pail-mirror")
}

/// Per sampled commit, oldest first: packs the tip names, those holding under 1 MiB of it,
/// and the bytes of all the packs (a full checkout reads about one request per pack).
async fn packs(args: PacksArgs) -> Result<()> {
    let repo = Repo::with_store(Store::open(&args.store)?, None);
    let (head, _) = repo
        .read_ref(&args.branch)
        .await?
        .context("no such branch")?;
    let history = repo.log(head, args.n).await?;
    println!("seq\tpacks\tsmall\tlive_small_bytes");
    for (i, (h, pc)) in history.iter().enumerate().rev() {
        if i % args.every != 0 && i != 0 {
            continue;
        }
        let _ = h;
        let mut live: HashMap<Hash, (HashSet<u64>, u64)> = HashMap::new();
        for e in repo.entries(pc.root).await? {
            if let pail::manifest::Loc::Pack { pack, off, len } = e.loc {
                let p = live.entry(pack).or_default();
                if p.0.insert(off) {
                    p.1 += len;
                }
            }
        }
        let small: Vec<u64> = live
            .values()
            .map(|p| p.1)
            .filter(|b| *b < pail::amortize::SMALL_PACK_BYTES)
            .collect();
        println!(
            "{}\t{}\t{}\t{}",
            history.len() - 1 - i,
            live.len(),
            small.len(),
            small.iter().sum::<u64>()
        );
    }
    Ok(())
}

async fn verify(args: VerifyArgs) -> Result<()> {
    let c = &args.common;
    let git = Git::open(&c.git_dir)?;
    let verifier = Repo::with_store(Store::open(&c.store)?, None);
    let mut ids = BlobIds::open(&c.state_dir)?;
    let (head, _) = verifier
        .read_ref(&c.pail_branch)
        .await?
        .context("pail branch not found")?;
    let history = verifier.log(head, args.n.unwrap_or(usize::MAX)).await?;
    let (mut ok, mut bad) = (0, 0);
    for (h, pc) in history.iter().rev() {
        // A repack changes no file and answers to no Git commit.
        if pc.is_maintenance() {
            continue;
        }
        let sha = pc
            .meta
            .get("git.commit")
            .context("commit lacks git.commit")?;
        let raw = git.commit_info(sha)?;
        let p = prove(&verifier, &mut ids, *h, sha, &raw.tree).await?;
        if p.commit_match && p.tree_match && p.changes_match {
            ok += 1;
        } else {
            bad += 1;
        }
        println!(
            "{} {} → rebuilt {}  {} files  {}",
            if p.commit_match { "✓" } else { "✗" },
            &sha[..12],
            &p.rebuilt_commit[..12],
            p.files,
            pc.subject().chars().take(60).collect::<String>()
        );
    }
    println!(
        "{ok}/{} commits reproduce their Git commit id byte for byte",
        ok + bad
    );
    anyhow::ensure!(bad == 0, "{bad} commit(s) failed");
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Dashboard

const DASHBOARD: &str = include_str!("dashboard.html");

fn serve(addr: &str, dash: Shared) -> Result<()> {
    let server = tiny_http::Server::http(addr).map_err(|e| anyhow::anyhow!("serve {addr}: {e}"))?;
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let (body, ctype) = if req.url().starts_with("/api/status") {
                (
                    serde_json::to_string(&*dash.lock().unwrap()).unwrap_or_default(),
                    "application/json",
                )
            } else {
                (DASHBOARD.to_string(), "text/html; charset=utf-8")
            };
            let header =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap();
            let _ = req.respond(tiny_http::Response::from_string(body).with_header(header));
        }
    });
    Ok(())
}

#[tokio::main]
async fn main() {
    let res = match Cli::parse().cmd {
        Cmd::Run(a) => run(a).await,
        Cmd::Verify(a) => verify(a).await,
        Cmd::Packs(a) => packs(a).await,
    };
    if let Err(e) = res {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_cmd(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e.x",
            ])
            .args(args)
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn body(tag: &str, n: usize) -> String {
        format!("{tag} {n}\n{}\n", "pack me ".repeat(40))
    }

    /// A Git repository of a first commit and six more, each adding two files.
    fn history(dir: &Path) -> Vec<String> {
        std::fs::create_dir_all(dir).unwrap();
        git_cmd(dir, &["init", "-q", "-b", "main"]);
        for k in 0..=6 {
            for (sub, name) in [("a", "n"), ("b", "m")] {
                let p = dir.join(sub).join(format!("{name}{k}.txt"));
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, body(name, k)).unwrap();
            }
            git_cmd(dir, &["add", "-A"]);
            git_cmd(dir, &["commit", "-q", "-m", &format!("commit {k}")]);
        }
        git_cmd(dir, &["rev-list", "--reverse", "HEAD"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Mirror the history with `batch` commits per pack; every commit must prove. Returns
    /// the repository.
    async fn mirror(root: &Path, git_dir: &Path, shas: &[String], batch: usize) -> Repo {
        let url = format!("file://{}", root.join(format!("store{batch}")).display());
        let repo = Repo::with_store(Store::open(&url).unwrap(), None);
        let verifier = Repo::with_store(Store::open(&url).unwrap(), None);
        let mut ids = BlobIds::open(&root.join(format!("state{batch}"))).unwrap();
        let git = Git::open(git_dir).unwrap();
        let (mut head, ..) = import_full(&repo, &git, "main", &shas[0]).await.unwrap();
        let mut pending = PackBatch::default();
        for (i, sha) in shas.iter().enumerate().skip(1) {
            let (h, ..) = if batch > 1 {
                if !pending.covers.contains(sha) {
                    pending = prepare_batch(&repo, &git, head, &shas[i..], batch)
                        .await
                        .unwrap();
                }
                import_batched(&repo, &git, "main", head, sha, &mut pending)
                    .await
                    .unwrap()
            } else {
                import_delta(&repo, &git, "main", head, sha).await.unwrap()
            };
            let raw = git.commit_info(sha).unwrap();
            let p = prove(&verifier, &mut ids, h, sha, &raw.tree).await.unwrap();
            assert!(p.commit_match && p.tree_match && p.changes_match, "{sha}");
            head = h;
        }
        repo
    }

    async fn tip(r: &Repo) -> Vec<Entry> {
        let (h, _) = r.read_ref("main").await.unwrap().unwrap();
        let root = r.commit_obj(&h).await.unwrap().root;
        r.entries(root).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_writes_one_pack_for_several_commits_and_still_proves() {
        let tmp = tempfile::tempdir().unwrap();
        let g = tmp.path().join("g");
        let shas = history(&g);
        let single = mirror(tmp.path(), &g, &shas, 1).await;
        let batched = mirror(tmp.path(), &g, &shas, 3).await;
        // The first commit's pack, then a pack per commit, or per three.
        assert_eq!(single.store.list("packs/").await.unwrap().len(), 7);
        assert_eq!(batched.store.list("packs/").await.unwrap().len(), 3);

        // Same files either way; in a batch, a pack's files are in path order.
        let (a, b) = (tip(&single).await, tip(&batched).await);
        assert_eq!(
            a.iter().map(|e| (&e.path, e.hash)).collect::<Vec<_>>(),
            b.iter().map(|e| (&e.path, e.hash)).collect::<Vec<_>>()
        );
        let at = |e: &Entry| match e.loc {
            pail::manifest::Loc::Pack { pack, off, .. } => (pack, off),
            _ => panic!("{e:?}"),
        };
        // Commits 1-3 are one batch: a/n1..n3 and b/m1..m3 share a pack, a's before b's.
        let in_batch = |name: &str| b.iter().find(|e| e.path == name).map(at).unwrap();
        let (p1, o1) = in_batch("a/n1.txt");
        let (p2, o2) = in_batch("a/n3.txt");
        let (p3, o3) = in_batch("b/m1.txt");
        assert!(p1 == p2 && p2 == p3 && o1 < o2 && o2 < o3);
    }
}
