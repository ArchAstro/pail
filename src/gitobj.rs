//! Git's object model computed from pail data: blob ids, tree objects and ids for any
//! directory, and raw commit objects. Pail stores no trees; these are derived from the
//! manifest and file contents (blob ids are cached locally by content hash).
//!
//! Object names follow Git: `<rev>`, `<rev>^{tree}`, `<rev>:<path>` (`<rev>:` is the root).

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use bytes::Bytes;

use crate::gitsrc::{RawCommit, commit_bytes, mode_for_kind, object_id};
use crate::hash::Hash;
use crate::manifest::{Entry, Kind};
use crate::objects::Commit;
use crate::rev::{Ctx, Ids};
use crate::snapshot::Snapshot;

/// What an object name resolves to.
pub enum Object {
    Commit { hash: Hash, commit: std::sync::Arc<Commit> },
    /// A directory of a commit ("" for the root).
    Tree { commit: Hash, path: String },
    Blob { commit: Hash, entry: Entry },
}

impl Object {
    pub fn kind(&self) -> &'static str {
        match self {
            Object::Commit { .. } => "commit",
            Object::Tree { .. } => "tree",
            Object::Blob { .. } => "blob",
        }
    }
}

/// One entry of a Git tree.
#[derive(Clone, Debug)]
pub struct TreeItem {
    /// `100644`, `100755`, `120000` or `40000`.
    pub mode: &'static str,
    pub kind: &'static str,
    pub oid: String,
    pub name: String,
    /// Full path from the repository root.
    pub path: String,
    pub size: Option<u64>,
}

pub struct GitObjects<'a> {
    pub ctx: &'a Ctx,
    blob_ids: std::sync::Mutex<HashMap<Hash, String>>,
}

fn cache_file(h: &Hash) -> Option<std::path::PathBuf> {
    let dir = crate::repo::default_cache_dir()?;
    let hex = h.hex();
    Some(dir.join("gitblob").join(&hex[..2]).join(hex))
}

impl<'a> GitObjects<'a> {
    pub fn new(ctx: &'a Ctx) -> GitObjects<'a> {
        GitObjects { ctx, blob_ids: Default::default() }
    }

    /// Git blob ids for entries, reading content only for ids not cached.
    pub async fn blob_ids(&self, entries: &[Entry]) -> Result<HashMap<Hash, String>> {
        let mut out = HashMap::new();
        let mut missing: Vec<Entry> = vec![];
        {
            let mem = self.blob_ids.lock().unwrap();
            for e in entries {
                if out.contains_key(&e.hash) {
                    continue;
                }
                if let Some(id) = mem.get(&e.hash) {
                    out.insert(e.hash, id.clone());
                } else if let Some(id) = cache_file(&e.hash).and_then(|p| std::fs::read_to_string(p).ok()).filter(|s| s.len() == 40) {
                    out.insert(e.hash, id);
                } else if !missing.iter().any(|m| m.hash == e.hash) {
                    missing.push(e.clone());
                }
            }
        }
        let mut start = 0;
        while start < missing.len() {
            let mut end = start;
            let mut bytes = 0;
            while end < missing.len() && (end == start || bytes < 256 << 20) {
                bytes += missing[end].size;
                end += 1;
            }
            let data = self.ctx.repo.read_files(&missing[start..end]).await?;
            for (e, d) in missing[start..end].iter().zip(data) {
                let id = object_id("blob", &d);
                if let Some(p) = cache_file(&e.hash) {
                    let _ = std::fs::create_dir_all(p.parent().unwrap());
                    let _ = std::fs::write(&p, &id);
                }
                out.insert(e.hash, id);
            }
            start = end;
        }
        self.blob_ids.lock().unwrap().extend(out.iter().map(|(k, v)| (*k, v.clone())));
        Ok(out)
    }

    /// Resolve `<rev>`, `<rev>^{tree}`, `<rev>:<path>`.
    pub async fn resolve(&self, name: &str) -> Result<Object> {
        if let Some((rev, path)) = name.split_once(':') {
            let commit = if rev.is_empty() { self.ctx.head.context("HEAD has no commits")? } else { self.ctx.resolve(rev).await? };
            let path = path.trim_start_matches("./").trim_matches('/');
            let snap = Snapshot::open(&self.ctx.repo, commit).await?;
            return match snap.stat(path).await? {
                Some(n) if n.entry.is_some() => Ok(Object::Blob { commit, entry: n.entry.unwrap() }),
                Some(_) => Ok(Object::Tree { commit, path: path.to_string() }),
                None => bail!("path '{path}' does not exist in '{rev}'"),
            };
        }
        if let Some(rev) = name.strip_suffix("^{tree}") {
            return Ok(Object::Tree { commit: self.ctx.resolve(rev).await?, path: String::new() });
        }
        let hash = self.ctx.resolve(name).await?;
        let commit = self.ctx.commit(&hash).await?;
        Ok(Object::Commit { hash, commit })
    }

    /// Items of a directory, with Git ids for blobs and subtrees (subtree ids hash
    /// everything under them).
    pub async fn tree_items(&self, commit: Hash, dir: &str, recursive: bool) -> Result<Vec<TreeItem>> {
        let snap = Snapshot::open(&self.ctx.repo, commit).await?;
        let files = snap.walk(dir).await?;
        if files.is_empty() && !dir.is_empty() {
            bail!("not a tree: {dir}");
        }
        let ids = self.blob_ids(&files).await?;
        let skip = if dir.is_empty() { 0 } else { dir.len() + 1 };
        let rel: Vec<(String, &Entry)> = files.iter().map(|e| (e.path[skip..].to_string(), e)).collect();
        let tree_ids = subtree_ids(&rel, &ids)?;
        let full = |r: &str| if dir.is_empty() { r.to_string() } else { format!("{dir}/{r}") };
        let mut items = vec![];
        let mut seen = std::collections::HashSet::new();
        for (r, e) in &rel {
            let parts: Vec<&str> = r.split('/').collect();
            let depth = if recursive { parts.len() - 1 } else { parts.len().min(2) - 1 };
            // Directories on the way: all of them when recursive, the first otherwise.
            for i in 0..depth {
                let d = parts[..=i].join("/");
                if seen.insert(d.clone()) {
                    items.push(TreeItem { mode: "40000", kind: "tree", oid: tree_ids[&d].clone(), name: parts[i].to_string(), path: full(&d), size: None });
                }
            }
            if recursive || parts.len() == 1 {
                items.push(TreeItem {
                    mode: mode_for_kind(e.kind),
                    kind: "blob",
                    oid: ids[&e.hash].clone(),
                    name: parts[parts.len() - 1].to_string(),
                    path: e.path.clone(),
                    size: Some(e.size),
                });
            }
        }
        // Git tree order: names compare as if directories ended with '/'.
        items.sort_by(|a, b| tree_key(a).cmp(&tree_key(b)));
        Ok(items)
    }

    pub async fn tree_id(&self, commit: Hash, dir: &str) -> Result<String> {
        let snap = Snapshot::open(&self.ctx.repo, commit).await?;
        let files = snap.walk(dir).await?;
        let ids = self.blob_ids(&files).await?;
        let skip = if dir.is_empty() { 0 } else { dir.len() + 1 };
        let rel: Vec<(String, &Entry)> = files.iter().map(|e| (e.path[skip..].to_string(), e)).collect();
        Ok(subtree_ids(&rel, &ids)?.remove("").unwrap_or_else(|| object_id("tree", b"")))
    }

    /// The raw tree object of a directory.
    pub async fn tree_bytes(&self, commit: Hash, dir: &str) -> Result<Vec<u8>> {
        let items = self.tree_items(commit, dir, false).await?;
        let mut buf = vec![];
        for it in items {
            buf.extend_from_slice(format!("{} {}\0", it.mode, it.name).as_bytes());
            buf.extend_from_slice(&hex::decode(&it.oid)?);
        }
        Ok(buf)
    }

    /// The raw Git commit object (exactly the original bytes for mirrored commits).
    pub async fn commit_bytes(&self, h: &Hash, c: &Commit) -> Result<Vec<u8>> {
        let tree = self.tree_id(*h, "").await?;
        let message = match c.meta.get("git.message_hex") {
            Some(x) => hex::decode(x)?,
            None => c.message.clone().into_bytes(),
        };
        let extra_headers = match (c.meta.get("git.extra_headers"), c.meta.get("git.extra_headers_hex")) {
            (Some(s), _) => s.clone().into_bytes(),
            (_, Some(x)) => hex::decode(x)?,
            _ => vec![],
        };
        let parents = self.ctx.parent_ids(c);
        Ok(commit_bytes(&RawCommit {
            tree,
            parents,
            author: c.author.clone(),
            author_date: c.author_date.clone(),
            committer: c.committer.clone(),
            committer_date: c.committer_date.clone(),
            extra_headers,
            message,
        }))
    }

    /// The object's Git id (commits per `--ids`).
    pub async fn id(&self, o: &Object) -> Result<String> {
        Ok(match o {
            Object::Commit { hash, commit } => self.ctx.id(hash, commit),
            Object::Tree { commit, path } => self.tree_id(*commit, path).await?,
            Object::Blob { entry, .. } => self.blob_ids(std::slice::from_ref(entry)).await?[&entry.hash].clone(),
        })
    }

    pub async fn contents(&self, o: &Object) -> Result<Bytes> {
        Ok(match o {
            Object::Commit { hash, commit } => self.commit_bytes(hash, commit).await?.into(),
            Object::Tree { commit, path } => self.tree_bytes(*commit, path).await?.into(),
            Object::Blob { entry, .. } => self.ctx.repo.read_files(std::slice::from_ref(entry)).await?.remove(0),
        })
    }

    /// Whether ids printed for commits are Git's.
    pub fn git_ids(&self) -> bool {
        self.ctx.ids == Ids::Git
    }
}

fn tree_key(t: &TreeItem) -> Vec<u8> {
    let mut k = t.path.as_bytes().to_vec();
    if t.kind == "tree" {
        k.push(b'/');
    }
    k
}

/// Git ids of every directory (relative path, "" for the top) in a list of files.
fn subtree_ids(files: &[(String, &Entry)], blob_ids: &HashMap<Hash, String>) -> Result<HashMap<String, String>> {
    enum Node {
        Blob(&'static str, [u8; 20]),
        Dir(BTreeMap<String, Node>),
    }
    let mut root: BTreeMap<String, Node> = BTreeMap::new();
    for (path, e) in files {
        let mut parts: Vec<&str> = path.split('/').collect();
        let name = parts.pop().unwrap().to_string();
        let mut dir = &mut root;
        for p in parts {
            let n = dir.entry(p.to_string()).or_insert_with(|| Node::Dir(BTreeMap::new()));
            dir = match n {
                Node::Dir(d) => d,
                Node::Blob(..) => bail!("{path}: a file is also used as a directory"),
            };
        }
        let raw: [u8; 20] = hex::decode(&blob_ids[&e.hash])?.try_into().map_err(|_| anyhow::anyhow!("bad blob id"))?;
        dir.insert(name, Node::Blob(mode_for_kind(e.kind), raw));
    }
    fn write(prefix: &str, dir: &BTreeMap<String, Node>, out: &mut HashMap<String, String>) -> [u8; 20] {
        let mut items: Vec<(Vec<u8>, &str, [u8; 20], &str)> = dir
            .iter()
            .map(|(name, n)| match n {
                Node::Blob(mode, oid) => (name.as_bytes().to_vec(), *mode, *oid, name.as_str()),
                Node::Dir(d) => {
                    let p = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                    let mut k = name.as_bytes().to_vec();
                    k.push(b'/');
                    (k, "40000", write(&p, d, out), name.as_str())
                }
            })
            .collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        let mut buf = vec![];
        for (_, mode, oid, name) in items {
            buf.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            buf.extend_from_slice(&oid);
        }
        let id = object_id("tree", &buf);
        out.insert(prefix.to_string(), id.clone());
        hex::decode(id).unwrap().try_into().unwrap()
    }
    let mut out = HashMap::new();
    write("", &root, &mut out);
    Ok(out)
}

/// Mode as `ls-tree`/`cat-file -p` print it (trees with a leading zero).
pub fn display_mode(mode: &str) -> String {
    format!("{mode:0>6}")
}

pub fn kind_of_mode(k: Kind) -> &'static str {
    mode_for_kind(k)
}
