//! Tip repack (`pail maintenance repack`): move the files a branch's tip keeps in many small
//! packs into a few large, path-ordered ones.
//!
//! The repack is an ordinary commit on the branch. Its tree has the same content as the
//! tip's (so the same Git tree ids, and an empty change list); its delta only re-points each
//! moved path's `Loc::Pack` at a new pack. The stored bytes of every blob are copied verbatim
//! (no recompression) after checking the blob's hash, the new packs go up with the commit
//! (`CommitOptions::uploads`) and the ref moves by the usual compare-and-swap, so the
//! protocol, and the argument that gc and writers stay safe, are `Repo::commit_opts`'s
//! (`verify/README.md`, "Packs and repacking"). The old packs stay: history names them.
//!
//! Tiered by default: only files in packs under [`Options::min_pack_bytes`] move, so dense
//! packs stay and repeated repacks stay cheap. `--full` moves everything.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;

use crate::hash::Hash;
use crate::manifest::{Entry, Loc};
use crate::pack::{PackBuilder, decode_blob, pack_key};
use crate::repo::{
    CommitOptions, CommitRequest, ConflictError, Delta, PARALLELISM, Repo, Upload, run_unordered,
};

/// The meta key marking a commit as maintenance (a repack): history views hide it.
pub const MAINTENANCE_META: &str = "pail.maintenance";
/// Its value for a repack.
pub const REPACK: &str = "repack";

/// New packs close at about this many bytes (4 MiB measured best: larger packs stall on
/// single-stream GETs).
pub const DEFAULT_TARGET_BYTES: u64 = 4 << 20;
/// Tiered repack moves files from packs smaller than this.
pub const DEFAULT_MIN_PACK_BYTES: u64 = 1 << 20;
/// Attempts before giving up on a branch that keeps changing the paths being moved.
pub const DEFAULT_ATTEMPTS: usize = 5;

#[derive(Debug, Clone)]
pub struct Options {
    pub branch: String,
    /// Move every packed file, not only those in small packs.
    pub full: bool,
    pub target_bytes: u64,
    pub min_pack_bytes: u64,
    /// Plan and report; write nothing.
    pub dry_run: bool,
    pub attempts: usize,
}

impl Options {
    pub fn new(branch: &str) -> Options {
        Options {
            branch: branch.to_string(),
            full: false,
            target_bytes: DEFAULT_TARGET_BYTES,
            min_pack_bytes: DEFAULT_MIN_PACK_BYTES,
            dry_run: false,
            attempts: DEFAULT_ATTEMPTS,
        }
    }
}

/// Who commits the repack, and when.
#[derive(Debug, Clone)]
pub struct Who {
    pub author: String,
    pub committer: String,
    /// `<unix seconds> <+hhmm>`.
    pub date: String,
}

/// What a repack moves, and what it saves a full checkout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Files of the tip stored in packs, and the packs holding them.
    pub packed_files: usize,
    pub packs: usize,
    /// Files moved, the distinct blobs among them, their stored bytes, and the packs they
    /// leave.
    pub moved_files: usize,
    pub moved_blobs: usize,
    pub moved_bytes: u64,
    pub source_packs: usize,
    /// Packs the move writes.
    pub new_packs: usize,
    /// Packs a full checkout reads (one request each, at least), before and after.
    pub requests_before: usize,
    pub requests_after: usize,
}

/// The entries to move, path-ordered, and what that does.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub moves: Vec<Entry>,
    pub stats: Stats,
}

/// What a repack did.
#[derive(Debug)]
pub enum Outcome {
    /// Nothing worth moving.
    Nothing(Stats),
    /// A dry run: the plan.
    Planned(Stats),
    Done {
        stats: Stats,
        commit: Hash,
        /// The tip the repack was made on.
        parent: Hash,
        /// Plans made: more than one when a concurrent commit changed a path being moved.
        attempts: usize,
        /// Paths dropped from the move because a concurrent commit changed them.
        dropped: usize,
    },
}

/// Which new pack each blob (by stored size, in order) goes in: a pack closes once it holds
/// `target` bytes.
pub fn layout(stored: &[u64], target: u64) -> Vec<usize> {
    let (mut pack, mut filled) = (0, 0u64);
    stored
        .iter()
        .map(|&len| {
            if filled >= target && filled > 0 {
                (pack, filled) = (pack + 1, 0);
            }
            filled += len;
            pack
        })
        .collect()
}

fn packed(e: &Entry) -> Option<(Hash, u64, u64)> {
    match &e.loc {
        Loc::Pack { pack, off, len } => Some((*pack, *off, *len)),
        _ => None,
    }
}

/// The entries to move: those in packs under `min_pack_bytes` (all packed ones if `full`),
/// in path order. A tiered repack of fewer than two packs would only copy a pack, so it
/// moves nothing. `pack_sizes` maps each pack to its size in the store.
pub fn plan(entries: &[Entry], pack_sizes: &HashMap<Hash, u64>, opts: &Options) -> Plan {
    let tip_packs: HashSet<Hash> = entries
        .iter()
        .filter_map(|e| packed(e).map(|p| p.0))
        .collect();
    let small =
        |p: &Hash| opts.full || pack_sizes.get(p).copied().unwrap_or(0) < opts.min_pack_bytes;
    let sources: HashSet<Hash> = tip_packs.iter().filter(|p| small(p)).copied().collect();
    let moves: Vec<Entry> = if sources.len() < 2 {
        vec![]
    } else {
        entries
            .iter()
            .filter(|e| packed(e).is_some_and(|p| sources.contains(&p.0)))
            .cloned()
            .collect()
    };
    let stats = describe(entries, &moves, opts);
    Plan { moves, stats }
}

/// What moving `moves` out of `entries` does.
fn describe(entries: &[Entry], moves: &[Entry], opts: &Options) -> Stats {
    let tip_packs: HashSet<Hash> = entries
        .iter()
        .filter_map(|e| packed(e).map(|p| p.0))
        .collect();
    let moved: HashSet<&str> = moves.iter().map(|e| e.path.as_str()).collect();
    let staying: HashSet<Hash> = entries
        .iter()
        .filter(|e| !moved.contains(e.path.as_str()))
        .filter_map(|e| packed(e).map(|p| p.0))
        .collect();
    let mut seen = HashSet::new();
    let blobs: Vec<u64> = moves
        .iter()
        .filter(|e| seen.insert(e.hash))
        .map(|e| packed(e).map_or(0, |p| p.2))
        .collect();
    let new_packs = layout(&blobs, opts.target_bytes)
        .last()
        .map_or(0, |n| n + 1);
    Stats {
        packed_files: entries.iter().filter(|e| packed(e).is_some()).count(),
        packs: tip_packs.len(),
        moved_files: moves.len(),
        moved_blobs: blobs.len(),
        moved_bytes: blobs.iter().sum(),
        source_packs: tip_packs.len() - staying.len(),
        new_packs,
        requests_before: tip_packs.len(),
        requests_after: staying.len() + new_packs,
    }
}

/// Every pack in the store with its size (one listing).
async fn pack_sizes(repo: &Repo) -> Result<HashMap<Hash, u64>> {
    let mut out = HashMap::new();
    for (key, size) in repo.store.list("packs/").await? {
        let name = key.trim_start_matches("packs/").trim_end_matches(".pack");
        if let Ok(h) = Hash::from_hex(name) {
            out.insert(h, size);
        }
    }
    Ok(out)
}

/// The stored bytes of the blobs `moves` name, by content hash: the ones in `have` kept,
/// the rest read from their packs (one request per pack, concurrently) and hash-checked.
async fn read_blobs(repo: &Repo, moves: &[Entry], have: &mut HashMap<Hash, Bytes>) -> Result<()> {
    let mut by_pack: BTreeMap<Hash, Vec<&Entry>> = BTreeMap::new();
    let mut seen = HashSet::new();
    for e in moves {
        if let Some((pack, ..)) = packed(e)
            && !have.contains_key(&e.hash)
            && seen.insert(e.hash)
        {
            by_pack.entry(pack).or_default().push(e);
        }
    }
    let reads = by_pack.into_iter().map(|(pack, es)| async move {
        let ranges: Vec<Range<u64>> = es
            .iter()
            .filter_map(|e| packed(e).map(|(_, off, len)| off..off + len))
            .collect();
        let got = repo.store.get_ranges(&pack_key(&pack), &ranges).await?;
        ensure!(
            got.len() == es.len(),
            "short read from pack {}",
            pack.short()
        );
        let mut out = Vec::with_capacity(es.len());
        for (e, stored) in es.into_iter().zip(got) {
            let raw = decode_blob(&stored, e.size)
                .with_context(|| format!("{}: a blob of pack {}", e.path, pack.short()))?;
            ensure!(
                Hash::of(&raw) == e.hash,
                "{}: a blob of pack {} does not match its hash",
                e.path,
                pack.short()
            );
            out.push((e.hash, stored));
        }
        Ok(out)
    });
    for blobs in run_unordered(reads.collect(), PARALLELISM).await? {
        have.extend(blobs);
    }
    Ok(())
}

/// The new packs for `moves` (path-ordered, blobs copied as stored) and the delta that
/// points the paths into them.
fn build(
    moves: &[Entry],
    blobs: &HashMap<Hash, Bytes>,
    target: u64,
) -> Result<(Delta, Vec<Upload>)> {
    let mut order: Vec<&Entry> = vec![];
    let mut seen = HashSet::new();
    for e in moves {
        if seen.insert(e.hash) {
            order.push(e);
        }
    }
    let stored: Vec<&Bytes> = order
        .iter()
        .map(|e| blobs.get(&e.hash).context("a blob was not read"))
        .collect::<Result<_>>()?;
    let sizes: Vec<u64> = stored.iter().map(|b| b.len() as u64).collect();
    let which = layout(&sizes, target);
    let mut builders: Vec<PackBuilder> = (0..which.last().map_or(0, |n| n + 1))
        .map(|_| PackBuilder::default())
        .collect();
    let mut at: HashMap<Hash, (usize, u64, u64)> = HashMap::new();
    for ((e, bytes), n) in order.iter().zip(&stored).zip(&which) {
        let (off, len) = builders[*n].add_stored(e.hash, bytes, e.size);
        at.insert(e.hash, (*n, off, len));
    }
    let finished: Vec<(Hash, Bytes)> = builders.into_iter().map(PackBuilder::finish).collect();
    let mut delta = Delta::new();
    for e in moves {
        let (n, off, len) = at[&e.hash];
        let mut moved = e.clone();
        moved.loc = Loc::Pack {
            pack: finished[n].0,
            off,
            len,
        };
        delta.insert(e.path.clone(), Some(moved));
    }
    let uploads = finished
        .iter()
        .map(|(h, data)| Upload {
            key: pack_key(h),
            data: data.clone(),
            check_first: false,
        })
        .collect();
    Ok((delta, uploads))
}

/// Called with the attempt number (from 1) after the packs are built and before they are
/// published. Tests use it to land a concurrent commit.
pub type Hook<'a> = Option<&'a (dyn Fn(usize) + Send + Sync)>;

/// Repack the tip of `opts.branch`: plan, read the blobs, write the packs and the commit.
///
/// If a concurrent commit changes a path being moved (`ConflictError`), those paths are
/// dropped from the move and the rest is published on the new tip, up to `opts.attempts`
/// times. A concurrent commit that changes other paths needs no retry: `commit_opts` replays
/// the delta on it.
pub async fn repack(repo: &Repo, opts: &Options, who: &Who, hook: Hook<'_>) -> Result<Outcome> {
    let sizes = pack_sizes(repo).await?;
    let mut blobs: HashMap<Hash, Bytes> = HashMap::new();
    let mut moves: Option<Vec<Entry>> = None;
    let mut dropped = 0;
    for attempt in 1..=opts.attempts.max(1) {
        let (tip, _) = repo
            .read_ref(&opts.branch)
            .await?
            .with_context(|| format!("branch {} not found", opts.branch))?;
        let tip_commit = repo.commit_obj(&tip).await?;
        let entries = repo.entries(tip_commit.root).await?;
        let (stats, now) = match moves.take() {
            None => {
                let p = plan(&entries, &sizes, opts);
                (p.stats, p.moves)
            }
            // A retry: what was planned, less what changed since.
            Some(before) => {
                let by_path: HashMap<&str, &Entry> =
                    entries.iter().map(|e| (e.path.as_str(), e)).collect();
                let kept: Vec<Entry> = before
                    .iter()
                    .filter_map(|m| {
                        by_path
                            .get(m.path.as_str())
                            .filter(|e| packed(e).is_some() && e.hash == m.hash && e.kind == m.kind)
                            .map(|e| (*e).clone())
                    })
                    .collect();
                dropped += before.len() - kept.len();
                (describe(&entries, &kept, opts), kept)
            }
        };
        if now.is_empty() {
            return Ok(Outcome::Nothing(stats));
        }
        if opts.dry_run {
            return Ok(Outcome::Planned(stats));
        }
        read_blobs(repo, &now, &mut blobs).await?;
        let (delta, uploads) = build(&now, &blobs, opts.target_bytes)?;
        if let Some(hook) = hook {
            hook(attempt);
        }
        let mut meta: BTreeMap<String, String> = tip_commit
            .meta
            .iter()
            .filter(|(k, _)| k.starts_with("git."))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        meta.insert(MAINTENANCE_META.to_string(), REPACK.to_string());
        let req = CommitRequest {
            branch: opts.branch.clone(),
            base: Some(tip),
            delta,
            author: who.author.clone(),
            author_date: who.date.clone(),
            committer: who.committer.clone(),
            committer_date: who.date.clone(),
            message: format!(
                "pail maintenance: repack {} files into {} packs\n",
                stats.moved_files, stats.new_packs
            ),
            meta,
        };
        match repo
            .commit_opts(
                req,
                CommitOptions {
                    uploads,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(out) => {
                return Ok(Outcome::Done {
                    stats,
                    commit: out.commit,
                    parent: tip,
                    attempts: attempt,
                    dropped,
                });
            }
            Err(e) if e.downcast_ref::<ConflictError>().is_some() => moves = Some(now),
            Err(e) => return Err(e),
        }
    }
    bail!(
        "branch {} kept changing the files being moved; gave up after {} attempts",
        opts.branch,
        opts.attempts
    )
}
