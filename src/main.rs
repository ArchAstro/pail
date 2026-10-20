use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use pail::hash::Hash;
use pail::objects::now_date;
use pail::repo::{CommitRequest, Delta, Repo};
use pail::rev::{Ctx, Ids};
use pail::local::LocalRepo;
use pail::worktree::{Checkout, Worktree, author, seed_ignored};

#[derive(Parser)]
#[command(name = "pail", version, about = "Version control that lives in a bucket")]
struct Cli {
    /// Run against a store (bare repository) instead of the working copy: s3://, gs://,
    /// file:// or a directory path. Also `PAIL_STORE`.
    #[arg(long)]
    store: Option<String>,
    /// Use this local repository (a bare `*.pail` directory or a working copy), as
    /// `git --git-dir`. Also `PAIL_DIR`.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Commit ids to print: pail's, or the Git ids of mirrored commits.
    #[arg(long, value_enum, default_value = "pail")]
    ids: IdsArg,
    /// Don't pipe output into a pager.
    #[arg(long, short = 'P')]
    no_pager: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum IdsArg {
    Pail,
    Git,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a repository in a store (s3://, gs://, file://) with an empty `main`.
    Init { store: String, #[arg(long, default_value = "main")] branch: String },
    /// Check out a branch into a new directory.
    Clone {
        store: String,
        dir: Option<PathBuf>,
        #[arg(short, long, default_value = "main")]
        branch: String,
        /// Create a bare repository (no working copy): local caches and indexes for reading
        /// the store, and a home for linked worktrees.
        #[arg(long)]
        bare: bool,
    },
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
    /// Show history, with `git log`'s options and output.
    #[command(disable_help_flag = true)]
    Log {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Show commits (with patches), trees or file contents, as `git show`.
    #[command(disable_help_flag = true)]
    Show {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Object contents, type or size, as `git cat-file` (`--batch` reads names from stdin).
    #[command(name = "cat-file", disable_help_flag = true)]
    CatFile {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// List a tree with Git modes and object ids, as `git ls-tree`.
    #[command(name = "ls-tree", disable_help_flag = true)]
    LsTree {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Resolve revisions and object names to ids, as `git rev-parse`.
    #[command(name = "rev-parse", disable_help_flag = true)]
    RevParse {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Paths changed between two commits, or base→stage (`--staged`).
    Diff { from: Option<String>, to: Option<String>, #[arg(long)] staged: bool },
    /// List a directory of a commit (`-r` for everything under it, `-l` for sizes).
    Ls {
        /// `<rev>`, `<rev>:<dir>` or a directory of HEAD.
        target: Option<String>,
        #[arg(short, long)]
        recursive: bool,
        #[arg(short, long)]
        long: bool,
    },
    /// Delete unreachable objects older than the grace period.
    Gc { store: Option<String>, #[arg(long, default_value_t = 168.0)] grace_hours: f64, #[arg(long)] dry_run: bool },
    /// Fill the local content cache, as `git fetch` fills a bare repository: every file of
    /// the given commits (default HEAD), or with --history every version along their history.
    Fetch {
        revs: Vec<String>,
        /// Also every file version introduced along the history of the revisions.
        #[arg(long)]
        history: bool,
        /// Stop the history walk after this many commits.
        #[arg(short = 'n', long)]
        max_count: Option<usize>,
        /// Every branch.
        #[arg(long)]
        all: bool,
    },
    /// Inspect or trim the local content cache.
    #[command(subcommand)]
    Cache(CacheCmd),
    /// Extra working copies that share file contents and build output copy-on-write.
    #[command(subcommand)]
    Worktree(WtCmd),
    /// Print a file from a commit: `<path>` (HEAD) or `<rev>:<path>`.
    Cat { path: String, #[arg(long)] rev: Option<String> },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Where the caches live and how much they hold.
    Stats,
    /// Delete least recently used file contents until the cache fits (e.g. `10G`, `500M`).
    Prune {
        #[arg(long)]
        max_size: String,
    },
}

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        Some('T') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    Ok((num.trim().parse::<f64>().with_context(|| format!("bad size {s}"))? * mult as f64) as u64)
}

fn dir_size(p: &std::path::Path) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    for e in ignore::WalkBuilder::new(p).standard_filters(false).build().flatten() {
        if let Ok(md) = e.metadata() {
            if md.is_file() {
                files += 1;
                bytes += md.len();
            }
        }
    }
    (files, bytes)
}

#[derive(Subcommand)]
enum WtCmd {
    /// Add a working copy of this one's base (or another branch), seeded with its build output.
    Add {
        path: PathBuf,
        /// Create this branch at the current base and check it out.
        #[arg(short = 'b', long = "create")]
        create: Option<String>,
        /// Check out this existing branch at its head (default: this working copy's branch and base).
        #[arg(long)]
        branch: Option<String>,
        /// Clone ignored files (build output, dependencies) from this directory (default: here).
        #[arg(long)]
        seed_from: Option<PathBuf>,
        /// Don't seed ignored files.
        #[arg(long)]
        no_seed: bool,
    },
    /// List working copies of this store on this machine.
    List,
    /// Delete a working copy (refuses if it has uncommitted changes unless --force).
    Remove { path: PathBuf, #[arg(long)] force: bool },
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

fn checkout_summary(co: &Checkout) -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let s = &co.stats;
    format!(
        "{} cloned, {} written",
        fmt_bytes(s.cloned_bytes.load(Relaxed)),
        fmt_bytes(s.copied_bytes.load(Relaxed))
    )
}

fn fmt_bytes(n: u64) -> String {
    let n = n as f64;
    if n >= 1e9 { format!("{:.2} GB", n / 1e9) } else if n >= 1e6 { format!("{:.1} MB", n / 1e6) } else { format!("{:.0} KB", n / 1e3) }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
        }
        if let Some(f) = e.downcast_ref::<pail::gitlog::Fatal>() {
            eprintln!("fatal: {f}");
            std::process::exit(128);
        }
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Where command output goes: a pager when stdout is a terminal (as Git does), else stdout.
struct Output {
    w: Box<dyn std::io::Write>,
    pager: Option<std::process::Child>,
    tty: bool,
}

impl Output {
    fn open(no_pager: bool) -> Output {
        use std::io::IsTerminal;
        let tty = std::io::stdout().is_terminal();
        let pager = ["PAIL_PAGER", "GIT_PAGER", "PAGER"].iter().find_map(|k| std::env::var(k).ok()).unwrap_or_else(|| "less".into());
        if tty && !no_pager && !pager.is_empty() && pager != "cat" {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", &pager]).stdin(std::process::Stdio::piped());
            if std::env::var_os("LESS").is_none() {
                cmd.env("LESS", "FRX");
            }
            if std::env::var_os("LV").is_none() {
                cmd.env("LV", "-c");
            }
            if let Ok(mut child) = cmd.spawn() {
                let stdin = child.stdin.take().unwrap();
                return Output { w: Box::new(std::io::BufWriter::new(stdin)), pager: Some(child), tty };
            }
        }
        Output { w: Box::new(std::io::BufWriter::new(std::io::stdout())), pager: None, tty }
    }

    fn finish(mut self) -> Result<()> {
        let flushed = self.w.flush();
        drop(self.w);
        if let Some(mut p) = self.pager.take() {
            let _ = p.wait();
        }
        flushed.or_else(|e| if e.kind() == std::io::ErrorKind::BrokenPipe { Ok(()) } else { Err(e) })?;
        Ok(())
    }
}

/// Finish a command: a Git `fatal:` goes to stderr before buffered stdout is flushed, as
/// Git's own output ordering has it.
fn done(out: Output, r: Result<()>) -> Result<()> {
    if let Err(e) = &r {
        if let Some(f) = e.downcast_ref::<pail::gitlog::Fatal>() {
            eprintln!("fatal: {f}");
            let _ = out.finish();
            std::process::exit(128);
        }
    }
    let f = out.finish();
    r?;
    f
}

/// `git log`: split words into revisions and paths the way Git does, then walk and print.
async fn cmd_log(ctx: &Ctx, args: &[String], out: &mut Output) -> Result<()> {
    use pail::gitlog::{Fatal, LogOpts, Pathspec, Printer, walk};
    let mut o = LogOpts::parse(args)?;
    let words = std::mem::take(&mut o.revs);
    let mut paths = vec![];
    for w in words {
        if !paths.is_empty() {
            paths.push(w);
            continue;
        }
        let is_rev = w.contains("..") || w.starts_with('^') || ctx.try_resolve(&w).await.unwrap_or(None).is_some();
        if is_rev {
            o.revs.push(w);
            continue;
        }
        let exists = match ctx.head().await? {
            Some(h) => pail::snapshot::Snapshot::open(&ctx.repo, h).await?.stat(&w).await?.is_some(),
            None => false,
        };
        if !exists {
            return Err(Fatal(format!(
                "ambiguous argument '{w}': unknown revision or path not in the working tree.\nUse '--' to separate paths from revisions, like this:\n'git <command> [<revision>...] -- [<file>...]'"
            ))
            .into());
        }
        paths.push(w);
    }
    paths.append(&mut o.paths);
    // Pathspecs are relative to the current directory inside a working copy.
    if let Some(root) = &ctx.worktree {
        let rel = cwd()?.strip_prefix(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        if !rel.is_empty() {
            paths = paths.into_iter().map(|p| format!("{rel}/{p}")).collect();
        }
    }
    o.paths = paths;
    let spec = Pathspec::new(&o.paths)?;
    let commits = walk(ctx, &o, &spec, o.diff.any_output()).await?;
    let any_merge = commits.iter().any(|c| c.commit.meta.get("git.parents").is_some_and(|p| p.contains(' ')));
    let mut printer = Printer::new(ctx, &o, out.tty, any_merge).await?;
    // Contents for diffs are fetched 64 commits at a time in one batched read.
    for chunk in commits.chunks(64) {
        printer.diff.prefetch(chunk).await?;
        for c in chunk {
            printer.show(c, &mut *out.w).await?;
        }
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    let ids = match cli.ids {
        IdsArg::Pail => Ids::Pail,
        IdsArg::Git => Ids::Git,
    };
    match cli.cmd {
        Cmd::Log { args } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let mut out = Output::open(cli.no_pager);
            let r = cmd_log(&ctx, &args, &mut out).await;
            done(out, r)?;
        }
        Cmd::Show { args } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let mut out = Output::open(cli.no_pager);
            let r = pail::gitcmd::show(&ctx, &args, &mut *out.w, out.tty).await;
            done(out, r)?;
        }
        Cmd::CatFile { args } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let mut out = Output::open(true);
            let r = pail::gitcmd::cat_file(&ctx, &args, &mut *out.w).await;
            done(out, r)?;
        }
        Cmd::LsTree { args } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let mut out = Output::open(true);
            let r = pail::gitcmd::ls_tree(&ctx, &args, &mut *out.w).await;
            done(out, r)?;
        }
        Cmd::RevParse { args } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let mut out = Output::open(true);
            let r = pail::gitcmd::rev_parse(&ctx, &args, &mut *out.w).await;
            done(out, r)?;
        }
        Cmd::Init { store, branch } => {
            let store = pail::rev::store_url(&store)?;
            let repo = Repo::remote(&store)?;
            if repo.read_ref(&branch).await?.is_some() {
                bail!("{store} already has branch {branch}");
            }
            // The default branch a bare read (`--store`) starts from.
            let head = serde_json::to_vec(&pail::rev::StoreHead { branch: branch.clone() })?;
            repo.store.put(pail::rev::STORE_HEAD_KEY, head.into()).await?;
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
        Cmd::Clone { store, dir, branch, bare } => {
            let store = pail::rev::store_url(&store)?;
            let name = store.trim_end_matches('/').rsplit('/').next().unwrap_or("repo").to_string();
            if bare {
                let dir = dir.unwrap_or_else(|| PathBuf::from(format!("{name}.pail")));
                if dir.exists() && std::fs::read_dir(&dir)?.next().is_some() {
                    bail!("{} exists and is not empty", dir.display());
                }
                let local = LocalRepo::create(&dir, &store, true)?;
                let repo = local.repo()?;
                let tip = repo.read_ref(&branch).await?;
                println!(
                    "bare repository {} for {store}{}",
                    local.dir.display(),
                    tip.map(|(h, _)| format!(" ({branch} @ {})", h.short())).unwrap_or_default()
                );
                return Ok(());
            }
            let dir = dir.unwrap_or_else(|| PathBuf::from(&name));
            let t = std::time::Instant::now();
            let mut co = Checkout::default();
            let wt = Worktree::clone(&store, None, &dir, &branch, None, &mut co).await?;
            let c = wt.repo.commit_obj(&wt.head.base.unwrap()).await?;
            let (gets, _, down, _) = wt.repo.store.stats.snapshot();
            println!(
                "cloned {branch} @ {} into {}: {} files, {} in {:.1}s ({gets} requests, {} transferred; {})",
                wt.head.base.unwrap().short(),
                dir.display(),
                c.files,
                fmt_bytes(c.bytes),
                t.elapsed().as_secs_f64(),
                fmt_bytes(down),
                checkout_summary(&co)
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
        Cmd::Ls { target, recursive, long } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let (rev, dir) = match target.as_deref() {
                Some(t) if t.contains(':') => t.split_once(':').map(|(r, d)| (r.to_string(), d.to_string())).unwrap(),
                Some(t) if ctx.try_resolve(t).await.unwrap_or(None).is_some() => (t.to_string(), String::new()),
                Some(t) => ("HEAD".into(), t.to_string()),
                None => ("HEAD".into(), String::new()),
            };
            let h = if rev.is_empty() { ctx.head_commit().await? } else { ctx.resolve(&rev).await? };
            let snap = pail::snapshot::Snapshot::open(&ctx.repo, h).await?;
            let dir = dir.trim_matches('/');
            let mut out = Output::open(cli.no_pager);
            let kind = |k: &pail::snapshot::NodeKind| match k {
                pail::snapshot::NodeKind::File => "file",
                pail::snapshot::NodeKind::Exec => "exec",
                pail::snapshot::NodeKind::Symlink => "link",
                pail::snapshot::NodeKind::Dir => "dir",
            };
            let r: Result<()> = async {
                use std::io::Write;
                if recursive {
                    for e in snap.walk(dir).await? {
                        if long {
                            writeln!(out.w, "{:4} {:>12}  {}", kind(&e.kind.into()), e.size, e.path)?;
                        } else {
                            writeln!(out.w, "{}", e.path)?;
                        }
                    }
                } else {
                    for n in snap.list_dir(dir).await? {
                        let slash = if n.kind == pail::snapshot::NodeKind::Dir { "/" } else { "" };
                        if long {
                            writeln!(out.w, "{:4} {:>12}  {}{slash}", kind(&n.kind), n.size, n.name())?;
                        } else {
                            writeln!(out.w, "{}{slash}", n.name())?;
                        }
                    }
                }
                Ok(())
            }
            .await;
            let f = out.finish();
            r?;
            f?;
        }
        Cmd::Gc { store, grace_hours, dry_run } => {
            let repo = match store {
                Some(s) => Repo::remote(&pail::rev::store_url(&s)?)?,
                None => Ctx::open(None, cli.repo, ids).await?.repo,
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
        Cmd::Fetch { revs, history, max_count, all } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let blobs = ctx.repo.blobs.as_ref().context("fetch fills a local repository; make one with `pail clone --bare <store>`")?;
            let t = std::time::Instant::now();
            let mut tips = vec![];
            for r in &revs {
                tips.push(ctx.resolve(r).await?);
            }
            if all {
                tips.extend(ctx.refs().await?.into_iter().map(|(_, h)| h));
            }
            if tips.is_empty() {
                tips.push(ctx.head_commit().await?);
            }
            // Snapshots of the tips, plus every version introduced along their history.
            let mut want: std::collections::HashMap<Hash, pail::manifest::Entry> = Default::default();
            let mut commits = 0;
            let mut seen = std::collections::HashSet::new();
            for tip in &tips {
                let root = ctx.repo.root_of(Some(*tip)).await?;
                for e in ctx.repo.entries(root).await? {
                    want.entry(e.hash).or_insert(e);
                }
                if history {
                    let mut cur = Some(*tip);
                    while let Some(h) = cur {
                        if max_count.is_some_and(|m| commits >= m) || !seen.insert(h) {
                            break;
                        }
                        commits += 1;
                        let c = ctx.repo.commit_obj(&h).await?;
                        let changes = match c.change_list() {
                            Some(cs) => cs,
                            None => {
                                let from = ctx.repo.root_of(c.parent).await?;
                                ctx.repo.diff(from, c.root).await?
                            }
                        };
                        for ch in changes {
                            if let Some(e) = ch.new {
                                want.entry(e.hash).or_insert(e);
                            }
                        }
                        cur = c.parent;
                    }
                }
            }
            let missing: Vec<pail::manifest::Entry> = want
                .into_values()
                .filter(|e| !matches!(e.loc, pail::manifest::Loc::Inline(_)) && !blobs.has(&e.hash))
                .collect();
            let total: u64 = missing.iter().map(|e| e.size).sum();
            let mut start = 0;
            while start < missing.len() {
                let mut end = start;
                let mut bytes = 0;
                while end < missing.len() && (end == start || bytes < 256 << 20) {
                    bytes += missing[end].size;
                    end += 1;
                }
                ctx.repo.read_files(&missing[start..end]).await?;
                start = end;
            }
            let (_, _, down, _) = ctx.repo.store.stats.snapshot();
            println!(
                "fetched {} file version(s), {} ({} transferred) in {:.1}s{}",
                missing.len(),
                fmt_bytes(total),
                fmt_bytes(down),
                t.elapsed().as_secs_f64(),
                if history { format!(" across {commits} commit(s)") } else { String::new() }
            );
        }
        Cmd::Cache(CacheCmd::Stats) => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let local = ctx.local.as_ref().context("not in a local repository")?;
            println!("repository {} ({})", local.dir.display(), local.config.store);
            for (name, what) in [("blobs", "file contents"), ("objects", "commits, pages"), ("graph", "commit graph"), ("index", "Git ids")] {
                let (n, bytes) = dir_size(&local.dir.join(name));
                println!("  {name:8} {:>9} files  {:>10}  {what}", n, fmt_bytes(bytes));
            }
        }
        Cmd::Cache(CacheCmd::Prune { max_size }) => {
            let max = parse_size(&max_size)?;
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let b = ctx.repo.blobs.as_ref().context("not in a local repository")?;
            let all = b.list();
            let mut total: u64 = all.iter().map(|x| x.1).sum();
            let before = total;
            let mut removed = 0;
            // Oldest first. Working copies hold their own clones, so nothing they use breaks.
            for (p, size, _) in all {
                if total <= max {
                    break;
                }
                if std::fs::remove_file(&p).is_ok() {
                    total -= size;
                    removed += 1;
                }
            }
            println!("removed {removed} file(s), {} → {}", fmt_bytes(before), fmt_bytes(total));
        }
        Cmd::Worktree(WtCmd::Add { path, create, branch, seed_from, no_seed }) => {
            // From a working copy or a bare repository: the new one links to the same repository.
            let ctx = Ctx::open(None, cli.repo, ids).await?;
            let local = ctx.local.clone().context("not in a local repository")?;
            let (cur_branch, cur_base) = (ctx.branch().await?.to_string(), ctx.head().await?);
            let (br, at) = match (create, branch) {
                (Some(_), Some(_)) => bail!("use either -b or --branch"),
                (Some(n), None) => {
                    let base = cur_base.context("nothing to branch from")?;
                    ctx.repo.create_branch(&n, base).await?;
                    (n, Some(base))
                }
                (None, Some(b)) => (b, None),
                (None, None) => (cur_branch, cur_base),
            };
            let donor = match (no_seed, seed_from, &ctx.worktree) {
                (true, _, _) => None,
                (false, Some(d), _) => Some(std::fs::canonicalize(d)?),
                (false, None, here) => here.clone(),
            };
            let t = std::time::Instant::now();
            let mut co = Checkout { donor: donor.clone(), ..Default::default() };
            let wt = Worktree::clone(&local.config.store, Some(&local), &path, &br, at, &mut co).await?;
            let files_s = t.elapsed().as_secs_f64();
            println!(
                "added {} on {br} @ {}: files in {files_s:.1}s ({})",
                wt.root.display(),
                wt.head.base.unwrap().short(),
                checkout_summary(&co)
            );
            if let Some(d) = donor {
                let t = std::time::Instant::now();
                let s = seed_ignored(&d, &wt.root)?;
                println!(
                    "seeded {} ignored path(s) from {} in {:.1}s ({} cloned, {} copied)",
                    s.paths.len(),
                    d.display(),
                    t.elapsed().as_secs_f64(),
                    s.cloned,
                    s.copied
                );
                for p in s.paths.iter().take(12) {
                    println!("  {p}");
                }
                if s.paths.len() > 12 {
                    println!("  … and {} more", s.paths.len() - 12);
                }
            }
        }
        Cmd::Worktree(WtCmd::List) => {
            let ctx = Ctx::open(None, cli.repo, ids).await?;
            let local = ctx.local.clone().context("not in a local repository")?;
            if local.config.bare {
                println!("  {:50} (bare)", local.dir.display());
            }
            for p in local.worktrees().into_keys() {
                let mark = if ctx.worktree.as_ref() == Some(&p) { "*" } else { " " };
                match Worktree::open(&p) {
                    Ok(w) if w.root == p => println!(
                        "{mark} {:50} {} @ {}",
                        p.display(),
                        w.head.branch,
                        w.head.base.map(|h| h.short()).unwrap_or_default()
                    ),
                    _ => println!("{mark} {:50} (missing)", p.display()),
                }
            }
        }
        Cmd::Worktree(WtCmd::Remove { path, force }) => {
            let root = std::fs::canonicalize(&path)?;
            let wt = Worktree::open(&root)?;
            if wt.root != root {
                bail!("{} is not the root of a working copy", path.display());
            }
            if !force && !wt.status().await?.is_clean() {
                bail!("{} has uncommitted changes (use --force to delete anyway)", root.display());
            }
            if cwd()?.starts_with(&root) {
                bail!("can't remove the working copy you are in");
            }
            // The first working copy's .pail/ is the repository others link to.
            let holds_repo = wt.local.dir == root.join(".pail");
            if holds_repo && wt.local.worktrees().keys().any(|p| *p != root) {
                bail!("{} holds the repository other working copies use; remove those first", root.display());
            }
            std::fs::remove_dir_all(&root)?;
            if !holds_repo {
                wt.local.detach(&root)?;
            }
            println!("removed {}", root.display());
        }
        Cmd::Cat { path, rev } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let (rev, path) = match (rev, path.split_once(':')) {
                (Some(r), _) => (r, path),
                (None, Some((r, p))) => (r.to_string(), p.to_string()),
                (None, None) => ("HEAD".into(), path),
            };
            let h = if rev.is_empty() { ctx.head_commit().await? } else { ctx.resolve(&rev).await? };
            let data = pail::snapshot::Snapshot::open(&ctx.repo, h).await?.read(&path).await?;
            use std::io::Write;
            std::io::stdout().write_all(&data)?;
        }
    }
    Ok(())
}
