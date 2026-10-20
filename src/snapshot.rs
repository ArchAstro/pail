//! Read-only, filesystem-shaped view of one commit: stat, read, list a directory, walk.
//! Directories are implied by paths (as in Git); a path is a directory when some file lies
//! under it. Every call works against the store directly, so it serves bare repositories.

use anyhow::{Context, Result, bail};
use bytes::Bytes;

use crate::hash::Hash;
use crate::manifest::{Entry, Kind, Page};
use crate::repo::Repo;

pub struct Snapshot<'a> {
    pub repo: &'a Repo,
    pub commit: Hash,
    pub root: Hash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Exec,
    Symlink,
    Dir,
}

impl From<Kind> for NodeKind {
    fn from(k: Kind) -> Self {
        match k {
            Kind::File => NodeKind::File,
            Kind::Exec => NodeKind::Exec,
            Kind::Symlink => NodeKind::Symlink,
        }
    }
}

/// One name in a directory listing, or the result of `stat`.
#[derive(Clone, Debug)]
pub struct Node {
    /// Full path from the repository root ("" for the root).
    pub path: String,
    pub kind: NodeKind,
    /// File size, or total bytes of the files under a directory.
    pub size: u64,
    /// Files under a directory (1 for a file).
    pub files: u64,
    /// The file entry (content hash, storage location); None for directories.
    pub entry: Option<Entry>,
}

impl Node {
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }
}

/// Upper bound of the keys under `dir/` in byte order: `dir0` ('0' follows '/').
fn dir_bounds(dir: &str) -> (String, String) {
    if dir.is_empty() { (String::new(), "\u{10FFFF}".to_string()) } else { (format!("{dir}/"), format!("{dir}0")) }
}

impl<'a> Snapshot<'a> {
    pub async fn open(repo: &'a Repo, commit: Hash) -> Result<Snapshot<'a>> {
        let root = repo.commit_obj(&commit).await?.root;
        Ok(Snapshot { repo, commit, root })
    }

    /// Files whose paths fall in `[lo, hi)`, in path order. Only the leaves that can hold such
    /// paths are fetched.
    pub async fn range(&self, lo: &str, hi: &str) -> Result<Vec<Entry>> {
        let (refs, _) = self.repo.leaf_refs(self.root).await?;
        let first = refs.partition_point(|c| c.key.as_str() < lo);
        let mut out = vec![];
        for (i, c) in refs.iter().enumerate().skip(first) {
            // Leaf i holds keys in (refs[i-1].key, refs[i].key].
            if i > first && refs[i - 1].key.as_str() >= hi {
                break;
            }
            let p = self.repo.page(&c.hash).await?;
            let Page::Leaf(es) = &*p else { bail!("expected a leaf page") };
            out.extend(es.iter().filter(|e| e.path.as_str() >= lo && e.path.as_str() < hi).cloned());
            if c.key.as_str() >= hi {
                break;
            }
        }
        Ok(out)
    }

    /// Every file under `dir` ("" for the whole tree), recursively.
    pub async fn walk(&self, dir: &str) -> Result<Vec<Entry>> {
        let dir = dir.trim_end_matches('/');
        let (lo, hi) = dir_bounds(dir);
        self.range(&lo, &hi).await
    }

    pub async fn file(&self, path: &str) -> Result<Option<Entry>> {
        self.repo.get_entry(self.root, path).await
    }

    /// What `path` is: a file, a directory, or nothing.
    pub async fn stat(&self, path: &str) -> Result<Option<Node>> {
        let path = path.trim_matches('/');
        if !path.is_empty() {
            if let Some(e) = self.file(path).await? {
                return Ok(Some(Node { path: e.path.clone(), kind: e.kind.into(), size: e.size, files: 1, entry: Some(e) }));
            }
        }
        let under = self.walk(path).await?;
        if under.is_empty() && !path.is_empty() {
            return Ok(None);
        }
        let size = under.iter().map(|e| e.size).sum();
        Ok(Some(Node { path: path.to_string(), kind: NodeKind::Dir, size, files: under.len() as u64, entry: None }))
    }

    /// The immediate children of a directory, in Git tree order (byte order of full paths).
    pub async fn list_dir(&self, dir: &str) -> Result<Vec<Node>> {
        let dir = dir.trim_matches('/');
        let under = self.walk(dir).await?;
        if under.is_empty() && !dir.is_empty() {
            if self.file(dir).await?.is_some() {
                bail!("{dir} is not a directory");
            }
            bail!("{dir}: no such file or directory");
        }
        let skip = if dir.is_empty() { 0 } else { dir.len() + 1 };
        let mut out: Vec<Node> = vec![];
        for e in under {
            let rest = &e.path[skip..];
            match rest.split_once('/') {
                None => out.push(Node { path: e.path.clone(), kind: e.kind.into(), size: e.size, files: 1, entry: Some(e) }),
                Some((name, _)) => {
                    let path = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
                    match out.last_mut() {
                        Some(n) if n.kind == NodeKind::Dir && n.path == path => {
                            n.size += e.size;
                            n.files += 1;
                        }
                        _ => out.push(Node { path, kind: NodeKind::Dir, size: e.size, files: 1, entry: None }),
                    }
                }
            }
        }
        Ok(out)
    }

    /// A file's bytes (a symlink's target).
    pub async fn read(&self, path: &str) -> Result<Bytes> {
        let e = self.file(path).await?.with_context(|| format!("path '{path}' does not exist in the commit"))?;
        Ok(self.repo.read_files(std::slice::from_ref(&e)).await?.remove(0))
    }

    /// Bytes `[offset, offset+len)` of a file, clamped to its size.
    pub async fn read_range(&self, path: &str, offset: u64, len: u64) -> Result<Bytes> {
        let data = self.read(path).await?;
        let start = (offset as usize).min(data.len());
        let end = (offset.saturating_add(len) as usize).min(data.len());
        Ok(data.slice(start..end))
    }
}
