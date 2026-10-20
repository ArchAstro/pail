//! Shell completion. `pail completion <shell>` prints a small script that calls back into
//! pail (`COMPLETE=<shell> pail -- <words>`) as you type, so values come from the live
//! repository: branch names from the store, config keys, hook names.

use std::time::Duration;

use anyhow::{Context, Result};
use clap_complete::engine::CompletionCandidate;
use clap_complete::env::Shells;

/// The registration script for `shell`.
pub fn registration(shell: &str) -> Result<()> {
    let shells = Shells::builtins();
    let sh = shells.completer(shell).with_context(|| format!("unsupported shell {shell}"))?;
    // `pail` from PATH, so the script keeps working across upgrades and reinstalls.
    sh.write_registration("COMPLETE", "pail", "pail", "pail", &mut std::io::stdout())?;
    Ok(())
}

/// Branch names (with their commit), from the repository around the current directory.
/// Completion must never hang or fail the shell: a slow or unreachable store yields nothing.
pub fn branches() -> Vec<CompletionCandidate> {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return vec![] };
    rt.block_on(async {
        let found = tokio::time::timeout(Duration::from_secs(2), async {
            let cwd = std::env::current_dir().ok()?;
            let local = match pail::local::discover(&cwd).ok()? {
                pail::local::Found::Worktree(_, l) | pail::local::Found::Bare(l) => l,
            };
            local.repo().ok()?.list_refs().await.ok()
        })
        .await;
        match found {
            Ok(Some(refs)) => refs
                .into_iter()
                .map(|(name, h)| CompletionCandidate::new(name).help(Some(h.short().into())))
                .collect(),
            _ => vec![],
        }
    })
}

/// Config keys pail reads, plus any already set.
pub fn config_keys() -> Vec<CompletionCandidate> {
    let known = [
        ("user.name", "your name, for commits"),
        ("user.email", "your email, for commits"),
        ("init.defaultBranch", "default branch for `pail init`"),
        ("core.hooksPath", "hooks directory (relative to the working copy)"),
        ("core.pager", "pager for long output"),
        ("color.ui", "auto, always or never"),
        ("log.date", "date format for log"),
        ("log.abbrevCommit", "abbreviate ids in log"),
        ("log.decorate", "show branch names in log"),
        ("format.pretty", "default log format"),
    ];
    let mut out: Vec<CompletionCandidate> =
        known.iter().map(|(k, h)| CompletionCandidate::new(*k).help(Some((*h).into()))).collect();
    let dir = std::env::current_dir().ok().and_then(|d| match pail::local::discover(&d).ok()? {
        pail::local::Found::Worktree(_, l) | pail::local::Found::Bare(l) => Some(l.dir),
    });
    let cfg = pail::config::Config::load(dir.as_deref());
    for (_, k, _) in cfg.list() {
        if !known.iter().any(|(n, _)| n.eq_ignore_ascii_case(&k)) {
            out.push(CompletionCandidate::new(k));
        }
    }
    out
}
