//! Local commit graph: a derived, rebuildable index of commit metadata, like Git's
//! commit-graph. History walks, id lookups and abbreviation lengths read it instead of
//! opening, verifying and parsing one commit object per commit.
//!
//! ```text
//! In the local repository's graph/:
//! commits2.dat                append-only records, ancestors before descendants:
//!                             [u32 len][32 pail id][20 git id or zeros]
//!                             [{"c": commit without its change list, "p": changed paths}]
//! commits2.idx                header, then (pail id, offset) sorted by id,
//!                             then (git id, row) sorted by git id; memory-mapped
//! ```
//!
//! Invariant: a commit in the graph has all its ancestors in the graph, so filling it from a
//! tip stops at the first known commit. Everything is derived from immutable, hash-verified
//! commits, so it never goes stale; deleting the directory only costs a rebuild.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use fs2::FileExt;
use memmap2::Mmap;

use crate::hash::Hash;
use crate::objects::Commit;

const MAGIC: &[u8; 8] = b"PAILGRF2";
const DAT: &str = "commits2.dat";
const IDX: &str = "commits2.idx";
const HEADER: usize = 64;
const ENTRY: usize = 40;
const GIT_ENTRY: usize = 24;
const REC_PREFIX: usize = 4 + 32 + 20;

/// A read-only view of the graph files as of when it was opened.
pub struct GraphView {
    idx: Option<Mmap>,
    dat: Option<Mmap>,
    count: usize,
    git_count: usize,
    pub abbrev_pail: usize,
    pub abbrev_git: usize,
}

fn read_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// Shared leading hex digits of two ids.
fn common_nibbles(a: &[u8], b: &[u8]) -> usize {
    let mut n = 0;
    for (x, y) in a.iter().zip(b) {
        if x == y {
            n += 2;
            continue;
        }
        if x >> 4 == y >> 4 {
            n += 1;
        }
        break;
    }
    n
}

fn map(p: &Path) -> Option<Mmap> {
    let f = std::fs::File::open(p).ok()?;
    if f.metadata().ok()?.len() == 0 {
        return None;
    }
    // Safety: the files are only ever appended to (dat) or replaced by rename (idx).
    unsafe { Mmap::map(&f).ok() }
}

impl GraphView {
    fn open(dir: &Path) -> GraphView {
        let idx = map(&dir.join(IDX)).filter(|m| m.len() >= HEADER && &m[..8] == MAGIC);
        let dat = map(&dir.join(DAT));
        let (count, git_count, ap, ag) = match &idx {
            Some(m) => (
                read_u64(m, 8) as usize,
                read_u64(m, 16) as usize,
                m[32] as usize,
                m[33] as usize,
            ),
            None => (0, 0, 7, 7),
        };
        GraphView {
            idx,
            dat,
            count,
            git_count,
            abbrev_pail: ap.max(7),
            abbrev_git: ag.max(7),
        }
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn entry(&self, i: usize) -> (&[u8], u64) {
        let m = self.idx.as_ref().unwrap();
        let at = HEADER + i * ENTRY;
        (&m[at..at + 32], read_u64(m, at + 32))
    }

    fn git_entry(&self, i: usize) -> (&[u8], u32) {
        let m = self.idx.as_ref().unwrap();
        let at = HEADER + self.count * ENTRY + i * GIT_ENTRY;
        (&m[at..at + 20], read_u32(m, at + 20))
    }

    fn find(&self, h: &Hash) -> Option<usize> {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.entry(mid).0.cmp(&h.0[..]) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    pub fn contains(&self, h: &Hash) -> bool {
        self.find(h).is_some()
    }

    fn record(&self, h: &Hash) -> Option<&[u8]> {
        let (_, off) = self.entry(self.find(h)?);
        let dat = self.dat.as_ref()?;
        let off = off as usize;
        let len = read_u32(dat, off) as usize;
        dat.get(off + REC_PREFIX..off + 4 + len)
    }

    /// A commit's metadata (no change list).
    pub fn get(&self, h: &Hash) -> Option<Commit> {
        #[derive(serde::Deserialize)]
        struct Meta {
            c: Commit,
        }
        serde_json::from_slice::<Meta>(self.record(h)?)
            .ok()
            .map(|m| m.c)
    }

    /// Paths the commit changed, when it records a change list.
    pub fn changed_paths(&self, h: &Hash) -> Option<Vec<String>> {
        #[derive(serde::Deserialize)]
        struct Paths<'a> {
            #[serde(borrow)]
            p: Option<std::borrow::Cow<'a, str>>,
        }
        let r: Paths = serde_json::from_slice(self.record(h)?).ok()?;
        r.p.map(|p| {
            if p.is_empty() {
                vec![]
            } else {
                p.split('\n').map(str::to_string).collect()
            }
        })
    }

    /// Pail ids starting with a hex prefix.
    pub fn pail_prefix(&self, prefix: &str) -> Vec<Hash> {
        let lo = self.lower_bound(prefix, self.count, |i| self.entry(i).0);
        (lo..self.count)
            .map(|i| self.entry(i).0)
            .take_while(|id| hex::encode(id).starts_with(prefix))
            .map(|id| Hash(id.try_into().unwrap()))
            .collect()
    }

    /// Pail ids of commits whose Git id starts with a hex prefix.
    pub fn git_prefix(&self, prefix: &str) -> Vec<Hash> {
        let lo = self.lower_bound(prefix, self.git_count, |i| self.git_entry(i).0);
        (lo..self.git_count)
            .map(|i| self.git_entry(i))
            .take_while(|(g, _)| hex::encode(g).starts_with(prefix))
            .map(|(_, row)| Hash(self.entry(row as usize).0.try_into().unwrap()))
            .collect()
    }

    fn lower_bound<'s>(&'s self, prefix: &str, n: usize, key: impl Fn(usize) -> &'s [u8]) -> usize {
        let (mut lo, mut hi) = (0, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if hex::encode(key(mid)).as_str() < prefix {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

pub struct Graph {
    dir: PathBuf,
    view: std::sync::RwLock<Arc<GraphView>>,
}

impl Graph {
    /// The graph in a local repository's `graph/` (one per repository: Git-id lookups and
    /// abbreviation lengths are per repository).
    pub fn open(dir: &Path) -> Result<Graph> {
        let dir = dir.to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let view = GraphView::open(&dir);
        Ok(Graph {
            dir,
            view: std::sync::RwLock::new(Arc::new(view)),
        })
    }

    pub fn view(&self) -> Arc<GraphView> {
        self.view.read().unwrap().clone()
    }

    /// Add `commits` (each with all its ancestors already in the graph or earlier in the
    /// list, oldest first) and rebuild the index.
    pub fn append(&self, commits: &[(Hash, Arc<Commit>)]) -> Result<()> {
        let lock = std::fs::File::create(self.dir.join("lock"))?;
        lock.lock_exclusive()?;
        let current = GraphView::open(&self.dir);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(DAT))?;
        let mut buf = vec![];
        for (h, c) in commits {
            if current.contains(h) {
                continue;
            }
            let slim = Commit {
                changes: None,
                ..(**c).clone()
            };
            let paths = c
                .changes
                .as_ref()
                .and_then(|cs| cs.parse().ok())
                .map(|m| m.into_keys().collect::<Vec<_>>().join("\n"));
            let json = serde_json::to_vec(&serde_json::json!({ "c": slim, "p": paths }))?;
            let git: [u8; 20] = c
                .meta
                .get("git.commit")
                .and_then(|g| hex::decode(g).ok())
                .and_then(|v| v.try_into().ok())
                .unwrap_or([0; 20]);
            buf.extend_from_slice(&((32 + 20 + json.len()) as u32).to_le_bytes());
            buf.extend_from_slice(&h.0);
            buf.extend_from_slice(&git);
            buf.extend_from_slice(&json);
        }
        f.write_all(&buf)?;
        f.sync_data().ok();
        drop(f);
        self.rebuild_locked()?;
        *self.view.write().unwrap() = Arc::new(GraphView::open(&self.dir));
        Ok(())
    }

    /// Rewrite the index from the record prefixes (no JSON parsing). A torn record at the
    /// end (a writer that died mid-append) is cut off.
    fn rebuild_locked(&self) -> Result<()> {
        let dat_path = self.dir.join(DAT);
        let dat = std::fs::read(&dat_path).context("reading commit graph")?;
        let mut rows: Vec<([u8; 32], u64, [u8; 20])> = vec![];
        let mut off = 0usize;
        while off + 4 <= dat.len() {
            let len = read_u32(&dat, off) as usize;
            if len < 52 || off + 4 + len > dat.len() {
                break;
            }
            let h: [u8; 32] = dat[off + 4..off + 36].try_into().unwrap();
            let g: [u8; 20] = dat[off + 36..off + 56].try_into().unwrap();
            rows.push((h, off as u64, g));
            off += 4 + len;
        }
        if off < dat.len() {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&dat_path)?
                .set_len(off as u64)?;
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows.dedup_by(|a, b| a.0 == b.0);
        let mut gits: Vec<([u8; 20], u32)> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.2 != [0; 20])
            .map(|(i, r)| (r.2, i as u32))
            .collect();
        gits.sort();
        let abbrev = |ids: Vec<&[u8]>| {
            ids.windows(2)
                .map(|w| common_nibbles(w[0], w[1]) + 1)
                .max()
                .unwrap_or(0)
                .max(7)
        };
        let ap = abbrev(rows.iter().map(|r| &r.0[..]).collect());
        let ag = abbrev(gits.iter().map(|g| &g.0[..]).collect());
        let mut out = Vec::with_capacity(HEADER + rows.len() * ENTRY + gits.len() * GIT_ENTRY);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(rows.len() as u64).to_le_bytes());
        out.extend_from_slice(&(gits.len() as u64).to_le_bytes());
        out.extend_from_slice(&(off as u64).to_le_bytes());
        out.push(ap.min(64) as u8);
        out.push(ag.min(40) as u8);
        out.resize(HEADER, 0);
        for (h, o, _) in &rows {
            out.extend_from_slice(h);
            out.extend_from_slice(&o.to_le_bytes());
        }
        for (g, row) in &gits {
            out.extend_from_slice(g);
            out.extend_from_slice(&row.to_le_bytes());
        }
        let tmp = self.dir.join(format!("{IDX}.tmp{}", std::process::id()));
        std::fs::write(&tmp, &out)?;
        std::fs::rename(&tmp, self.dir.join(IDX))?;
        Ok(())
    }
}
