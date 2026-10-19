use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use pail::hash::Hash;
use pail::objects::now_date;
use pail::repo::{CommitRequest, Delta, Repo};
use pail::worktree::{Worktree, author};

#[derive(Parser)]
#[command(name = "pail", version, about = "Version control that lives in a bucket")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a repository in a store (s3://, gs://, file://) with an empty `main`.
    Init { store: String, #[arg(long, default_value = "main")] branch: String },
    /// Check out a branch into a new directory.
    Clone { store: String, dir: Option<PathBuf>, #[arg(short, long, default_value = "main")] branch: String },
    /// Show staged, unstaged and untracked changes.
    Status,
    /// Stage changes (`-A` for everything).
    Add { paths: Vec<String>, #[arg(short = 'A', long)] all: bool },
    /// Remove paths from the stage.
    Unstage { paths: Vec<String> },
    /// Discard unstaged changes to paths.
    Restore { paths: Vec<String> },
    /// Commit the stage (`-a` stages tracked changes first).
    Commit { #[arg(short, long)] message: String, #[arg(short, long)] all: bool },
    /// Bring the working copy up to date with its branch.
    Sync,
    /// List branches, or create one at the current base.
    Branch { name: Option<String>, #[arg(short, long)] delete: bool },
    /// Switch to another branch (`-c` creates it at the current base).
    Switch { branch: String, #[arg(short, long)] create: bool },
    /// Squash a branch's changes onto another branch.
    Land { branch: String, #[arg(long, default_value = "main")] onto: String, #[arg(short, long)] message: Option<String> },
    /// Show history.
    Log { #[arg(short = 'n', default_value_t = 20)] n: usize, branch: Option<String> },
    /// Paths changed between two commits, or base→stage (`--staged`).
    Diff { from: Option<String>, to: Option<String>, #[arg(long)] staged: bool },
    /// List files in a commit.
    Ls { rev: Option<String> },
    /// Delete unreachable objects older than the grace period.
    Gc { store: Option<String>, #[arg(long, default_value_t = 168.0)] grace_hours: f64, #[arg(long)] dry_run: bool },
    /// Print a file from a commit.
    Cat { path: String, #[arg(long)] rev: Option<String> },
}

fn cwd() -> Result<PathBuf> {
    Ok(std::fs::canonicalize(std::env::current_dir()?)?)
}

/// Resolve a branch name, commit hash or unique hash prefix.
async fn resolve_rev(repo: &Repo, rev: &str) -> Result<Hash> {
    if let Some((h, _)) = repo.read_ref(rev).await? {
        return Ok(h);
    }
    if rev.len() == 64 {
        return Hash::from_hex(rev);
    }
    let keys = repo.store.list(&format!("commits/{rev}")).await?;
    match keys.as_slice() {
        [(k, _)] => Hash::from_hex(k.trim_start_matches("commits/")),
        [] => bail!("unknown revision {rev}"),
        _ => bail!("ambiguous revision {rev}"),
    }
}

fn fmt_bytes(n: u64) -> String {
    let n = n as f64;
    if n >= 1e9 { format!("{:.2} GB", n / 1e9) } else if n >= 1e6 { format!("{:.1} MB", n / 1e6) } else { format!("{:.0} KB", n / 1e3) }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Init { store, branch } => {
            let repo = Repo::open(&store)?;
            if repo.read_ref(&branch).await?.is_some() {
                bail!("{store} already has branch {branch}");
            }
            let who = author();
            let date = now_date();
            let out = repo
                .commit(CommitRequest {
                    branch: branch.clone(),
                    base: None,
                    delta: Delta::new(),
                    author: who.clone(),
                    author_date: date.clone(),
                    committer: who,
                    committer_date: date,
                    message: "Initial empty commit".into(),
                    meta: BTreeMap::new(),
                })
                .await?;
            println!("initialized {store} ({branch} @ {})", out.commit.short());
        }
        Cmd::Clone { store, dir, branch } => {
            let dir = dir.unwrap_or_else(|| {
                PathBuf::from(store.trim_end_matches('/').rsplit('/').next().unwrap_or("repo"))
            });
            let t = std::time::Instant::now();
            let wt = Worktree::clone(&store, &dir, &branch).await?;
            let c = wt.repo.commit_obj(&wt.head.base.unwrap()).await?;
            let (gets, _, down, _) = wt.repo.store.stats.snapshot();
            println!(
                "cloned {branch} @ {} into {}: {} files, {} in {:.1}s ({gets} requests, {} transferred)",
                wt.head.base.unwrap().short(),
                dir.display(),
                c.files,
                fmt_bytes(c.bytes),
                t.elapsed().as_secs_f64(),
                fmt_bytes(down)
            );
        }
        Cmd::Status => {
            let wt = Worktree::open(&cwd()?)?;
            let st = wt.status().await?;
            println!(
                "on {} @ {}",
                wt.head.branch,
                wt.head.base.map(|h| h.short()).unwrap_or_else(|| "(empty)".into())
            );
            if st.is_clean() && st.untracked.is_empty() {
                println!("nothing to commit, working copy clean");
            }
            if !st.staged.is_empty() {
                println!("\nstaged:");
                st.staged.iter().for_each(|(p, c)| println!("  {c} {p}"));
            }
            if !st.unstaged.is_empty() {
                println!("\nnot staged:");
                st.unstaged.iter().for_each(|(p, c)| println!("  {c} {p}"));
            }
            if !st.untracked.is_empty() {
                println!("\nuntracked:");
                st.untracked.iter().for_each(|p| println!("  ? {p}"));
            }
        }
        Cmd::Add { paths, all } => {
            let mut wt = Worktree::open(&cwd()?)?;
            let spec = if all || paths.is_empty() { vec![String::new()] } else { wt.resolve(&cwd()?, &paths)? };
            if !all && paths.is_empty() {
                bail!("nothing specified (use `pail add <path>` or `pail add -A`)");
            }
            let n = wt.add(&spec, false).await?;
            println!("staged {n} change(s)");
        }
        Cmd::Unstage { paths } => {
            let mut wt = Worktree::open(&cwd()?)?;
            let spec = if paths.is_empty() { vec![String::new()] } else { wt.resolve(&cwd()?, &paths)? };
            println!("unstaged {} path(s)", wt.unstage(&spec)?);
        }
        Cmd::Restore { paths } => {
            let wt = Worktree::open(&cwd()?)?;
            if paths.is_empty() {
                bail!("name the paths to restore (use `.` for everything)");
            }
            let spec = wt.resolve(&cwd()?, &paths)?;
            println!("restored {} path(s)", wt.restore(&spec).await?);
        }
        Cmd::Commit { message, all } => {
            let mut wt = Worktree::open(&cwd()?)?;
            if all {
                wt.add(&[String::new()], true).await?;
            }
            let out = wt.commit(&message).await?;
            let retried = if out.attempts > 1 { format!(", landed after {} attempts", out.attempts) } else { String::new() };
            println!("[{} {}] {}{retried}", wt.head.branch, out.commit.short(), message.lines().next().unwrap_or(""));
        }
        Cmd::Sync => {
            let mut wt = Worktree::open(&cwd()?)?;
            let n = wt.sync().await?;
            println!("{} @ {} ({n} path(s) updated)", wt.head.branch, wt.head.base.unwrap().short());
        }
        Cmd::Branch { name, delete } => {
            let here = Worktree::open(&cwd()?).ok();
            match (name, here) {
                (Some(n), _) if delete => {
                    let wt_repo = match Worktree::open(&cwd()?) {
                        Ok(w) => w.repo,
                        Err(e) => return Err(e),
                    };
                    wt_repo.delete_branch(&n).await?;
                    println!("deleted {n}");
                }
                (Some(n), Some(wt)) => {
                    let base = wt.head.base.context("nothing to branch from")?;
                    wt.repo.create_branch(&n, base).await?;
                    println!("created {n} @ {}", base.short());
                }
                (None, Some(wt)) => {
                    for (b, h) in wt.repo.list_refs().await? {
                        let mark = if b == wt.head.branch { "*" } else { " " };
                        println!("{mark} {b:30} {}", h.short());
                    }
                }
                _ => bail!("run inside a working copy"),
            }
        }
        Cmd::Switch { branch, create } => {
            let mut wt = Worktree::open(&cwd()?)?;
            if create {
                let base = wt.head.base.context("nothing to branch from")?;
                wt.repo.create_branch(&branch, base).await?;
            }
            let n = wt.switch(&branch).await?;
            println!("switched to {branch} @ {} ({n} path(s) changed)", wt.head.base.unwrap().short());
        }
        Cmd::Land { branch, onto, message } => {
            let wt = Worktree::open(&cwd()?)?;
            let repo = &wt.repo;
            let (src, _) = repo.read_ref(&branch).await?.with_context(|| format!("no branch {branch}"))?;
            let (dst, _) = repo.read_ref(&onto).await?.with_context(|| format!("no branch {onto}"))?;
            let anc = repo.merge_base(src, dst).await?.context("branches share no history")?;
            let from = repo.root_of(Some(anc)).await?;
            let to = repo.root_of(Some(src)).await?;
            let delta: Delta = repo.diff(from, to).await?.into_iter().map(|c| (c.path, c.new)).collect();
            if delta.is_empty() {
                println!("{branch} has nothing to land");
                return Ok(());
            }
            let subjects: Vec<String> = {
                let mut v = vec![];
                let mut cur = Some(src);
                while let Some(h) = cur.filter(|h| *h != anc) {
                    let c = repo.commit_obj(&h).await?;
                    v.push(format!("* {}", c.subject()));
                    cur = c.parent;
                }
                v
            };
            let msg = message.unwrap_or_else(|| format!("Land {branch}\n\n{}", subjects.join("\n")));
            let who = author();
            let date = now_date();
            let out = repo
                .commit(CommitRequest {
                    branch: onto.clone(),
                    base: Some(anc),
                    delta,
                    author: who.clone(),
                    author_date: date.clone(),
                    committer: who,
                    committer_date: date,
                    message: msg,
                    meta: BTreeMap::from([("landed.from".into(), branch.clone())]),
                })
                .await?;
            println!("landed {branch} onto {onto} @ {}", out.commit.short());
        }
        Cmd::Log { n, branch } => {
            let wt = Worktree::open(&cwd()?)?;
            let start = match branch {
                Some(b) => resolve_rev(&wt.repo, &b).await?,
                None => wt.head.base.context("empty history")?,
            };
            for (h, c) in wt.repo.log(start, n).await? {
                let git = c.meta.get("git.commit").map(|g| format!(" (git {})", &g[..10])).unwrap_or_default();
                println!("{} {}{git}  {}", h.short(), c.author.split(" <").next().unwrap_or(""), c.subject());
            }
        }
        Cmd::Diff { from, to, staged } => {
            let wt = Worktree::open(&cwd()?)?;
            if staged {
                for (p, s) in &wt.stage {
                    println!("{} {p}", if matches!(s, pail::worktree::Staged::Del) { 'D' } else { 'M' });
                }
                return Ok(());
            }
            let a = match from {
                Some(r) => resolve_rev(&wt.repo, &r).await?,
                None => wt.head.base.context("empty history")?,
            };
            let b = match to {
                Some(r) => Some(resolve_rev(&wt.repo, &r).await?),
                None => None,
            };
            let changes = match b {
                Some(b) => {
                    let (ra, rb) = (wt.repo.root_of(Some(a)).await?, wt.repo.root_of(Some(b)).await?);
                    wt.repo.diff(ra, rb).await?
                }
                None => {
                    let c = wt.repo.commit_obj(&a).await?;
                    let p = wt.repo.root_of(c.parent).await?;
                    wt.repo.diff(p, c.root).await?
                }
            };
            for c in changes {
                let code = match (&c.old, &c.new) {
                    (None, _) => 'A',
                    (_, None) => 'D',
                    _ => 'M',
                };
                println!("{code} {}", c.path);
            }
        }
        Cmd::Ls { rev } => {
            let wt = Worktree::open(&cwd()?)?;
            let h = match rev {
                Some(r) => resolve_rev(&wt.repo, &r).await?,
                None => wt.head.base.context("empty history")?,
            };
            let root = wt.repo.root_of(Some(h)).await?;
            for e in wt.repo.entries(root).await? {
                println!("{:?}\t{}\t{}", e.kind, e.size, e.path);
            }
        }
        Cmd::Gc { store, grace_hours, dry_run } => {
            let repo = match store {
                Some(s) => Repo::open(&s)?,
                None => Worktree::open(&cwd()?)?.repo,
            };
            let r = pail::gc::gc(&repo, (grace_hours * 3600.0) as i64, dry_run).await?;
            println!(
                "live: {} commits, {} pages, {} packs, {} chunks; {} {} object(s), {}; kept {} young unreferenced",
                r.live_commits,
                r.live_pages,
                r.live_packs,
                r.live_chunks,
                if dry_run { "would delete" } else { "deleted" },
                r.deleted.len(),
                fmt_bytes(r.bytes_freed),
                r.kept_young
            );
        }
        Cmd::Cat { path, rev } => {
            let wt = Worktree::open(&cwd()?)?;
            let h = match rev {
                Some(r) => resolve_rev(&wt.repo, &r).await?,
                None => wt.head.base.context("empty history")?,
            };
            let root = wt.repo.root_of(Some(h)).await?;
            let e = wt.repo.get_entry(root, &path).await?.with_context(|| format!("{path} not in {}", h.short()))?;
            let data = wt.repo.read_files(std::slice::from_ref(&e)).await?;
            use std::io::Write;
            std::io::stdout().write_all(&data[0])?;
        }
    }
    Ok(())
}
