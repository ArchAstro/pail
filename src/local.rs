//! A local pail repository: the directory that is to pail what `.git` is to Git. It names
//! the store and holds every local copy and index; nothing lives outside it.
//!
//! ```text
//! <repo>/                     name.pail/ when bare, <worktree>/.pail/ otherwise
//!   config                    Git-format config: core.store, core.bare, user.*, … (local layer)
//!   HEAD                      the store's default branch, remembered
//!   objects/commits|pages/    verified copies of immutable store objects
//!   blobs/<hh>/<blake3>       file contents: read cache and the source worktrees clone from
//!   graph/                    commit graph (metadata, changed paths, id index)
//!   index/gitblob.idx         Git blob ids by content hash
//!   index/gittree.idx         Git root tree ids by manifest root
//!   worktrees.json            working copies attached to this repository
//! ```
//!
//! A working copy's own state (HEAD.json, stage.json, pending/, statcache.bin) sits in its
//! `.pail/`. The first working copy's `.pail/` is the repository; a linked one
//! (`pail worktree add`) has a `repo` file naming the shared repository instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::repo::Repo;
use crate::store::Store;

pub const DOT: &str = ".pail";

#[derive(Serialize, Deserialize, Clone)]
pub struct RepoConfig {
    pub store: String,
    #[serde(default)]
    pub bare: bool,
}

#[derive(Clone)]
pub struct LocalRepo {
    pub dir: PathBuf,
    pub config: RepoConfig,
}

fn write_json<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(&tmp, p)?;
    Ok(())
}

impl LocalRepo {
    pub fn create(dir: &Path, store: &str, bare: bool) -> Result<LocalRepo> {
        std::fs::create_dir_all(dir)?;
        let dir = std::fs::canonicalize(dir)?;
        let mut f = crate::config::File::load(&dir.join("config"));
        f.set("core.store", store)?;
        f.set("core.bare", if bare { "true" } else { "false" })?;
        f.save()?;
        Ok(LocalRepo { dir, config: RepoConfig { store: store.to_string(), bare } })
    }

    pub fn open(dir: &Path) -> Result<LocalRepo> {
        let canon = std::fs::canonicalize(dir).with_context(|| format!("{} is not a pail repository", dir.display()))?;
        let f = crate::config::File::load(&canon.join("config"));
        let get = |k: &str| f.entries().into_iter().rev().find(|(key, _)| key == k).map(|(_, v)| v);
        if let Some(store) = get("core.store") {
            let bare = get("core.bare").is_some_and(|b| b == "true");
            return Ok(LocalRepo { dir: canon, config: RepoConfig { store, bare } });
        }
        // Repositories made before the Git-format config.
        let p = canon.join("config.json");
        let config: RepoConfig = serde_json::from_slice(&std::fs::read(&p).with_context(|| format!("{} is not a pail repository", dir.display()))?)?;
        Ok(LocalRepo { dir: canon, config })
    }

    /// The repository behind a working copy's `.pail/`: itself, or the one its `repo` names.
    pub fn for_worktree(dot: &Path) -> Result<LocalRepo> {
        match std::fs::read_to_string(dot.join("repo")) {
            Ok(target) => LocalRepo::open(Path::new(target.trim())),
            Err(_) => LocalRepo::open(dot),
        }
    }

    /// A store client whose caches and indexes live in this repository.
    pub fn repo(&self) -> Result<Repo> {
        Ok(Repo::with_local(Store::open(&self.config.store)?, &self.dir))
    }

    pub fn is_repo(dir: &Path) -> bool {
        let f = crate::config::File::load(&dir.join("config"));
        f.entries().iter().any(|(k, _)| k == "core.store") || dir.join("config.json").is_file()
    }

    // ---------- attached working copies ----------

    fn registry_path(&self) -> PathBuf {
        self.dir.join("worktrees.json")
    }

    pub fn worktrees(&self) -> BTreeMap<PathBuf, ()> {
        std::fs::read(self.registry_path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<PathBuf>>(&b).ok())
            .map(|v| v.into_iter().map(|p| (p, ())).collect())
            .unwrap_or_default()
    }

    fn edit_worktrees(&self, f: impl FnOnce(&mut BTreeMap<PathBuf, ()>)) -> Result<()> {
        use fs2::FileExt;
        let lock = std::fs::File::create(self.dir.join("worktrees.lock"))?;
        lock.lock_exclusive()?;
        let mut w = self.worktrees();
        f(&mut w);
        write_json(&self.registry_path(), &w.keys().collect::<Vec<_>>())
    }

    pub fn attach(&self, root: &Path) -> Result<()> {
        self.edit_worktrees(|w| {
            w.insert(root.to_path_buf(), ());
        })
    }

    pub fn detach(&self, root: &Path) -> Result<()> {
        self.edit_worktrees(|w| {
            w.remove(root);
        })
    }
}

/// Where a command runs, found from a directory upwards.
pub enum Found {
    /// A working copy: its root and its repository.
    Worktree(PathBuf, LocalRepo),
    /// A bare repository directory.
    Bare(LocalRepo),
}

pub fn discover(start: &Path) -> Result<Found> {
    let mut p = start.to_path_buf();
    loop {
        let dot = p.join(DOT);
        if dot.join("HEAD.json").is_file() {
            return Ok(Found::Worktree(p.clone(), LocalRepo::for_worktree(&dot)?));
        }
        if LocalRepo::is_repo(&p) && !p.ends_with(DOT) {
            let r = LocalRepo::open(&p)?;
            if r.config.bare {
                return Ok(Found::Bare(r));
            }
        }
        if !p.pop() {
            bail!("not in a pail repository or working copy (no {DOT}/ found; use --repo <dir> or --store <url>)");
        }
    }
}
