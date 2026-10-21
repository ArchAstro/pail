//! Hooks: executables pail runs at fixed points of its own workflow, like Git's, but shaped
//! by pail's model, where a commit is published the moment it is made.
//!
//! ```text
//! pail commit    pre-commit → commit-msg <file> → [objects written] → pre-publish → CAS
//!                → post-rewrite replay (if the commit was replayed) → post-commit
//! pail land      pre-publish → CAS → post-rewrite land
//! pail switch / clone / worktree add      post-checkout <prev> <new> 1
//! pail sync                               post-sync <old> <new>
//! ```
//!
//! 1. `core.hooksPath` picks the directory (relative paths are from the working copy's root,
//!    so `.githooks`-style checked-in hooks work); default `<repo>/hooks`. Empty or
//!    `/dev/null` turns hooks off. `--no-verify` skips the hooks that can refuse.
//! 2. A hook is a file named exactly after the event and executable; anything else is
//!    ignored (so helpers like `_lib` can sit beside hooks).
//! 3. Hooks inherit the caller's environment, with the running pail's directory first on
//!    `PATH` (so `pail` in a hook is the same pail), plus informational `PAIL_HOOK_*`
//!    variables pail itself never reads. Pail exports nothing that locates a repository
//!    (Git's exported `GIT_DIR` lets a hook's test children retarget the real repository),
//!    so a hook's children cannot be redirected by it.
//! 4. A hook's stdout and stderr both go to pail's stderr. A refusing hook fails the command
//!    with exit code 3 ([`HookRefused`]); `PAIL_TRACE_HOOKS=<file>` appends one JSON line per
//!    hook run (name, args, exit code, milliseconds) for tools that need to tell a hook's
//!    refusal from any other failure.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result};

/// The exit code of a command a hook refused.
pub const REFUSED_EXIT: i32 = 3;

/// A hook exited non-zero (or was killed).
#[derive(Debug)]
pub struct HookRefused {
    pub hook: String,
    pub code: Option<i32>,
}

impl std::fmt::Display for HookRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(c) => write!(f, "{} hook refused (exit {c})", self.hook),
            None => write!(f, "{} hook was killed", self.hook),
        }
    }
}

impl std::error::Error for HookRefused {}

#[derive(Clone, Debug)]
pub struct Hooks {
    /// None: hooks are off (configured so, or `--no-verify` for refusing hooks).
    dir: Option<PathBuf>,
    /// Where hooks run: the working copy's root, or the repository for bare commands.
    cwd: PathBuf,
    /// `PAIL_HOOK_*` context added to every hook's environment.
    env: Vec<(String, String)>,
}

impl Hooks {
    /// Hooks for a working copy (`root`) or a bare repository (`root` = the repository).
    pub fn new(cfg: &crate::config::Config, repo_dir: &Path, root: &Path) -> Hooks {
        let dir = match cfg.get("core.hookspath") {
            Some(p) if p.is_empty() || p == "/dev/null" => None,
            Some(p) => Some(root.join(expand_home(&p))),
            None => Some(repo_dir.join("hooks")),
        };
        Hooks {
            dir,
            cwd: root.to_path_buf(),
            env: vec![],
        }
    }

    /// No hooks at all.
    pub fn off(root: &Path) -> Hooks {
        Hooks {
            dir: None,
            cwd: root.to_path_buf(),
            env: vec![],
        }
    }

    pub fn with_env(mut self, k: &str, v: impl Into<String>) -> Hooks {
        self.env.push((format!("PAIL_HOOK_{k}"), v.into()));
        self
    }

    /// The executable for `name`, if there is one.
    pub fn find(&self, name: &str) -> Option<PathBuf> {
        let p = self.dir.as_ref()?.join(name);
        let md = std::fs::metadata(&p).ok()?;
        (md.is_file() && md.permissions().mode() & 0o111 != 0).then_some(p)
    }

    /// Run `name` if it exists. Ok(false): no such hook.
    pub fn run(&self, name: &str, args: &[&str], stdin: Option<&[u8]>) -> Result<bool> {
        let Some(exe) = self.find(name) else {
            return Ok(false);
        };
        let started = Instant::now();
        // Both of the hook's streams go to our stderr: stdout stays the command's own.
        let err = std::io::stderr().as_fd().try_clone_to_owned()?;
        let mut cmd = Command::new(&exe);
        // `pail` inside a hook is the pail running it (Git puts its exec-path first likewise).
        if let Some(bin) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf))
        {
            let mut paths = vec![bin];
            paths.extend(
                std::env::var_os("PATH")
                    .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
                    .unwrap_or_default(),
            );
            if let Ok(joined) = std::env::join_paths(paths) {
                cmd.env("PATH", joined);
            }
        }
        cmd.args(args)
            .current_dir(&self.cwd)
            .env("PAIL_HOOK", name)
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::from(err.try_clone()?))
            .stderr(Stdio::from(err));
        let mut child = cmd
            .spawn()
            .with_context(|| format!("running the {name} hook ({})", exe.display()))?;
        if let Some(data) = stdin {
            let mut pipe = child.stdin.take().expect("piped stdin");
            // A hook may exit without reading its input; that is not an error.
            let _ = pipe.write_all(data);
        }
        let status = child.wait()?;
        trace(name, args, status.code(), started.elapsed().as_millis());
        if status.success() {
            Ok(true)
        } else {
            Err(HookRefused {
                hook: name.to_string(),
                code: status.code(),
            }
            .into())
        }
    }

    /// Run a hook whose outcome cannot undo anything (post-*): a failure is reported, not raised.
    pub fn notify(&self, name: &str, args: &[&str], stdin: Option<&[u8]>) {
        if let Err(e) = self.run(name, args, stdin) {
            eprintln!("warning: {e}");
        }
    }
}

use std::os::fd::AsFd;

fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

/// One JSON line per hook run to `$PAIL_TRACE_HOOKS`, when set.
fn trace(name: &str, args: &[&str], code: Option<i32>, ms: u128) {
    let Some(path) = std::env::var_os("PAIL_TRACE_HOOKS") else {
        return;
    };
    let line = serde_json::json!({"hook": name, "args": args, "exit": code, "ms": ms as u64});
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// A commit id for hook arguments; all zeros for "none" (Git's convention).
pub fn id(h: Option<crate::hash::Hash>) -> String {
    h.map(|h| h.hex()).unwrap_or_else(|| "0".repeat(64))
}
