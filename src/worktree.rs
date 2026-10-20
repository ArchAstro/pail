//! A checked-out working copy: `.pail/` state, the Git-style stage, and the stat cache.
//! Its repository (store, caches, indexes; see `local`) is `.pail/` itself, or for a linked
//! working copy the one `.pail/repo` names.
//!
//! ```text
//! .pail/HEAD.json       {"branch": "...", "base": "<commit>"}
//! .pail/stage.json      staged delta only: path → put|del (size O(changes), not O(repo))
//! .pail/pending/<hash>  staged file contents, snapshotted at `add` time
//! .pail/statcache.json  path → (mtime, size, inode, hash) — a cache, safe to delete
//! .pail/repo            linked working copies: path of the shared repository
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::blobs::{Blobs, CloneStats, clone_path};
use crate::local::LocalRepo;
use crate::hash::Hash;
use crate::manifest::{Change, Entry, Kind, Loc};
use crate::objects::now_date;
use crate::repo::{CommitRequest, Delta, FileInput, Repo};

pub const DOT: &str = crate::local::DOT;

#[derive(Serialize, Deserialize, Clone)]
pub struct Head {
    pub branch: String,
    pub base: Option<Hash>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Staged {
    Put { kind: KindName, size: u64, hash: Hash },
    Del,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum KindName {
    File,
    Exec,
    Symlink,
}

impl From<Kind> for KindName {
    fn from(k: Kind) -> Self {
        match k {
            Kind::File => KindName::File,
            Kind::Exec => KindName::Exec,
            Kind::Symlink => KindName::Symlink,
        }
    }
}

impl From<KindName> for Kind {
    fn from(k: KindName) -> Self {
        match k {
            KindName::File => Kind::File,
            KindName::Exec => Kind::Exec,
            KindName::Symlink => Kind::Symlink,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct StatEntry {
    mtime: i64,
    mtime_ns: i64,
    size: u64,
    ino: u64,
    kind: KindName,
    hash: Hash,
}

/// What a path looks like on disk.
#[derive(Clone, Debug, PartialEq)]
pub struct DiskFile {
    pub kind: Kind,
    pub size: u64,
    pub hash: Hash,
}

/// The view of one path across base, stage and disk.
#[derive(Default, Debug)]
pub struct Status {
    pub staged: Vec<(String, char)>,
    pub unstaged: Vec<(String, char)>,
    pub untracked: Vec<String>,
}

impl Status {
    pub fn is_clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty()
    }
}

pub struct Worktree {
    pub root: PathBuf,
    pub repo: Repo,
    /// The repository this working copy belongs to.
    pub local: LocalRepo,
    pub head: Head,
    pub stage: BTreeMap<String, Staged>,
}

fn dot(root: &Path) -> PathBuf {
    root.join(DOT)
}

fn read_json<T: for<'a> Deserialize<'a>>(p: &Path) -> Result<T> {
    let b = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
    Ok(serde_json::from_slice(&b)?)
}

fn write_json<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(&tmp, p)?;
    Ok(())
}

/// Content and kind of a path on disk (symlinks yield their target).
pub fn read_disk(path: &Path) -> Result<Option<(Kind, Vec<u8>)>> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if md.file_type().is_symlink() {
        let t = std::fs::read_link(path)?;
        return Ok(Some((Kind::Symlink, t.as_os_str().as_encoded_bytes().to_vec())));
    }
    if !md.is_file() {
        return Ok(None);
    }
    let kind = if md.permissions().mode() & 0o111 != 0 { Kind::Exec } else { Kind::File };
    Ok(Some((kind, std::fs::read(path)?)))
}

/// Make way for a new file at `path`: create parent directories (removing files that sit where
/// a directory must go) and remove whatever is at the path itself.
fn prepare_path(root: &Path, path: &str) -> Result<PathBuf> {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        let mut p = parent.to_path_buf();
        let mut blockers = vec![];
        while p.starts_with(root) && p != root {
            if std::fs::symlink_metadata(&p).map(|m| !m.is_dir()).unwrap_or(false) {
                blockers.push(p.clone());
            }
            p.pop();
        }
        for b in blockers {
            std::fs::remove_file(&b)?;
        }
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(md) = std::fs::symlink_metadata(&full) {
        if md.is_dir() {
            std::fs::remove_dir_all(&full)?;
        } else {
            std::fs::remove_file(&full)?;
        }
    }
    Ok(full)
}

fn set_mode(full: &Path, kind: Kind) -> Result<()> {
    let mode = if kind == Kind::Exec { 0o755 } else { 0o644 };
    Ok(std::fs::set_permissions(full, std::fs::Permissions::from_mode(mode))?)
}

/// Set a file's mtime (and atime) to `t`, or to now.
fn set_mtime(full: &Path, t: Option<std::time::SystemTime>) -> Result<()> {
    let c = std::ffi::CString::new(full.as_os_str().as_encoded_bytes())?;
    let ts = t.map(|t| {
        let d = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let s = libc::timespec { tv_sec: d.as_secs() as _, tv_nsec: d.subsec_nanos() as _ };
        [s, s]
    });
    let ptr = ts.as_ref().map_or(std::ptr::null(), |a| a.as_ptr());
    if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ptr, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("setting mtime of {}", full.display()));
    }
    Ok(())
}

/// Write one file (or symlink) into a directory tree, replacing whatever is there.
pub fn write_disk(root: &Path, path: &str, kind: Kind, data: &[u8]) -> Result<()> {
    let full = prepare_path(root, path)?;
    match kind {
        Kind::Symlink => {
            let target = std::str::from_utf8(data).context("symlink target is not UTF-8")?;
            std::os::unix::fs::symlink(target, &full)?;
        }
        Kind::File | Kind::Exec => {
            std::fs::write(&full, data)?;
            set_mode(&full, kind)?;
        }
    }
    Ok(())
}

/// Remove a file and any directories it leaves empty.
pub fn remove_disk(root: &Path, path: &str) -> Result<()> {
    let full = root.join(path);
    match std::fs::remove_file(&full) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    let mut p = full.parent().map(Path::to_path_buf);
    while let Some(dir) = p {
        if dir == root || std::fs::remove_dir(&dir).is_err() {
            break;
        }
        p = dir.parent().map(Path::to_path_buf);
    }
    Ok(())
}

/// How to put file contents on disk.
#[derive(Default)]
pub struct Checkout {
    /// Clone from this machine-local store instead of writing bytes (None: write directly).
    pub blobs: Option<Blobs>,
    /// A sibling directory whose identical files lend their mtimes, so build tools that compare
    /// source and artifact mtimes (cargo, make) see seeded build output as up to date.
    pub donor: Option<PathBuf>,
    pub stats: CloneStats,
}

impl Checkout {
    /// Clone from the repository's content store.
    pub fn for_repo(repo: &Repo) -> Checkout {
        Checkout { blobs: repo.blobs.clone(), ..Default::default() }
    }

    /// The donor's mtime for `path` if the donor holds the same content.
    fn donor_mtime(&self, e: &Entry) -> Option<std::time::SystemTime> {
        let full = self.donor.as_ref()?.join(&e.path);
        let md = std::fs::symlink_metadata(&full).ok()?;
        if !md.is_file() || md.len() != e.size {
            return None;
        }
        (Hash::of(&std::fs::read(&full).ok()?) == e.hash).then(|| md.modified().ok()).flatten()
    }

    /// Write the given entries' contents to disk, fetching what the store lacks in
    /// memory-bounded batches.
    pub async fn materialize(&self, repo: &Repo, root: &Path, entries: &[Entry]) -> Result<()> {
        let Some(blobs) = &self.blobs else {
            return self.write_direct(repo, root, entries).await;
        };
        // Symlinks and inline files carry their bytes in the manifest; nothing to share.
        let (direct, shared): (Vec<Entry>, Vec<Entry>) =
            entries.iter().cloned().partition(|e| e.kind == Kind::Symlink || matches!(e.loc, Loc::Inline(_)));
        self.write_direct(repo, root, &direct).await?;
        let mut seen = HashSet::new();
        let missing: Vec<Entry> =
            shared.iter().filter(|e| seen.insert(e.hash) && !blobs.has(&e.hash)).cloned().collect();
        for batch in batches(&missing) {
            for (e, d) in batch.iter().zip(repo.read_files(batch).await?) {
                blobs.put(&e.hash, &d)?;
            }
        }
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
        let per = shared.len().div_ceil(threads).max(1);
        std::thread::scope(|s| {
            let jobs: Vec<_> = shared
                .chunks(per)
                .map(|chunk| {
                    s.spawn(move || -> Result<()> {
                        for e in chunk {
                            let full = prepare_path(root, &e.path)?;
                            blobs.checkout(&e.hash, &full, &self.stats)?;
                            set_mode(&full, e.kind)?;
                            set_mtime(&full, self.donor_mtime(e))?;
                        }
                        Ok(())
                    })
                })
                .collect();
            jobs.into_iter().try_for_each(|j| j.join().expect("checkout thread panicked"))
        })
    }

    async fn write_direct(&self, repo: &Repo, root: &Path, entries: &[Entry]) -> Result<()> {
        for batch in batches(entries) {
            for (e, d) in batch.iter().zip(repo.read_files(batch).await?) {
                write_disk(root, &e.path, e.kind, &d)?;
                if e.kind != Kind::Symlink {
                    if let Some(t) = self.donor_mtime(e) {
                        set_mtime(&root.join(&e.path), Some(t))?;
                    }
                }
                self.stats.add(false, e.size);
            }
        }
        Ok(())
    }
}

/// Split entries into runs of about 256MB of content.
fn batches(entries: &[Entry]) -> Vec<&[Entry]> {
    let mut out = vec![];
    let mut start = 0;
    while start < entries.len() {
        let mut end = start;
        let mut bytes = 0u64;
        while end < entries.len() && (end == start || bytes < 256 << 20) {
            bytes += entries[end].size;
            end += 1;
        }
        out.push(&entries[start..end]);
        start = end;
    }
    out
}

/// What `seed_ignored` brought over.
#[derive(Default, Debug)]
pub struct Seeded {
    pub paths: Vec<String>,
    pub cloned: usize,
    pub copied: usize,
}

/// What `seed_ignored` clones when the donor has no `.pailseed`: build output, dependency
/// directories and local env files. Other ignored paths (scratch space, nested checkouts, logs)
/// are left behind: they can hold millions of files, and each cloned file still costs an inode.
pub const DEFAULT_SEED: &str = "target/\nnode_modules/\n_build/\ndeps/\n.next/\n.turbo/\ndist/\nbuild/\n.venv/\n__pycache__/\n.gradle/\n.env\n.env.*\n";

/// Seed rules (gitignore syntax): the donor's `.pailseed`, else `DEFAULT_SEED`.
fn seed_rules(donor: &Path) -> Result<ignore::gitignore::Gitignore> {
    let text = std::fs::read_to_string(donor.join(".pailseed")).unwrap_or_else(|_| DEFAULT_SEED.to_string());
    let mut b = ignore::gitignore::GitignoreBuilder::new(donor);
    for line in text.lines() {
        b.add_line(None, line)?;
    }
    Ok(b.build()?)
}

/// Clone the ignored paths of `donor` that match the seed rules into `dst`, skipping paths that
/// already exist. Directories are cloned whole, so a new working copy starts with a warm build
/// that shares the donor's blocks until either side rebuilds.
pub fn seed_ignored(donor: &Path, dst: &Path) -> Result<Seeded> {
    let rules = seed_rules(donor)?;
    let kept: HashSet<PathBuf> = ignore_walker(donor)
        .flatten()
        .filter_map(|e| e.path().strip_prefix(donor).ok().map(Path::to_path_buf))
        .collect();
    let mut out = Seeded::default();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for ent in std::fs::read_dir(donor.join(&rel))? {
            let ent = ent?;
            let name = ent.file_name();
            if name == DOT || name == ".git" {
                continue;
            }
            let child = rel.join(&name);
            if kept.contains(&child) {
                if ent.file_type()?.is_dir() {
                    stack.push(child);
                }
                continue;
            }
            let is_dir = ent.file_type()?.is_dir();
            if !rules.matched(&child, is_dir).is_ignore() {
                continue;
            }
            let to = dst.join(&child);
            if std::fs::symlink_metadata(&to).is_ok() {
                continue;
            }
            std::fs::create_dir_all(to.parent().unwrap())?;
            if clone_path(&ent.path(), &to)? {
                out.cloned += 1;
            } else {
                copy_tree(&ent.path(), &to)?;
                out.copied += 1;
            }
            out.paths.push(child.to_string_lossy().into_owned());
        }
    }
    out.paths.sort();
    Ok(out)
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(src)?;
    if md.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
    } else if md.is_dir() {
        std::fs::create_dir(dst)?;
        for ent in std::fs::read_dir(src)? {
            let ent = ent?;
            copy_tree(&ent.path(), &dst.join(ent.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst)?;
        let _ = std::fs::File::options().write(true).open(dst).and_then(|f| f.set_modified(md.modified()?));
    }
    Ok(())
}

/// Walks the non-ignored files of a tree, with pail's ignore rules.
fn ignore_walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .add_custom_ignore_filename(".pailignore")
        .filter_entry(|e| e.file_name() != DOT && e.file_name() != ".git")
        .build()
}

pub fn find_root(start: &Path) -> Result<PathBuf> {
    let mut p = start.to_path_buf();
    loop {
        if dot(&p).join("HEAD.json").exists() {
            return Ok(p);
        }
        if !p.pop() {
            bail!("not inside a pail working copy (no {DOT}/ found)");
        }
    }
}

pub fn author() -> String {
    if let Ok(a) = std::env::var("PAIL_AUTHOR") {
        return a;
    }
    let git = |k: &str| {
        std::process::Command::new("git")
            .args(["config", "--get", k])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    match (git("user.name"), git("user.email")) {
        (Some(n), Some(e)) => format!("{n} <{e}>"),
        _ => format!("{} <unknown>", std::env::var("USER").unwrap_or_else(|_| "unknown".into())),
    }
}

impl Worktree {
    pub fn open(start: &Path) -> Result<Worktree> {
        let root = find_root(start)?;
        let local = LocalRepo::for_worktree(&dot(&root))?;
        let head: Head = read_json(&dot(&root).join("HEAD.json"))?;
        let stage_path = dot(&root).join("stage.json");
        let stage = if stage_path.exists() { read_json(&stage_path)? } else { BTreeMap::new() };
        Ok(Worktree { root, repo: local.repo()?, local, head, stage })
    }

    /// Check out `branch` (at its head, or at commit `at`) into a new directory. With
    /// `shared`, the working copy attaches to that repository (a linked worktree); otherwise
    /// its `.pail/` becomes a new repository for `store`.
    pub async fn clone(
        store: &str,
        shared: Option<&LocalRepo>,
        dir: &Path,
        branch: &str,
        at: Option<Hash>,
        co: &mut Checkout,
    ) -> Result<Worktree> {
        if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
            bail!("{} exists and is not empty", dir.display());
        }
        std::fs::create_dir_all(dot(dir).join("pending"))?;
        let root = std::fs::canonicalize(dir)?;
        let local = match shared {
            Some(r) => {
                std::fs::write(dot(&root).join("repo"), r.dir.to_string_lossy().as_bytes())?;
                r.clone()
            }
            None => LocalRepo::create(&dot(&root), store, false)?,
        };
        let repo = local.repo()?;
        let commit = match at {
            Some(h) => h,
            None => repo.read_ref(branch).await?.with_context(|| format!("branch {branch} not found"))?.0,
        };
        if co.blobs.is_none() {
            co.blobs = repo.blobs.clone();
        }
        let c = repo.commit_obj(&commit).await?;
        let entries = repo.entries(c.root).await?;
        co.materialize(&repo, &root, &entries).await?;
        let head = Head { branch: branch.to_string(), base: Some(commit) };
        write_json(&dot(&root).join("HEAD.json"), &head)?;
        local.attach(&root)?;
        let wt = Worktree { root, repo, local, head, stage: BTreeMap::new() };
        let mut cache = HashMap::new();
        for e in &entries {
            wt.cache_stat(&mut cache, &e.path, e.kind, e.hash);
        }
        wt.save_statcache(&cache)?;
        Ok(wt)
    }

    fn save_head(&self) -> Result<()> {
        write_json(&dot(&self.root).join("HEAD.json"), &self.head)
    }

    fn save_stage(&self) -> Result<()> {
        write_json(&dot(&self.root).join("stage.json"), &self.stage)
    }

    fn load_statcache(&self) -> HashMap<String, StatEntry> {
        read_json(&dot(&self.root).join("statcache.json")).unwrap_or_default()
    }

    fn save_statcache(&self, c: &HashMap<String, StatEntry>) -> Result<()> {
        write_json(&dot(&self.root).join("statcache.json"), c)
    }

    fn cache_stat(&self, cache: &mut HashMap<String, StatEntry>, path: &str, kind: Kind, hash: Hash) {
        if let Ok(md) = std::fs::symlink_metadata(self.root.join(path)) {
            cache.insert(
                path.to_string(),
                StatEntry { mtime: md.mtime(), mtime_ns: md.mtime_nsec(), size: md.size(), ino: md.ino(), kind: kind.into(), hash },
            );
        }
    }

    pub async fn base_entries(&self) -> Result<BTreeMap<String, Entry>> {
        let root = self.repo.root_of(self.head.base).await?;
        Ok(self.repo.entries(root).await?.into_iter().map(|e| (e.path.clone(), e)).collect())
    }

    /// All non-ignored paths on disk, plus tracked paths even if an ignore rule matches them.
    fn disk_paths(&self, tracked: impl Iterator<Item = String>) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for ent in ignore_walker(&self.root).flatten() {
            let ft = match ent.file_type() {
                Some(f) => f,
                None => continue,
            };
            if ft.is_dir() {
                continue;
            }
            if let Ok(rel) = ent.path().strip_prefix(&self.root) {
                if let Some(s) = rel.to_str() {
                    out.insert(s.to_string());
                }
            }
        }
        for p in tracked {
            if !out.contains(&p) && std::fs::symlink_metadata(self.root.join(&p)).is_ok() {
                out.insert(p);
            }
        }
        out
    }

    /// Hash everything on disk, reusing the stat cache for unchanged files.
    pub fn scan(&self, tracked: impl Iterator<Item = String>) -> Result<BTreeMap<String, DiskFile>> {
        let mut cache = self.load_statcache();
        let cache_written = std::fs::metadata(dot(&self.root).join("statcache.json"))
            .map(|m| m.mtime())
            .unwrap_or(0);
        let mut out = BTreeMap::new();
        let mut dirty = false;
        for p in self.disk_paths(tracked) {
            let full = self.root.join(&p);
            let Ok(md) = std::fs::symlink_metadata(&full) else { continue };
            if let Some(c) = cache.get(&p) {
                // Files modified in the same second the cache was written are re-hashed (racy mtime).
                if c.mtime == md.mtime()
                    && c.mtime_ns == md.mtime_nsec()
                    && c.size == md.size()
                    && c.ino == md.ino()
                    && md.mtime() < cache_written
                {
                    let kind_now = if md.file_type().is_symlink() {
                        Kind::Symlink
                    } else if md.permissions().mode() & 0o111 != 0 {
                        Kind::Exec
                    } else {
                        Kind::File
                    };
                    if Kind::from(c.kind) == kind_now {
                        out.insert(p, DiskFile { kind: kind_now, size: c.size, hash: c.hash });
                        continue;
                    }
                }
            }
            let Some((kind, data)) = read_disk(&full)? else { continue };
            let hash = Hash::of(&data);
            out.insert(p.clone(), DiskFile { kind, size: data.len() as u64, hash });
            self.cache_stat(&mut cache, &p, kind, hash);
            dirty = true;
        }
        if dirty {
            cache.retain(|k, _| out.contains_key(k));
            self.save_statcache(&cache)?;
        }
        Ok(out)
    }

    /// Base with the stage applied: what `commit` would produce.
    fn index_view(&self, base: &BTreeMap<String, Entry>) -> BTreeMap<String, DiskFile> {
        let mut view: BTreeMap<String, DiskFile> = base
            .iter()
            .map(|(k, e)| (k.clone(), DiskFile { kind: e.kind, size: e.size, hash: e.hash }))
            .collect();
        for (p, s) in &self.stage {
            match s {
                Staged::Put { kind, size, hash } => {
                    view.insert(p.clone(), DiskFile { kind: (*kind).into(), size: *size, hash: *hash });
                }
                Staged::Del => {
                    view.remove(p);
                }
            }
        }
        view
    }

    pub async fn status(&self) -> Result<Status> {
        let base = self.base_entries().await?;
        let index = self.index_view(&base);
        let disk = self.scan(index.keys().cloned())?;
        let mut st = Status::default();
        for (p, s) in &self.stage {
            let code = match (s, base.contains_key(p)) {
                (Staged::Del, _) => 'D',
                (Staged::Put { .. }, true) => 'M',
                (Staged::Put { .. }, false) => 'A',
            };
            st.staged.push((p.clone(), code));
        }
        for (p, f) in &index {
            match disk.get(p) {
                None => st.unstaged.push((p.clone(), 'D')),
                Some(d) if d != f => st.unstaged.push((p.clone(), 'M')),
                _ => {}
            }
        }
        st.untracked = disk.keys().filter(|p| !index.contains_key(*p)).cloned().collect();
        Ok(st)
    }

    /// Repo-relative paths selected by user arguments (files or directory prefixes).
    pub fn resolve(&self, cwd: &Path, args: &[String]) -> Result<Vec<String>> {
        let mut out = vec![];
        for a in args {
            let abs = if Path::new(a).is_absolute() { PathBuf::from(a) } else { cwd.join(a) };
            let abs = normalize(&abs);
            let rel = abs.strip_prefix(&self.root).with_context(|| format!("{a} is outside the working copy"))?;
            out.push(rel.to_string_lossy().trim_end_matches('/').to_string());
        }
        Ok(out)
    }

    fn matches(spec: &[String], path: &str) -> bool {
        spec.iter().any(|s| s.is_empty() || path == s || path.starts_with(&format!("{s}/")))
    }

    /// Stage the current disk state of the selected paths (`""` selects everything).
    /// With `tracked_only`, new untracked files are skipped (as `commit -a` does).
    pub async fn add(&mut self, spec: &[String], tracked_only: bool) -> Result<usize> {
        let base = self.base_entries().await?;
        let index = self.index_view(&base);
        let disk = self.scan(index.keys().cloned())?;
        let candidates: BTreeSet<String> =
            index.keys().chain(disk.keys()).filter(|p| Self::matches(spec, p)).cloned().collect();
        let pending = dot(&self.root).join("pending");
        std::fs::create_dir_all(&pending)?;
        let mut n = 0;
        for p in candidates {
            if tracked_only && !index.contains_key(&p) {
                continue;
            }
            let before = self.stage.get(&p).cloned();
            match disk.get(&p) {
                Some(d) => {
                    if base.get(&p).is_some_and(|e| e.kind == d.kind && e.hash == d.hash) {
                        self.stage.remove(&p);
                    } else {
                        if index.get(&p) != Some(d) || !pending.join(d.hash.hex()).exists() {
                            // Snapshot the bytes now: later edits are not part of this stage.
                            let (kind, data) = read_disk(&self.root.join(&p))?.context("file vanished")?;
                            let h = Hash::of(&data);
                            std::fs::write(pending.join(h.hex()), &data)?;
                            self.stage.insert(p.clone(), Staged::Put { kind: kind.into(), size: data.len() as u64, hash: h });
                        }
                    }
                }
                None => {
                    if base.contains_key(&p) {
                        self.stage.insert(p.clone(), Staged::Del);
                    } else {
                        self.stage.remove(&p);
                    }
                }
            }
            if self.stage.get(&p) != before.as_ref() {
                n += 1;
            }
        }
        self.save_stage()?;
        Ok(n)
    }

    pub fn unstage(&mut self, spec: &[String]) -> Result<usize> {
        let before = self.stage.len();
        self.stage.retain(|p, _| !Self::matches(spec, p));
        self.save_stage()?;
        Ok(before - self.stage.len())
    }

    pub async fn commit(&mut self, message: &str) -> Result<crate::repo::CommitOutcome> {
        if self.stage.is_empty() {
            bail!("nothing staged (use `pail add <path>` or `pail commit -a`)");
        }
        let base = self.base_entries().await?;
        let pending = dot(&self.root).join("pending");
        let mut inputs = vec![];
        let mut delta: Delta = BTreeMap::new();
        for (p, s) in &self.stage {
            match s {
                Staged::Put { kind, hash, .. } => {
                    let data = std::fs::read(pending.join(hash.hex()))
                        .with_context(|| format!("staged content for {p} is missing"))?;
                    inputs.push(FileInput { path: p.clone(), kind: (*kind).into(), data: Bytes::from(data) });
                }
                Staged::Del => {
                    delta.insert(p.clone(), None);
                }
            }
        }
        // Content already in the store (moves, reverts to the base version) is reused.
        let reuse = base.values().map(|e| (e.hash, e.loc.clone())).collect();
        for e in self.repo.write_files(&inputs, &reuse).await? {
            delta.insert(e.path.clone(), Some(e));
        }
        let who = author();
        let date = now_date();
        let out = self
            .repo
            .commit(CommitRequest {
                branch: self.head.branch.clone(),
                base: self.head.base,
                delta,
                author: who.clone(),
                author_date: date.clone(),
                committer: who,
                committer_date: date,
                message: message.to_string(),
                meta: BTreeMap::new(),
            })
            .await?;
        // The branch may have advanced under us (replayed commit). Bring in others' changes.
        let old_base = self.head.base;
        let landed_parent = self.repo.commit_obj(&out.commit).await?.parent;
        self.head.base = Some(out.commit);
        self.save_head()?;
        let staged: Vec<String> = self.stage.keys().cloned().collect();
        // The committed snapshots are now shared content: hand them to the blob store so other
        // working copies clone them instead of fetching.
        if let Some(blobs) = &self.repo.blobs {
            for s in self.stage.values() {
                if let Staged::Put { hash, .. } = s {
                    let f = pending.join(hash.hex());
                    if f.exists() {
                        blobs.adopt(hash, &f)?;
                    }
                }
            }
        }
        self.stage.clear();
        self.save_stage()?;
        let _ = std::fs::remove_dir_all(&pending);
        if landed_parent != old_base {
            let from = self.repo.root_of(old_base).await?;
            let to = self.repo.root_of(landed_parent).await?;
            let theirs: Vec<Change> = self.repo.diff(from, to).await?;
            let theirs: Vec<Change> = theirs.into_iter().filter(|c| !staged.contains(&c.path)).collect();
            self.apply_changes(&theirs).await?;
        }
        Ok(out)
    }

    /// Apply tree changes to disk and the stat cache.
    async fn apply_changes(&self, changes: &[Change]) -> Result<()> {
        let mut cache = self.load_statcache();
        let writes: Vec<Entry> = changes.iter().filter_map(|c| c.new.clone()).collect();
        for c in changes.iter().filter(|c| c.new.is_none()) {
            remove_disk(&self.root, &c.path)?;
            cache.remove(&c.path);
        }
        Checkout::for_repo(&self.repo).materialize(&self.repo, &self.root, &writes).await?;
        for e in &writes {
            self.cache_stat(&mut cache, &e.path, e.kind, e.hash);
        }
        self.save_statcache(&cache)
    }

    /// Switch the working copy to another branch. Requires a clean working copy.
    pub async fn switch(&mut self, branch: &str) -> Result<usize> {
        let st = self.status().await?;
        if !st.is_clean() {
            bail!("working copy has changes; commit them or `pail restore` first");
        }
        let (target, _) = self.repo.read_ref(branch).await?.with_context(|| format!("branch {branch} not found"))?;
        let from = self.repo.root_of(self.head.base).await?;
        let to = self.repo.root_of(Some(target)).await?;
        let changes = self.repo.diff(from, to).await?;
        self.apply_changes(&changes).await?;
        self.head = Head { branch: branch.to_string(), base: Some(target) };
        self.save_head()?;
        Ok(changes.len())
    }

    /// Fast-forward the base to the branch head, updating untouched files on disk.
    pub async fn sync(&mut self) -> Result<usize> {
        let (head, _) = self
            .repo
            .read_ref(&self.head.branch)
            .await?
            .with_context(|| format!("branch {} not found", self.head.branch))?;
        if Some(head) == self.head.base {
            return Ok(0);
        }
        let st = self.status().await?;
        let local: BTreeSet<&str> =
            st.staged.iter().chain(st.unstaged.iter()).map(|(p, _)| p.as_str()).collect();
        let from = self.repo.root_of(self.head.base).await?;
        let to = self.repo.root_of(Some(head)).await?;
        let changes = self.repo.diff(from, to).await?;
        let clash: Vec<&str> = changes.iter().map(|c| c.path.as_str()).filter(|p| local.contains(p)).collect();
        if !clash.is_empty() {
            bail!("sync would overwrite local changes in: {}", clash.join(", "));
        }
        self.apply_changes(&changes).await?;
        self.head.base = Some(head);
        self.save_head()?;
        Ok(changes.len())
    }

    /// Discard unstaged changes for the selected paths (restore from the stage or base).
    pub async fn restore(&self, spec: &[String]) -> Result<usize> {
        let base = self.base_entries().await?;
        let index = self.index_view(&base);
        let disk = self.scan(index.keys().cloned())?;
        let pending = dot(&self.root).join("pending");
        let mut fetch = vec![];
        let mut n = 0;
        for (p, f) in index.iter().filter(|(p, _)| Self::matches(spec, p)) {
            if disk.get(p) == Some(f) {
                continue;
            }
            n += 1;
            match self.stage.get(p) {
                Some(Staged::Put { kind, hash, .. }) => {
                    write_disk(&self.root, p, (*kind).into(), &std::fs::read(pending.join(hash.hex()))?)?
                }
                _ => fetch.push(base[p].clone()),
            }
        }
        for p in disk.keys().filter(|p| Self::matches(spec, p) && !index.contains_key(*p)) {
            if base.contains_key(p) {
                remove_disk(&self.root, p)?; // staged deletion
                n += 1;
            }
        }
        Checkout::for_repo(&self.repo).materialize(&self.repo, &self.root, &fetch).await?;
        Ok(n)
    }
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}
