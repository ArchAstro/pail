//! Repository operations over a store: reading and writing trees, files and commits,
//! and the compare-and-swap commit protocol.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};
use std::future::Future;

use crate::hash::Hash;
use crate::manifest::{
    BuiltPage, Change, Child, ChunkRef, Entry, Kind, Loc, Page, build_leaves, build_page, build_upper,
    decode_stored_page, diff_sorted, leaf_child, split_runs,
};
use crate::objects::{Commit, RefValue, commit_key, page_key, ref_key, validate_branch};
use crate::pack::{CHUNK_THRESHOLD, INLINE_MAX, PACK_MAX, PackBuilder, chunk_key, chunk_spans, decode_blob, encode_blob, pack_key};
use crate::store::{Cas, Store, Version};

/// Requests in flight per operation.
pub const PARALLELISM: usize = 64;
/// How long a contended commit keeps retrying before giving up. Time-based rather than
/// attempt-based because stores differ: GCS allows ~1 write/s to one object, S3 far more.
pub const COMMIT_DEADLINE: Duration = Duration::from_secs(60);

/// A path-level edit: `Some(entry)` upserts, `None` deletes.
pub type Delta = BTreeMap<String, Option<Entry>>;

/// File contents to store.
pub struct FileInput {
    pub path: String,
    pub kind: Kind,
    pub data: Bytes,
}

pub struct NewTree {
    pub root: Hash,
    pub pages: Vec<BuiltPage>,
    pub files: u64,
    pub bytes: u64,
}

/// Raised when a concurrent commit touched the same paths.
#[derive(Debug, thiserror::Error)]
#[error("conflict: {} path(s) changed on {branch} since your base: {}", paths.len(), preview(paths))]
pub struct ConflictError {
    pub branch: String,
    pub paths: Vec<String>,
}

fn preview(paths: &[String]) -> String {
    let mut s = paths.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > 5 {
        s.push_str(", …");
    }
    s
}

pub struct CommitRequest {
    pub branch: String,
    /// Commit the delta was computed against (`None` = empty tree / new branch).
    pub base: Option<Hash>,
    pub delta: Delta,
    pub author: String,
    pub author_date: String,
    pub committer: String,
    pub committer_date: String,
    pub message: String,
    pub meta: BTreeMap<String, String>,
}

/// The descriptive fields of a commit.
#[derive(Debug, Clone)]
pub struct CommitFields {
    pub author: String,
    pub author_date: String,
    pub committer: String,
    pub committer_date: String,
    pub message: String,
    pub meta: BTreeMap<String, String>,
}

/// A commit stored by `write_commit` (not yet on any branch).
#[derive(Debug, Clone)]
pub struct Written {
    pub commit: Hash,
    pub pages: usize,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct LandOutcome {
    /// `onto`'s new tip.
    pub tip: Hash,
    /// The commits now on `onto`, oldest first.
    pub landed: Vec<Hash>,
    /// The stack's own commits were reused (nothing rewritten).
    pub fast_forward: bool,
    pub attempts: u32,
}

#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub commit: Hash,
    pub version: Version,
    pub attempts: u32,
    pub pages_written: usize,
    pub files: u64,
    pub bytes: u64,
    /// The commit the publish check approved, when the published one is a replay of it.
    pub replayed_from: Option<Hash>,
}

/// A ref move about to be published: `branch` from `old` to `new`, all objects written.
#[derive(Debug, Clone)]
pub struct Publish {
    pub branch: String,
    pub old: Option<Hash>,
    pub new: Hash,
}

/// Called once with the first fully written commit (or landed stack) before the CAS that
/// would publish it; an error aborts with nothing published. A lost race replays the same
/// change on the new head without asking again: the replay only adds others' disjoint
/// changes, and re-running a check (often a test suite) per lost race would starve writers.
pub type PublishCheck<'a> = &'a (dyn Fn(&Publish) -> Result<()> + Sync);

pub struct Repo {
    pub store: Store,
    /// The local repository directory holding this client's caches and indexes (see
    /// `local`). None: a pure remote client that keeps nothing on disk.
    pub local: Option<PathBuf>,
    /// Local content cache (read-through): file contents by content hash, shared with
    /// working copies. None reads every file from the store.
    pub blobs: Option<crate::blobs::Blobs>,
    /// Git blob ids by content hash, and Git root tree ids by manifest root.
    pub git_blobs: crate::gitobj::IdIndex,
    pub git_trees: crate::gitobj::IdIndex,
    pages: Mutex<HashMap<Hash, Arc<Page>>>,
    commits: Mutex<HashMap<Hash, Arc<Commit>>>,
    /// Commit metadata (no change lists) served by the local commit graph.
    metas: Mutex<HashMap<Hash, Arc<Commit>>>,
    graph: std::sync::OnceLock<Option<crate::graph::Graph>>,
}

impl Repo {
    /// A client that talks to the store only and keeps nothing locally.
    pub fn remote(url: &str) -> Result<Repo> {
        Ok(Repo::with_store(Store::open(url)?, None))
    }

    /// A client whose caches and indexes live in the local repository directory `dir`.
    pub fn with_local(store: Store, dir: &Path) -> Repo {
        let mut r = Repo::with_store(store, Some(dir.to_path_buf()));
        r.blobs = Some(crate::blobs::Blobs { dir: dir.join("blobs") });
        r
    }

    pub fn with_store(store: Store, local: Option<PathBuf>) -> Repo {
        let index = |name: &str| crate::gitobj::IdIndex::new(local.as_ref().map(|d| d.join("index").join(name)));
        Repo {
            store,
            git_blobs: index("gitblob.idx"),
            git_trees: index("gittree.idx"),
            local,
            blobs: None,
            pages: Mutex::default(),
            commits: Mutex::default(),
            metas: Mutex::default(),
            graph: std::sync::OnceLock::new(),
        }
    }

    fn cache_path(&self, kind: &str, h: &Hash) -> Option<PathBuf> {
        let hex = h.hex();
        self.local.as_ref().map(|d| d.join("objects").join(kind).join(&hex[..2]).join(&hex))
    }

    fn cache_read(&self, kind: &str, h: &Hash) -> Option<Vec<u8>> {
        std::fs::read(self.cache_path(kind, h)?).ok()
    }

    fn cache_write(&self, kind: &str, h: &Hash, data: &[u8]) {
        let Some(p) = self.cache_path(kind, h) else { return };
        if p.exists() {
            return;
        }
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = p.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, data).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }

    // ---------- refs ----------

    pub async fn read_ref(&self, branch: &str) -> Result<Option<(Hash, Version)>> {
        let Some((b, v)) = self.store.get_opt(&ref_key(branch)).await? else { return Ok(None) };
        let r: RefValue = serde_json::from_slice(&b).with_context(|| format!("corrupt ref {branch}"))?;
        Ok(Some((r.commit, v)))
    }

    pub async fn list_refs(&self) -> Result<Vec<(String, Hash)>> {
        let keys = self.store.list("refs/").await?;
        let futs = keys.into_iter().map(|(k, _)| async move {
            let name = k.trim_start_matches("refs/").to_string();
            let r = self.read_ref(&name).await?;
            Ok::<_, anyhow::Error>(r.map(|(h, _)| (name, h)))
        });
        let out = run_ordered(futs.collect(), PARALLELISM).await?;
        Ok(out.into_iter().flatten().collect())
    }

    /// Create a branch pointing at `commit`. Fails if it already exists.
    pub async fn create_branch(&self, name: &str, commit: Hash) -> Result<Version> {
        validate_branch(name)?;
        let body = Bytes::from(serde_json::to_vec(&RefValue { commit })?);
        match self.store.cas(&ref_key(name), body, None).await? {
            Cas::Ok(v) => Ok(v),
            Cas::Conflict => bail!("branch {name} already exists"),
        }
    }

    /// Move a branch to any commit (not necessarily a descendant). Records a reflog entry first.
    pub async fn force_ref(&self, name: &str, commit: Hash) -> Result<()> {
        let old = self.read_ref(name).await?;
        if let Some((old, _)) = &old {
            let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
            let rec = serde_json::json!({"old": old, "new": commit});
            self.store.put(&format!("reflog/{name}/{ts:015}"), Bytes::from(rec.to_string())).await?;
        }
        let body = Bytes::from(serde_json::to_vec(&RefValue { commit })?);
        match self.store.cas(&ref_key(name), body, old.as_ref().map(|(_, v)| v)).await? {
            Cas::Ok(_) => Ok(()),
            Cas::Conflict => bail!("branch {name} moved concurrently; retry"),
        }
    }

    pub async fn delete_branch(&self, name: &str) -> Result<()> {
        self.store.delete(&ref_key(name)).await
    }

    // ---------- commits ----------

    pub async fn commit_obj(&self, h: &Hash) -> Result<Arc<Commit>> {
        if let Some(c) = self.commits.lock().unwrap().get(h) {
            return Ok(c.clone());
        }
        let data = match self.cache_read("commits", h) {
            Some(d) => Bytes::from(d),
            None => {
                let d = self.store.get(&commit_key(h)).await?;
                self.cache_write("commits", h, &d);
                d
            }
        };
        let c = Arc::new(Commit::decode(h, &data)?);
        self.commits.lock().unwrap().insert(*h, c.clone());
        Ok(c)
    }

    /// The local commit graph (None when the metadata cache is off).
    pub fn graph(&self) -> Option<&crate::graph::Graph> {
        self.graph.get_or_init(|| self.local.as_ref().and_then(|d| crate::graph::Graph::open(&d.join("graph")).ok())).as_ref()
    }

    /// Make the graph hold `tips` and all their ancestors: walk each tip down to the first
    /// commit it already knows, then append oldest first.
    pub async fn ensure_graph(&self, tips: &[Hash]) -> Result<()> {
        let Some(g) = self.graph() else { return Ok(()) };
        let view = g.view();
        let mut seen = HashSet::new();
        let mut order: Vec<(Hash, Arc<Commit>)> = vec![];
        for tip in tips {
            let mut chain = vec![];
            let mut cur = Some(*tip);
            while let Some(h) = cur {
                if view.contains(&h) || !seen.insert(h) {
                    break;
                }
                let c = self.commit_obj(&h).await?;
                cur = c.parent;
                chain.push((h, c));
            }
            chain.reverse();
            order.extend(chain);
        }
        if !order.is_empty() {
            g.append(&order)?;
        }
        Ok(())
    }

    /// A commit's metadata without its change list: from memory, the commit graph, or the
    /// commit object. Use `commit_obj` when the change list is needed.
    pub async fn commit_meta(&self, h: &Hash) -> Result<Arc<Commit>> {
        if let Some(c) = self.commits.lock().unwrap().get(h).or(self.metas.lock().unwrap().get(h)) {
            return Ok(c.clone());
        }
        if let Some(c) = self.graph().and_then(|g| g.view().get(h)) {
            let c = Arc::new(c);
            self.metas.lock().unwrap().insert(*h, c.clone());
            return Ok(c);
        }
        self.commit_obj(h).await
    }

    /// Paths a commit changed, from the commit graph (None if it doesn't know them).
    pub fn changed_paths(&self, h: &Hash) -> Option<Vec<String>> {
        self.graph()?.view().changed_paths(h)
    }

    /// Up to `n` commits starting at `start`, newest first.
    pub async fn log(&self, start: Hash, n: usize) -> Result<Vec<(Hash, Arc<Commit>)>> {
        let mut out = vec![];
        let mut cur = Some(start);
        while let Some(h) = cur {
            if out.len() >= n {
                break;
            }
            let c = self.commit_obj(&h).await?;
            cur = c.parent;
            out.push((h, c));
        }
        Ok(out)
    }

    /// Nearest common ancestor of two commits (linear history: walk by generation).
    pub async fn merge_base(&self, a: Hash, b: Hash) -> Result<Option<Hash>> {
        let (mut a, mut b) = (Some(a), Some(b));
        loop {
            let (Some(x), Some(y)) = (a, b) else { return Ok(None) };
            if x == y {
                return Ok(Some(x));
            }
            let (cx, cy) = (self.commit_meta(&x).await?, self.commit_meta(&y).await?);
            if cx.generation >= cy.generation {
                a = cx.parent;
            }
            if cy.generation >= cx.generation {
                b = cy.parent;
            }
        }
    }

    pub async fn is_ancestor(&self, ancestor: Hash, of: Hash) -> Result<bool> {
        let target = self.commit_meta(&ancestor).await?.generation;
        let mut cur = Some(of);
        while let Some(h) = cur {
            if h == ancestor {
                return Ok(true);
            }
            let c = self.commit_meta(&h).await?;
            if c.generation <= target {
                return Ok(false);
            }
            cur = c.parent;
        }
        Ok(false)
    }

    pub async fn root_of(&self, commit: Option<Hash>) -> Result<Hash> {
        match commit {
            Some(h) => Ok(self.commit_obj(&h).await?.root),
            None => Ok(build_page(Page::Leaf(vec![])).hash),
        }
    }

    // ---------- pages / trees ----------

    pub async fn page(&self, h: &Hash) -> Result<Arc<Page>> {
        if let Some(p) = self.pages.lock().unwrap().get(h) {
            return Ok(p.clone());
        }
        let empty = build_page(Page::Leaf(vec![]));
        let page = if *h == empty.hash {
            empty.page
        } else {
            match self.cache_read("pages", h).and_then(|d| decode_stored_page(h, &d).ok()) {
                Some(p) => p,
                None => {
                    let d = self.store.get(&page_key(h)).await?;
                    let p = decode_stored_page(h, &d)?;
                    self.cache_write("pages", h, &d);
                    p
                }
            }
        };
        let p = Arc::new(page);
        self.pages.lock().unwrap().insert(*h, p.clone());
        Ok(p)
    }

    /// The ordered list of leaf references under `root`, plus every internal page hash seen.
    /// Walks one level at a time so a tree of depth d costs d round trips.
    pub async fn leaf_refs(&self, root: Hash) -> Result<(Vec<Child>, HashSet<Hash>)> {
        let mut internal = HashSet::new();
        let top = self.page(&root).await?;
        if let Page::Leaf(_) = &*top {
            let c = leaf_child(&BuiltPage { hash: root, data: Bytes::new(), page: (*top).clone() });
            return Ok((if c.files == 0 { vec![] } else { vec![c] }, internal));
        }
        internal.insert(root);
        let mut level_pages = vec![top];
        loop {
            let children: Vec<Child> = level_pages
                .iter()
                .flat_map(|p| match &**p {
                    Page::Node { children, .. } => children.clone(),
                    Page::Leaf(_) => vec![],
                })
                .collect();
            let level = level_pages[0].level();
            if level == 1 {
                return Ok((children, internal));
            }
            internal.extend(children.iter().map(|c| c.hash));
            level_pages = run_ordered(children.iter().map(|c| self.page(&c.hash)).collect(), PARALLELISM).await?;
        }
    }

    async fn leaves(&self, refs: &[Child]) -> Result<Vec<Arc<Page>>> {
        run_ordered(refs.iter().map(|c| self.page(&c.hash)).collect(), PARALLELISM).await
    }

    /// Every entry in the tree, in path order.
    pub async fn entries(&self, root: Hash) -> Result<Vec<Entry>> {
        let (refs, _) = self.leaf_refs(root).await?;
        let pages = self.leaves(&refs).await?;
        Ok(pages
            .iter()
            .flat_map(|p| match &**p {
                Page::Leaf(es) => es.clone(),
                Page::Node { .. } => vec![],
            })
            .collect())
    }

    pub async fn get_entry(&self, root: Hash, path: &str) -> Result<Option<Entry>> {
        let (refs, _) = self.leaf_refs(root).await?;
        let i = refs.partition_point(|c| c.key.as_str() < path);
        let Some(c) = refs.get(i) else { return Ok(None) };
        let p = self.page(&c.hash).await?;
        let Page::Leaf(es) = &*p else { bail!("expected leaf") };
        Ok(es.binary_search_by(|e| e.path.as_str().cmp(path)).ok().map(|i| es[i].clone()))
    }

    /// Changes from tree `a` to tree `b`. Leaves shared by hash are never fetched.
    pub async fn diff(&self, a: Hash, b: Hash) -> Result<Vec<Change>> {
        if a == b {
            return Ok(vec![]);
        }
        let ((ra, _), (rb, _)) = futures::try_join!(self.leaf_refs(a), self.leaf_refs(b))?;
        let ha: HashSet<Hash> = ra.iter().map(|c| c.hash).collect();
        let hb: HashSet<Hash> = rb.iter().map(|c| c.hash).collect();
        let da: Vec<Child> = ra.into_iter().filter(|c| !hb.contains(&c.hash)).collect();
        let db: Vec<Child> = rb.into_iter().filter(|c| !ha.contains(&c.hash)).collect();
        let (pa, pb) = futures::try_join!(self.leaves(&da), self.leaves(&db))?;
        let flat = |ps: &[Arc<Page>]| -> Vec<Entry> {
            ps.iter()
                .flat_map(|p| match &**p {
                    Page::Leaf(es) => es.clone(),
                    _ => vec![],
                })
                .collect()
        };
        Ok(diff_sorted(&flat(&pa), &flat(&pb)))
    }

    /// Apply a path-level delta to a tree. Only leaves containing edits (plus a neighbour
    /// when a page boundary key is deleted) are rewritten; internal levels are rebuilt
    /// canonically from the leaf list and only pages that did not exist before are returned.
    pub async fn apply_delta(&self, root: Hash, delta: &Delta) -> Result<NewTree> {
        let (refs, known) = self.leaf_refs(root).await?;
        let mut out: Vec<Child> = vec![];
        let mut new_leaves: Vec<BuiltPage> = vec![];
        if refs.is_empty() {
            let es: Vec<Entry> = delta.values().flatten().cloned().collect();
            new_leaves = build_leaves(es);
            out = new_leaves.iter().map(leaf_child).collect();
        } else {
            let mut per_leaf: BTreeMap<usize, Vec<(&String, &Option<Entry>)>> = BTreeMap::new();
            for (k, v) in delta {
                let i = refs.partition_point(|c| c.key.as_str() < k.as_str()).min(refs.len() - 1);
                per_leaf.entry(i).or_default().push((k, v));
            }
            // Prefetch the leaves we know we must rewrite.
            let touched: Vec<Child> = per_leaf.keys().map(|&i| refs[i].clone()).collect();
            self.leaves(&touched).await?;
            let mut pending: Vec<Entry> = vec![];
            for (i, r) in refs.iter().enumerate() {
                let edits = per_leaf.get(&i);
                if edits.is_none() && pending.is_empty() {
                    out.push(r.clone());
                    continue;
                }
                let page = self.page(&r.hash).await?;
                let Page::Leaf(es) = &*page else { bail!("expected leaf page") };
                let merged = match edits {
                    Some(ed) => merge_edits(es, ed),
                    None => es.clone(),
                };
                pending.extend(merged);
                let (groups, tail) = split_runs(std::mem::take(&mut pending), 0, |e| e.path.as_str());
                for g in groups {
                    let b = build_page(Page::Leaf(g));
                    out.push(leaf_child(&b));
                    new_leaves.push(b);
                }
                pending = tail;
            }
            if !pending.is_empty() {
                let b = build_page(Page::Leaf(pending));
                out.push(leaf_child(&b));
                new_leaves.push(b);
            }
        }
        let files = out.iter().map(|c| c.files).sum();
        let bytes = out.iter().map(|c| c.bytes).sum();
        let (root, upper) = build_upper(out);
        let mut pages = new_leaves;
        pages.extend(upper.into_iter().filter(|p| !known.contains(&p.hash)));
        Ok(NewTree { root, pages, files, bytes })
    }

    async fn upload_pages(&self, pages: &[BuiltPage]) -> Result<usize> {
        let empty = build_page(Page::Leaf(vec![])).hash;
        let todo: Vec<&BuiltPage> = pages.iter().filter(|p| p.hash != empty).collect();
        let n = todo.len();
        run_unordered(todo.into_iter().map(|p| async move {
            self.store.put_new(&page_key(&p.hash), p.data.clone()).await?;
            self.cache_write("pages", &p.hash, &p.data);
            self.pages.lock().unwrap().insert(p.hash, Arc::new(p.page.clone()));
            Ok::<_, anyhow::Error>(())
        }).collect(), PARALLELISM)
        .await?;
        Ok(n)
    }

    // ---------- file contents ----------

    /// Store file contents and return their manifest entries (same order as `inputs`).
    /// `reuse` maps content hashes to locations already in the store (dedupe).
    pub async fn write_files(&self, inputs: &[FileInput], reuse: &HashMap<Hash, Loc>) -> Result<Vec<Entry>> {
        enum Pending {
            Done(Loc),
            InPack { pack: usize, off: u64, len: u64 },
        }
        let mut slots: Vec<(Hash, Pending)> = Vec::with_capacity(inputs.len());
        let mut packs: Vec<PackBuilder> = vec![PackBuilder::default()];
        let mut in_push: HashMap<Hash, (usize, u64, u64)> = HashMap::new();
        let mut chunk_uploads: Vec<(Hash, Bytes)> = vec![];
        let mut chunk_seen: HashSet<Hash> = HashSet::new();

        for f in inputs {
            let h = Hash::of(&f.data);
            let slot = if f.kind == Kind::Symlink || f.data.len() <= INLINE_MAX {
                Pending::Done(Loc::Inline(f.data.to_vec()))
            } else if let Some(loc) = reuse.get(&h) {
                Pending::Done(loc.clone())
            } else if f.data.len() >= CHUNK_THRESHOLD {
                let mut refs = vec![];
                for (off, len) in chunk_spans(&f.data) {
                    let piece = &f.data[off..off + len];
                    let ch = Hash::of(piece);
                    if chunk_seen.insert(ch) {
                        chunk_uploads.push((ch, Bytes::from(encode_blob(piece))));
                    }
                    refs.push(ChunkRef { hash: ch, len: len as u64 });
                }
                Pending::Done(Loc::Chunks(refs))
            } else if let Some(&(pack, off, len)) = in_push.get(&h) {
                Pending::InPack { pack, off, len }
            } else {
                if packs.last().unwrap().len() >= PACK_MAX {
                    packs.push(PackBuilder::default());
                }
                let pack = packs.len() - 1;
                let (off, len) = packs[pack].add(h, &f.data);
                in_push.insert(h, (pack, off, len));
                Pending::InPack { pack, off, len }
            };
            slots.push((h, slot));
        }

        let finished: Vec<(Hash, Bytes)> =
            packs.into_iter().filter(|p| !p.is_empty()).map(|p| p.finish()).collect();
        // Packs are new by construction. Chunks of an edited large file mostly exist already,
        // and a create-if-absent PUT still sends the body, so ask first (a HEAD costs no bytes).
        let uploads = finished
            .iter()
            .map(|(h, d)| (pack_key(h), d.clone(), false))
            .chain(chunk_uploads.into_iter().map(|(h, d)| (chunk_key(&h), d, true)));
        run_unordered(
            uploads
                .map(|(k, d, check)| async move {
                    if check && self.store.size(&k).await?.is_some() {
                        return Ok(false);
                    }
                    self.store.put_new(&k, d).await
                })
                .collect(),
            PARALLELISM,
        )
        .await?;

        Ok(inputs
            .iter()
            .zip(slots)
            .map(|(f, (hash, slot))| {
                let loc = match slot {
                    Pending::Done(l) => l,
                    Pending::InPack { pack, off, len } => Loc::Pack { pack: finished[pack].0, off, len },
                };
                Entry { path: f.path.clone(), kind: f.kind, size: f.data.len() as u64, hash, loc }
            })
            .collect())
    }

    /// Read file contents for entries (same order). Every result is hash-verified.
    pub async fn read_files(&self, entries: &[Entry]) -> Result<Vec<Bytes>> {
        let mut out: Vec<Option<Bytes>> = vec![None; entries.len()];
        let mut by_pack: BTreeMap<Hash, Vec<usize>> = BTreeMap::new();
        let mut chunked: Vec<usize> = vec![];
        for (i, e) in entries.iter().enumerate() {
            if !matches!(e.loc, Loc::Inline(_)) {
                if let Some(d) = self.blobs.as_ref().and_then(|b| b.read(&e.hash)) {
                    out[i] = Some(d);
                    continue;
                }
            }
            match &e.loc {
                Loc::Inline(b) => out[i] = Some(Bytes::from(b.clone())),
                Loc::Pack { pack, .. } => by_pack.entry(*pack).or_default().push(i),
                Loc::Chunks(_) => chunked.push(i),
                Loc::Unresolved => bail!("{}: content {} is not cached and has no known location", e.path, e.hash.short()),
            }
        }
        let jobs = by_pack
            .into_iter()
            .map(|(pack, idxs)| futures::future::Either::Left(self.read_pack_group(entries, pack, idxs)))
            .chain(chunked.into_iter().map(|i| {
                futures::future::Either::Right(async move { Ok(vec![(i, self.read_chunked(&entries[i]).await?)]) })
            }));
        let results: Vec<Vec<(usize, Bytes)>> =
            run_unordered(jobs.collect(), PARALLELISM).await?;
        let mut fetched = vec![false; entries.len()];
        for (i, b) in results.into_iter().flatten() {
            fetched[i] = true;
            out[i] = Some(b);
        }
        let verified: Vec<Bytes> = entries
            .iter()
            .zip(out)
            .map(|(e, b)| {
                let b = b.ok_or_else(|| anyhow!("missing content for {}", e.path))?;
                anyhow::ensure!(
                    b.len() as u64 == e.size && Hash::of(&b) == e.hash,
                    "content of {} failed hash verification",
                    e.path
                );
                Ok(b)
            })
            .collect::<Result<_>>()?;
        // Cache what came from the store, now that it is verified.
        if let Some(blobs) = &self.blobs {
            for (i, b) in verified.iter().enumerate() {
                if fetched[i] {
                    blobs.put(&entries[i].hash, b)?;
                }
            }
        }
        Ok(verified)
    }

    async fn read_pack_group(&self, entries: &[Entry], pack: Hash, mut idxs: Vec<usize>) -> Result<Vec<(usize, Bytes)>> {
        let span = |i: usize| match &entries[i].loc {
            Loc::Pack { off, len, .. } => *off..*off + *len,
            _ => unreachable!(),
        };
        idxs.sort_by_key(|&i| span(i).start);
        let mut got = Vec::with_capacity(idxs.len());
        // Bound each request batch so huge packs still stream in manageable pieces.
        for batch in idxs.chunks(4096) {
            let ranges: Vec<_> = batch.iter().map(|&i| span(i)).collect();
            let bytes = self.store.get_ranges(&pack_key(&pack), &ranges).await?;
            for (&i, b) in batch.iter().zip(bytes) {
                got.push((i, Bytes::from(decode_blob(&b, entries[i].size)?)));
            }
        }
        Ok(got)
    }

    async fn read_chunked(&self, e: &Entry) -> Result<Bytes> {
        let Loc::Chunks(chunks) = &e.loc else { bail!("not chunked") };
        let mut parts = Vec::with_capacity(chunks.len());
        let fetched: Vec<Bytes> =
            run_ordered(chunks.iter().map(|c| self.store.get_owned(chunk_key(&c.hash))).collect(), 16).await?;
        for (c, d) in chunks.iter().zip(fetched) {
            let raw = decode_blob(&d, c.len)?;
            anyhow::ensure!(Hash::of(&raw) == c.hash, "chunk {} corrupt", c.hash.short());
            parts.push(raw);
        }
        Ok(Bytes::from(parts.concat()))
    }

    // ---------- commit protocol ----------

    /// Build and store a commit applying `delta` to `parent`, without moving any ref. The
    /// commit records its change list against `parent`. Immutable and idempotent: an
    /// unreferenced result is garbage for `gc`.
    pub async fn write_commit(&self, parent: Option<Hash>, delta: &Delta, f: &CommitFields) -> Result<Written> {
        let parent_root = self.root_of(parent).await?;
        let tree = self.apply_delta(parent_root, delta).await?;
        // The exact change against `parent`: paths whose kind or content differ (no-op edits
        // drop out).
        let mut changes = BTreeMap::new();
        for (path, new) in delta {
            let old = self.get_entry(parent_root, path).await?;
            let same = match (&old, new) {
                (Some(a), Some(b)) => a.same_content(b),
                (None, None) => true,
                _ => false,
            };
            if !same {
                changes.insert(
                    path.clone(),
                    crate::objects::FileChange {
                        old: old.as_ref().map(|e| crate::objects::FileState::of(e, false)),
                        new: new.as_ref().map(|e| crate::objects::FileState::of(e, true)),
                    },
                );
            }
        }
        let pages = self.upload_pages(&tree.pages).await?;
        let generation = match parent {
            Some(p) => self.commit_meta(&p).await?.generation + 1,
            None => 0,
        };
        let commit = Commit {
            parent,
            root: tree.root,
            generation,
            files: tree.files,
            bytes: tree.bytes,
            author: f.author.clone(),
            author_date: f.author_date.clone(),
            committer: f.committer.clone(),
            committer_date: f.committer_date.clone(),
            message: f.message.clone(),
            meta: f.meta.clone(),
            changes: Some(crate::objects::Changes::new(&changes)),
        };
        let (ch, data) = commit.encode();
        self.store.put_new(&commit_key(&ch), data.clone()).await?;
        self.cache_write("commits", &ch, &data);
        Ok(Written { commit: ch, pages, files: tree.files, bytes: tree.bytes })
    }

    /// Move `branch` from `expected` (None: create) to `commit` in one compare-and-swap.
    /// Conflict means someone else moved it first.
    pub async fn advance_ref(&self, branch: &str, expected: Option<&Version>, commit: Hash) -> Result<Cas> {
        let body = Bytes::from(serde_json::to_vec(&RefValue { commit })?);
        self.store.cas(&ref_key(branch), body, expected).await
    }

    /// A delta that reproduces commit `h` on another parent: its change list (new sides), or
    /// its manifest diff for commits written before change lists.
    pub async fn delta_of(&self, h: &Hash) -> Result<Delta> {
        let c = self.commit_obj(h).await?;
        let changes = match c.change_list() {
            Some(cs) => cs,
            None => self.diff(self.root_of(c.parent).await?, c.root).await?,
        };
        Ok(changes.into_iter().map(|c| (c.path, c.new)).collect())
    }

    /// Land every commit of `branch` that `onto` lacks, in order, with a single CAS on `onto`
    /// (no other ref is written): a fast-forward when `onto` hasn't moved since the stack was
    /// based on it, otherwise each commit is replayed on the new head (author, dates and
    /// message kept; committer is the lander; `rewritten.from` names the original) and `onto`
    /// moves once to the last one. Commits already landed (found by `rewritten.from` on
    /// `onto`) are skipped, so landing is idempotent and a growing stack lands incrementally.
    /// A stack commit touching a path `onto` changed meanwhile is a conflict; nothing moves.
    pub async fn land_stack(&self, branch: &str, onto: &str, committer: &str, date: &str) -> Result<LandOutcome> {
        self.land_stack_with(branch, onto, committer, date, None).await
    }

    /// [`Repo::land_stack`], asking `check` before the first publish attempt.
    pub async fn land_stack_with(
        &self,
        branch: &str,
        onto: &str,
        committer: &str,
        date: &str,
        check: Option<PublishCheck<'_>>,
    ) -> Result<LandOutcome> {
        validate_branch(onto)?;
        let mut checked = false;
        let started = std::time::Instant::now();
        let mut attempts = 0;
        loop {
            attempts += 1;
            let (tip, _) = self.read_ref(branch).await?.with_context(|| format!("no branch {branch}"))?;
            let (onto_tip, version) = self.read_ref(onto).await?.with_context(|| format!("no branch {onto}"))?;
            let base = self.merge_base(tip, onto_tip).await?.context("branches share no history")?;
            let mut stack = vec![];
            let mut cur = Some(tip);
            while let Some(h) = cur.filter(|h| *h != base) {
                stack.push(h);
                cur = self.commit_meta(&h).await?.parent;
            }
            stack.reverse();
            // Stack commits `onto` already has as replayed copies: a prefix of the stack, since
            // landing keeps order. The rest goes on top of the last copy.
            let mut copies: HashMap<String, Hash> = HashMap::new();
            let mut cur = Some(onto_tip);
            while let Some(h) = cur.filter(|h| *h != base) {
                let c = self.commit_meta(&h).await?;
                if let Some(orig) = c.meta.get("rewritten.from") {
                    copies.entry(orig.clone()).or_insert(h);
                }
                cur = c.parent;
            }
            let done = stack.iter().take_while(|h| copies.contains_key(&h.hex())).count();
            let from = if done == 0 { base } else { copies[&stack[done - 1].hex()] };
            let stack: Vec<Hash> = stack[done..].to_vec();
            if stack.is_empty() {
                return Ok(LandOutcome { tip: onto_tip, landed: vec![], fast_forward: true, attempts });
            }
            let (new_tip, landed, ff) = if done == 0 && base == onto_tip {
                (tip, stack.clone(), true)
            } else {
                // Paths `onto` changed since what the rest of the stack builds on must not be
                // touched by it.
                let theirs: std::collections::BTreeSet<String> =
                    self.diff(self.root_of(Some(from)).await?, self.root_of(Some(onto_tip)).await?).await?.into_iter().map(|c| c.path).collect();
                let mut deltas = vec![];
                let mut overlap = std::collections::BTreeSet::new();
                for h in &stack {
                    let d = self.delta_of(h).await?;
                    overlap.extend(d.keys().filter(|p| theirs.contains(*p)).cloned());
                    deltas.push(d);
                }
                if !overlap.is_empty() {
                    return Err(ConflictError { branch: onto.to_string(), paths: overlap.into_iter().collect() }.into());
                }
                let mut parent = onto_tip;
                let mut landed = vec![];
                for (h, d) in stack.iter().zip(&deltas) {
                    let c = self.commit_meta(h).await?;
                    let mut meta = c.meta.clone();
                    meta.insert("landed.from".into(), branch.to_string());
                    meta.insert("rewritten.from".into(), h.hex());
                    let fields = CommitFields {
                        author: c.author.clone(),
                        author_date: c.author_date.clone(),
                        committer: committer.to_string(),
                        committer_date: date.to_string(),
                        message: c.message.clone(),
                        meta,
                    };
                    parent = self.write_commit(Some(parent), d, &fields).await?.commit;
                    landed.push(parent);
                }
                (parent, landed, false)
            };
            if !checked {
                if let Some(check) = check {
                    check(&Publish { branch: onto.to_string(), old: Some(onto_tip), new: new_tip })?;
                }
                checked = true;
            }
            // The one write that publishes the stack.
            match self.advance_ref(onto, Some(&version), new_tip).await {
                Ok(Cas::Ok(_)) => {
                    return Ok(LandOutcome { tip: new_tip, landed, fast_forward: ff, attempts });
                }
                Ok(Cas::Conflict) | Err(_) => {
                    if let Some((now, _)) = self.read_ref(onto).await? {
                        if now == new_tip {
                            return Ok(LandOutcome { tip: new_tip, landed, fast_forward: ff, attempts });
                        }
                    }
                }
            }
            if started.elapsed() >= COMMIT_DEADLINE {
                bail!("gave up landing after {attempts} attempts: {onto} is too contended");
            }
            backoff(attempts).await;
        }
    }

    /// Commit a delta to a branch: upload immutable objects, then compare-and-swap the ref.
    /// If the branch moved, the delta is replayed on the new head when no path overlaps.
    pub async fn commit(&self, req: CommitRequest) -> Result<CommitOutcome> {
        self.commit_with(req, None).await
    }

    /// [`Repo::commit`], asking `check` before the first publish attempt.
    pub async fn commit_with(&self, req: CommitRequest, check: Option<PublishCheck<'_>>) -> Result<CommitOutcome> {
        validate_branch(&req.branch)?;
        let mut checked: Option<Hash> = None;
        let base_root = self.root_of(req.base).await?;
        let mut attempts = 0;
        let started = std::time::Instant::now();
        let mut pages_written = 0;
        loop {
            attempts += 1;
            let head = self.read_ref(&req.branch).await?;
            let (parent, expected) = match &head {
                Some((h, v)) => (Some(*h), Some(v.clone())),
                None => (None, None),
            };
            if head.is_none() && req.base.is_some() {
                bail!("branch {} does not exist", req.branch);
            }
            let head_root = self.root_of(parent).await?;
            if parent != req.base {
                // Someone else committed since our base. Replay only if paths are disjoint.
                let theirs = self.diff(base_root, head_root).await?;
                let overlap: Vec<String> =
                    theirs.into_iter().filter(|c| req.delta.contains_key(&c.path)).map(|c| c.path).collect();
                if !overlap.is_empty() {
                    return Err(ConflictError { branch: req.branch.clone(), paths: overlap }.into());
                }
            }
            let fields = CommitFields {
                author: req.author.clone(),
                author_date: req.author_date.clone(),
                committer: req.committer.clone(),
                committer_date: req.committer_date.clone(),
                message: req.message.clone(),
                meta: req.meta.clone(),
            };
            let tree = self.write_commit(parent, &req.delta, &fields).await?;
            pages_written += tree.pages;
            let ch = tree.commit;
            if checked.is_none() {
                if let Some(check) = check {
                    check(&Publish { branch: req.branch.clone(), old: parent, new: ch })?;
                }
                checked = Some(ch);
            }
            let replayed_from = checked.filter(|c| *c != ch);
            let body = Bytes::from(serde_json::to_vec(&RefValue { commit: ch })?);
            match self.store.cas(&ref_key(&req.branch), body, expected.as_ref()).await {
                Ok(Cas::Ok(version)) => {
                    return Ok(CommitOutcome {
                        commit: ch,
                        version,
                        attempts,
                        pages_written,
                        files: tree.files,
                        bytes: tree.bytes,
                        replayed_from,
                    });
                }
                Ok(Cas::Conflict) => {}
                Err(e) => {
                    // Unknown outcome (e.g. timeout): the ref tells us whether we won.
                    if let Some((now, v)) = self.read_ref(&req.branch).await? {
                        if now == ch {
                            return Ok(CommitOutcome {
                                commit: ch,
                                version: v,
                                attempts,
                                pages_written,
                                files: tree.files,
                                bytes: tree.bytes,
                                replayed_from,
                            });
                        }
                    }
                    if started.elapsed() >= COMMIT_DEADLINE {
                        return Err(e);
                    }
                }
            }
            if started.elapsed() >= COMMIT_DEADLINE {
                bail!("gave up after {attempts} attempts in {:?}: {} is too contended", started.elapsed(), req.branch);
            }
            backoff(attempts).await;
        }
    }
}

/// Apply sorted edits to a sorted entry list.
fn merge_edits(entries: &[Entry], edits: &[(&String, &Option<Entry>)]) -> Vec<Entry> {
    let mut out = Vec::with_capacity(entries.len() + edits.len());
    let (mut i, mut j) = (0, 0);
    while i < entries.len() || j < edits.len() {
        let take_edit = match (entries.get(i), edits.get(j)) {
            (Some(e), Some((k, _))) => match e.path.as_str().cmp(k.as_str()) {
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => {
                    i += 1; // replaced or deleted
                    true
                }
                std::cmp::Ordering::Greater => true,
            },
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => unreachable!(),
        };
        if take_edit {
            if let Some(e) = edits[j].1 {
                out.push(e.clone());
            }
            j += 1;
        } else {
            out.push(entries[i].clone());
            i += 1;
        }
    }
    out
}

/// Run futures with bounded concurrency, keeping input order.
pub async fn run_ordered<T, F: Future<Output = Result<T>>>(futs: Vec<F>, n: usize) -> Result<Vec<T>> {
    stream::iter(futs).buffered(n).try_collect().await
}

/// Run futures with bounded concurrency, in completion order.
pub async fn run_unordered<T, F: Future<Output = Result<T>>>(futs: Vec<F>, n: usize) -> Result<Vec<T>> {
    stream::iter(futs).buffer_unordered(n).try_collect().await
}

/// Wait before retrying a lost CAS. Losing once only means another writer got in first, and
/// the rebase is local, so the first retry is immediate; repeated losses back off with full
/// jitter (capped) to spread contending writers out instead of synchronising them.
async fn backoff(attempts: u32) {
    if attempts <= 1 {
        return;
    }
    let cap = (5u64 << attempts.min(9)).min(2000);
    tokio::time::sleep(Duration::from_millis(rand::random::<u64>() % cap + 1)).await;
}
