//! Where read commands run (a working copy or a bare store) and how revisions resolve.
//!
//! Three places to read from: a working copy (HEAD is its checked-out base), a local bare
//! repository (`pail clone --bare`, found from the current directory or `--repo`/`PAIL_DIR`),
//! or a store directly (`--store`/`PAIL_STORE`: nothing is kept locally). A bare read's HEAD
//! is the store's default branch (the `HEAD` object written by `pail init`, else `main`).
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
    /// The branch HEAD names (resolved on first use when bare).
    branch: OnceCell<String>,
    /// The commit HEAD points at; None in an empty repository (resolved on first use).
    head: OnceCell<Option<Hash>>,
    /// Root of the working copy, None when bare.
    pub worktree: Option<PathBuf>,
    /// The local repository (None when reading a store directly).
    pub local: Option<crate::local::LocalRepo>,
    /// Global config plus the repository's own.
    pub config: crate::config::Config,
    /// The store's default branch (its `HEAD`).
    default_branch: OnceCell<String>,
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
    /// A store read directly (`store`, `PAIL_STORE`), a local repository (`repo`,
    /// `PAIL_DIR`), or whatever the current directory is in.
    pub async fn open(store: Option<String>, repo: Option<PathBuf>, ids: Ids) -> Result<Ctx> {
        let store = store.or_else(|| std::env::var("PAIL_STORE").ok().filter(|s| !s.is_empty()));
        let bare = |repo: Repo, local: Option<crate::local::LocalRepo>| Ctx {
            repo,
            branch: OnceCell::new(),
            head: OnceCell::new(),
            worktree: None,
            config: crate::config::Config::load(local.as_ref().map(|l| l.dir.as_path())),
            local,
            default_branch: OnceCell::new(),
            ids,
            git_index: OnceCell::new(),
        };
        if let Some(s) = store {
            // Nothing is read until a command needs HEAD, and nothing is kept on disk.
            return Ok(bare(Repo::remote(&store_url(&s)?)?, None));
        }
        let start = match repo.or_else(|| std::env::var_os("PAIL_DIR").map(PathBuf::from)) {
            Some(d) => std::fs::canonicalize(&d).with_context(|| format!("{} not found", d.display()))?,
            None => std::fs::canonicalize(std::env::current_dir()?)?,
        };
        match crate::local::discover(&start)? {
            crate::local::Found::Bare(local) => Ok(bare(local.repo()?, Some(local))),
            crate::local::Found::Worktree(root, _) => {
                let wt = Worktree::open(&root)?;
                Ok(Ctx {
                    repo: wt.repo,
                    branch: OnceCell::new_with(Some(wt.head.branch)),
                    head: OnceCell::new_with(Some(wt.head.base)),
                    worktree: Some(wt.root),
                    config: crate::config::Config::load(Some(&wt.local.dir)),
                    local: Some(wt.local),
                    default_branch: OnceCell::new(),
                    ids,
                    git_index: OnceCell::new(),
                })
            }
        }
    }

    /// Where the store's default branch is remembered (it rarely changes, so HEAD costs one
    /// read instead of two).
    fn head_memo(&self) -> Option<PathBuf> {
        Some(self.local.as_ref()?.dir.join("HEAD"))
    }

    async fn store_default_branch(&self) -> Result<String> {
        let b = match self.repo.store.get_opt(STORE_HEAD_KEY).await? {
            Some((d, _)) => serde_json::from_slice::<StoreHead>(&d)?.branch,
            // Stores made before HEAD existed: the configured default.
            None => self.config.default_branch(),
        };
        if let Some(p) = self.head_memo() {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, &b);
        }
        Ok(b)
    }

    /// The branch HEAD names: a working copy's branch, else the store's default branch.
    pub async fn branch(&self) -> Result<&str> {
        self.branch.get_or_try_init(|| async { self.default_branch().await.map(str::to_string) }).await.map(String::as_str)
    }

    /// The store's default branch (`HEAD` in the store; remembered by a local repository):
    /// what `clone` checks out and `land` lands onto unless told otherwise.
    pub async fn default_branch(&self) -> Result<&str> {
        self.default_branch
            .get_or_try_init(|| async {
                if let Some(b) = self.head_memo().and_then(|p| std::fs::read_to_string(p).ok()).filter(|b| !b.is_empty()) {
                    return Ok(b.trim().to_string());
                }
                self.store_default_branch().await
            })
            .await
            .map(String::as_str)
    }

    /// Make `branch` the store's default branch.
    pub async fn set_default_branch(&self, branch: &str) -> Result<()> {
        crate::objects::validate_branch(branch)?;
        if self.repo.read_ref(branch).await?.is_none() {
            bail!("branch {branch} not found");
        }
        let head = serde_json::to_vec(&StoreHead { branch: branch.to_string() })?;
        self.repo.store.put(STORE_HEAD_KEY, head.into()).await?;
        if let Some(p) = self.head_memo() {
            let _ = std::fs::write(p, branch);
        }
        Ok(())
    }

    /// The commit HEAD points at (None in an empty repository).
    pub async fn head(&self) -> Result<Option<Hash>> {
        self.head
            .get_or_try_init(|| async {
                let b = self.branch().await?.to_string();
                if let Some((h, _)) = self.repo.read_ref(&b).await? {
                    return Ok(Some(h));
                }
                // The remembered default branch may have moved: ask the store once more.
                let fresh = self.store_default_branch().await?;
                if fresh != b {
                    return Ok(self.repo.read_ref(&fresh).await?.map(|(h, _)| h));
                }
                Ok(None)
            })
            .await
            .copied()
    }

    /// HEAD, or Git's error for an empty branch.
    pub async fn head_commit(&self) -> Result<Hash> {
        match self.head().await? {
            Some(h) => Ok(h),
            None => bail!("your current branch '{}' does not have any commits yet", self.branch().await?),
        }
    }

    /// Commit metadata (no change list): from the commit graph when it knows the commit.
    pub async fn commit(&self, h: &Hash) -> Result<Arc<Commit>> {
        self.repo.commit_meta(h).await
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
            return self.head().await;
        }
        // Ids the commit graph knows resolve locally, without asking the store about refs.
        let hexlike = base.len() >= 4 && base.bytes().all(|b| b.is_ascii_hexdigit());
        if hexlike {
            if let Some(h) = self.graph_lookup(&base.to_ascii_lowercase())? {
                return Ok(Some(h));
            }
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
        if self.repo.graph().is_some() {
            // Bring every branch into the graph, then look again; pail ids it still doesn't
            // know are listed from the store.
            let tips: Vec<Hash> = self.repo.list_refs().await?.into_iter().map(|(_, h)| h).collect();
            self.repo.ensure_graph(&tips).await?;
            if let Some(h) = self.graph_lookup(&base)? {
                return Ok(Some(h));
            }
            let mut found: Vec<Hash> = vec![];
            if base.len() < 64 {
                for (k, _) in self.repo.store.list(&format!("commits/{base}")).await? {
                    found.push(Hash::from_hex(k.trim_start_matches("commits/"))?);
                }
            }
            return match found.len() {
                0 => Ok(None),
                1 => Ok(Some(found[0])),
                _ => bail!("short object ID {base} is ambiguous"),
            };
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

    /// A unique commit for a hex prefix (pail or Git id) among those the graph holds.
    fn graph_lookup(&self, hex: &str) -> Result<Option<Hash>> {
        let Some(g) = self.repo.graph() else { return Ok(None) };
        let v = g.view();
        if hex.len() == 64 {
            return Ok(Hash::from_hex(hex).ok().filter(|h| v.contains(h)));
        }
        let mut found = v.pail_prefix(hex);
        if hex.len() <= 40 {
            found.extend(v.git_prefix(hex));
        }
        found.sort();
        found.dedup();
        match found.len() {
            0 => Ok(None),
            1 => Ok(Some(found[0])),
            _ => bail!("short object ID {hex} is ambiguous"),
        }
    }

    /// Git's default abbreviation: 7 characters, longer where needed to stay unique among the
    /// commits known locally (the commit graph), as Git does among its local objects.
    pub async fn default_abbrev(&self) -> Result<usize> {
        if let Some(g) = self.repo.graph() {
            let v = g.view();
            if !v.is_empty() {
                return Ok(match self.ids {
                    Ids::Git => v.abbrev_git,
                    Ids::Pail => v.abbrev_pail,
                });
            }
        }
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
