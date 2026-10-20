//! Where read commands run (a working copy or a bare store) and how revisions resolve.
//!
//! A bare repository is just the store: `pail --store s3://bucket/repo log`, `PAIL_STORE`, or
//! a plain directory path. Its HEAD is the store's default branch (the `HEAD` object written
//! by `pail init`, else `main`). In a working copy, HEAD is the checked-out base.
//!
//! Revisions follow Git: `HEAD`/`@`, branch names (`refs/heads/<b>` too), full or unique
//! abbreviated ids, `<rev>~<n>`, `<rev>^`, `<rev>^0`, `<rev>^{commit}`. For mirrored history,
//! Git commit ids resolve as well, and `--ids=git` displays them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use crate::hash::Hash;
use crate::objects::Commit;
use crate::repo::Repo;
use crate::worktree::Worktree;

/// Which commit ids to print.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ids {
    Pail,
    /// The Git commit id a mirrored commit was imported from (pail's id when there is none).
    Git,
}

/// The store's default branch, `HEAD` at the store root.
#[derive(Serialize, Deserialize)]
pub struct StoreHead {
    pub branch: String,
}

pub const STORE_HEAD_KEY: &str = "HEAD";

pub struct Ctx {
    pub repo: Repo,
    /// The branch HEAD names.
    pub branch: String,
    /// The commit HEAD points at (None in an empty repository).
    pub head: Option<Hash>,
    /// Root of the working copy, None when bare.
    pub worktree: Option<PathBuf>,
    pub ids: Ids,
    git_index: OnceCell<GitIndex>,
}

/// Every commit reachable from a branch, with the Git ids of mirrored ones.
#[derive(Default)]
pub struct GitIndex {
    pub to_pail: HashMap<String, Hash>,
    pub commits: std::collections::HashSet<Hash>,
}

/// Turn a plain path into a `file://` URL; leave URLs alone.
pub fn store_url(s: &str) -> Result<String> {
    if s.contains("://") {
        return Ok(s.to_string());
    }
    let p = std::path::absolute(s)?;
    Ok(format!("file://{}", p.display()))
}

impl Ctx {
    /// A bare store if one is named (flag or `PAIL_STORE`), else the working copy around `cwd`.
    pub async fn open(store: Option<String>, ids: Ids) -> Result<Ctx> {
        let store = store.or_else(|| std::env::var("PAIL_STORE").ok().filter(|s| !s.is_empty()));
        if let Some(s) = store {
            let repo = Repo::open(&store_url(&s)?)?;
            let branch = match repo.store.get_opt(STORE_HEAD_KEY).await? {
                Some((d, _)) => serde_json::from_slice::<StoreHead>(&d)?.branch,
                None => "main".to_string(),
            };
            let head = repo.read_ref(&branch).await?.map(|(h, _)| h);
            return Ok(Ctx { repo, branch, head, worktree: None, ids, git_index: OnceCell::new() });
        }
        let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
        let wt = Worktree::open(&cwd).context("not in a working copy (use --store <url> for a bare repository)")?;
        Ok(Ctx {
            repo: wt.repo,
            branch: wt.head.branch,
            head: wt.head.base,
            worktree: Some(wt.root),
            ids,
            git_index: OnceCell::new(),
        })
    }

    pub async fn commit(&self, h: &Hash) -> Result<Arc<Commit>> {
        self.repo.commit_obj(h).await
    }

    /// Git ids of every mirrored commit reachable from a branch.
    pub async fn git_index(&self) -> Result<&GitIndex> {
        self.git_index
            .get_or_try_init(|| async {
                let mut idx = GitIndex::default();
                for (_, tip) in self.repo.list_refs().await? {
                    let mut cur = Some(tip);
                    while let Some(h) = cur {
                        if !idx.commits.insert(h) {
                            break;
                        }
                        let c = self.repo.commit_obj(&h).await?;
                        if let Some(g) = c.meta.get("git.commit") {
                            idx.to_pail.insert(g.clone(), h);
                        }
                        cur = c.parent;
                    }
                }
                Ok::<_, anyhow::Error>(idx)
            })
            .await
    }

    /// The id to print for a commit.
    pub fn id(&self, h: &Hash, c: &Commit) -> String {
        match (self.ids, c.meta.get("git.commit")) {
            (Ids::Git, Some(g)) => g.clone(),
            _ => h.hex(),
        }
    }

    /// Parent ids to print. With `--ids=git`, all of the Git parents (merges keep theirs).
    pub fn parent_ids(&self, c: &Commit) -> Vec<String> {
        match (self.ids, c.meta.get("git.parents")) {
            (Ids::Git, Some(p)) => p.split_whitespace().map(str::to_string).collect(),
            _ => c.parent.map(|p| vec![p.hex()]).unwrap_or_default(),
        }
    }

    /// Resolve a revision to a commit, failing like Git when it doesn't name one.
    pub async fn resolve(&self, spec: &str) -> Result<Hash> {
        match self.try_resolve(spec).await? {
            Some(h) => Ok(h),
            None => bail!("ambiguous argument '{spec}': unknown revision or path not in the working tree."),
        }
    }

    /// Resolve a revision, or None if `spec` doesn't look like one that exists.
    pub async fn try_resolve(&self, spec: &str) -> Result<Option<Hash>> {
        let cut = spec.find(['~', '^']).unwrap_or(spec.len());
        let (base, mut rest) = spec.split_at(cut);
        let Some(mut h) = self.resolve_base(base).await? else { return Ok(None) };
        while !rest.is_empty() {
            let op = rest.as_bytes()[0];
            rest = &rest[1..];
            if op == b'^' && rest.starts_with('{') {
                let end = rest.find('}').context("unterminated ^{")?;
                match &rest[1..end] {
                    "" | "commit" => {}
                    other => bail!("{spec}: ^{{{other}}} is not supported here"),
                }
                rest = &rest[end + 1..];
                continue;
            }
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            let n: usize = if digits == 0 { 1 } else { rest[..digits].parse()? };
            rest = &rest[digits..];
            let steps = match (op, n) {
                (b'~', n) => n,
                (b'^', 0) => 0,
                (b'^', 1) => 1,
                (b'^', n) => {
                    let c = self.commit(&h).await?;
                    let parents = c.meta.get("git.parents").map(|p| p.split_whitespace().count()).unwrap_or(0);
                    if n <= parents {
                        bail!("{spec}: parent {n} is a merged side branch; pail keeps first-parent history only");
                    }
                    return Ok(None);
                }
                _ => unreachable!(),
            };
            for _ in 0..steps {
                match self.commit(&h).await?.parent {
                    Some(p) => h = p,
                    None => return Ok(None),
                }
            }
        }
        Ok(Some(h))
    }

    async fn resolve_base(&self, base: &str) -> Result<Option<Hash>> {
        if base == "HEAD" || base == "@" || base.is_empty() {
            return Ok(self.head);
        }
        let name = base.strip_prefix("refs/heads/").unwrap_or(base);
        if crate::objects::validate_branch(name).is_ok() {
            if let Some((h, _)) = self.repo.read_ref(name).await? {
                return Ok(Some(h));
            }
        }
        if base.len() < 4 || !base.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let base = base.to_ascii_lowercase();
        if base.len() == 64 {
            return Ok(Some(Hash::from_hex(&base)?));
        }
        let mut found: Vec<Hash> = vec![];
        if base.len() < 64 {
            for (k, _) in self.repo.store.list(&format!("commits/{base}")).await? {
                found.push(Hash::from_hex(k.trim_start_matches("commits/"))?);
            }
        }
        if base.len() <= 40 {
            let idx = self.git_index().await?;
            if base.len() == 40 {
                if let Some(h) = idx.to_pail.get(&base) {
                    found.push(*h);
                }
            } else {
                found.extend(idx.to_pail.iter().filter(|(g, _)| g.starts_with(&base)).map(|(_, h)| *h));
            }
        }
        found.sort();
        found.dedup();
        match found.len() {
            0 => Ok(None),
            1 => Ok(Some(found[0])),
            _ => bail!("short object ID {base} is ambiguous"),
        }
    }

    /// Git's default abbreviation: 7 characters, longer where needed to stay unique.
    pub async fn default_abbrev(&self) -> Result<usize> {
        let idx = self.git_index().await?;
        Ok(match self.ids {
            Ids::Git => Self::abbrev_len(idx.to_pail.keys().map(String::as_str), 7),
            Ids::Pail => {
                let hex: Vec<String> = idx.commits.iter().map(Hash::hex).collect();
                Self::abbrev_len(hex.iter().map(String::as_str), 7)
            }
        })
    }

    /// Branch name → tip, for decorations.
    pub async fn refs(&self) -> Result<Vec<(String, Hash)>> {
        let mut r = self.repo.list_refs().await?;
        r.sort();
        Ok(r)
    }

    /// Shortest abbreviation (at least `min`) that keeps every given id unique.
    pub fn abbrev_len<'a>(ids: impl Iterator<Item = &'a str>, min: usize) -> usize {
        let mut v: Vec<&str> = ids.collect();
        v.sort_unstable();
        let mut len = min;
        for w in v.windows(2) {
            let common = w[0].bytes().zip(w[1].bytes()).take_while(|(a, b)| a == b).count();
            len = len.max(common + 1);
        }
        len
    }
}
