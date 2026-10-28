use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::engine::ArgValueCandidates;
use pail::config::{Config, Scope};
use pail::hash::Hash;
use pail::local::LocalRepo;
use pail::objects::now_date;
use pail::repo::{CommitRequest, Delta, Repo};
use pail::rev::{Ctx, Ids};
use pail::worktree::{Checkout, Worktree, seed_ignored};

mod cli_help;
mod complete;

#[derive(Parser)]
#[command(
    name = "pail",
    version,
    about = "Version control that lives in a bucket",
    help_template = cli_help::TOP,
    disable_help_subcommand = true
)]
struct Cli {
    /// Print the agent skill file (SKILL.md) and exit.
    #[arg(long)]
    skill: bool,
    /// Read a store directly (s3://, gs://, file://), with no local state. Also PAIL_STORE.
    #[arg(long)]
    store: Option<String>,
    /// Use this repository (a bare *.pail dir or a working copy), as `git --git-dir`. Also PAIL_DIR.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Print pail's commit ids, or the Git ids of mirrored commits.
    #[arg(long, value_enum, default_value = "pail")]
    ids: IdsArg,
    /// Don't pipe output into a pager.
    #[arg(long, short = 'P')]
    no_pager: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum IdsArg {
    Pail,
    Git,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a repository in a store (s3://, gs://, file://) with an empty default branch.
    #[command(after_long_help = cli_help::INIT)]
    Init {
        /// Where the repository lives: s3://bucket/prefix, gs://bucket/prefix or file:///path.
        store: String,
        /// The default branch (else `init.defaultBranch`, else `main`).
        #[arg(long)]
        branch: Option<String>,
    },
    /// Check out a branch into a new directory.
    #[command(after_long_help = cli_help::CLONE)]
    Clone {
        /// The store: s3://bucket/prefix, gs://bucket/prefix, file:///path or a directory.
        store: String,
        /// Directory to create (default: the store's last path segment).
        dir: Option<PathBuf>,
        /// Branch to check out (default: the store's default branch).
        #[arg(short, long)]
        branch: Option<String>,
        /// Check out this commit (or `<rev>`) instead of the branch's head: e.g. the exact
        /// commit a CI event named.
        #[arg(long)]
        at: Option<String>,
        /// Create a bare repository (no working copy): local caches and indexes for reading
        /// the store, and a home for linked worktrees.
        #[arg(long)]
        bare: bool,
    },
    /// Show staged, unstaged and untracked changes.
    Status,
    /// Stage changes (`-A` for everything).
    #[command(after_long_help = cli_help::ADD)]
    Add {
        /// Files or directories (relative to the current directory).
        paths: Vec<String>,
        /// Stage every change in the working copy, including new and deleted files.
        #[arg(short = 'A', long)]
        all: bool,
    },
    /// Remove paths from the stage (their files are untouched).
    Unstage {
        /// Files or directories to unstage.
        paths: Vec<String>,
    },
    /// Discard unstaged changes to paths (back to the staged or committed version).
    Restore {
        /// Files or directories to restore.
        paths: Vec<String>,
    },
    /// Commit the stage and publish it in one atomic write (`-a` stages tracked changes first).
    #[command(after_long_help = cli_help::COMMIT)]
    Commit {
        /// The commit message.
        #[arg(short, long)]
        message: String,
        /// Stage changes to tracked files first (not new files).
        #[arg(short, long)]
        all: bool,
        /// Skip the pre-commit, commit-msg and pre-publish hooks.
        #[arg(short = 'n', long)]
        no_verify: bool,
    },
    /// Bring the working copy up to date with its branch (a fast-forward pull).
    #[command(after_long_help = cli_help::SYNC)]
    Sync,
    /// List branches, or create one at the current base.
    #[command(after_long_help = cli_help::BRANCH)]
    Branch {
        /// Branch to create (or delete with -d, or make the default with --default).
        #[arg(add = ArgValueCandidates::new(complete::branches))]
        name: Option<String>,
        /// Delete the branch.
        #[arg(short, long)]
        delete: bool,
        /// Show the store's default branch, or with a name make it the default.
        #[arg(long)]
        default: bool,
    },
    /// Get and set options, as `git config`: the repository's file, or `--global`.
    #[command(after_long_help = cli_help::CONFIG)]
    Config {
        /// Use the user's file (~/.config/pail/config).
        #[arg(long)]
        global: bool,
        /// Use the repository's file (the default for writes).
        #[arg(long)]
        local: bool,
        /// List every setting.
        #[arg(short = 'l', long)]
        list: bool,
        /// Print the value of a key.
        #[arg(long)]
        get: bool,
        /// Remove a key.
        #[arg(long)]
        unset: bool,
        /// Show which file each setting comes from.
        #[arg(long)]
        show_scope: bool,
        /// A key such as user.name or core.hooksPath.
        #[arg(add = ArgValueCandidates::new(complete::config_keys))]
        key: Option<String>,
        /// The value to set.
        value: Option<String>,
    },
    /// Switch the working copy to another branch (`-c` creates it at the current base).
    #[command(after_long_help = cli_help::SWITCH)]
    Switch {
        /// The branch to switch to (or create with -c).
        #[arg(add = ArgValueCandidates::new(complete::branches))]
        branch: String,
        /// Create the branch at the current base first.
        #[arg(short, long)]
        create: bool,
    },
    /// Land a branch onto the default branch: squashed, or with --stack commit by commit.
    #[command(after_long_help = cli_help::LAND)]
    Land {
        /// The branch to land.
        #[arg(add = ArgValueCandidates::new(complete::branches))]
        branch: String,
        /// Branch to land onto (default: the store's default branch).
        #[arg(long, add = ArgValueCandidates::new(complete::branches))]
        onto: Option<String>,
        /// Message of the squashed commit (default: the branch's subjects).
        #[arg(short, long)]
        message: Option<String>,
        /// Land each commit of the branch, not one squashed commit.
        #[arg(long, conflicts_with = "message")]
        stack: bool,
        /// Skip the pre-publish hook.
        #[arg(short = 'n', long)]
        no_verify: bool,
    },
    /// Rewrite a branch's commits (reorder, reword, squash, fixup, drop, move onto another
    /// base) in one atomic publish.
    #[command(after_long_help = cli_help::REBASE)]
    Rebase {
        /// What the branch's own commits sit on: a branch or a commit (default: the default
        /// branch). Its commits after this are rewritten.
        #[arg(add = ArgValueCandidates::new(complete::branches))]
        upstream: Option<String>,
        /// The branch to rewrite (default: the working copy's branch).
        #[arg(short, long, add = ArgValueCandidates::new(complete::branches))]
        branch: Option<String>,
        /// Put the commits on this branch or commit instead of the upstream.
        #[arg(long, add = ArgValueCandidates::new(complete::branches))]
        onto: Option<String>,
        /// Edit the plan as a todo list first (Git's verbs).
        #[arg(short, long)]
        interactive: bool,
        /// Meld `fixup! <subject>` and `squash! <subject>` commits into their targets.
        #[arg(long)]
        autosquash: bool,
        /// Run this plan (JSON, `-` for stdin; see --print-plan) instead of building one.
        #[arg(long, value_name = "FILE", conflicts_with_all = ["upstream", "onto", "interactive", "autosquash"])]
        plan: Option<String>,
        /// Print the plan as JSON and stop.
        #[arg(long)]
        print_plan: bool,
        /// Build the new history and report it, publishing nothing.
        #[arg(long)]
        dry_run: bool,
        /// Report as JSON.
        #[arg(long)]
        json: bool,
        /// Undo the branch's latest rewrite (itself a rewrite: commits added since are kept).
        #[arg(long, conflicts_with_all = ["upstream", "onto", "interactive", "autosquash", "plan"])]
        undo: bool,
        /// Allow rewriting the store's default branch, which everyone builds on.
        #[arg(long)]
        rewrite_default: bool,
        /// Skip the pre-publish hook.
        #[arg(short = 'n', long)]
        no_verify: bool,
    },
    /// Show history, with `git log`'s options and output.
    #[command(after_long_help = cli_help::GIT_READS)]
    #[command(disable_help_flag = true)]
    Log {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Show commits (with patches), trees or file contents, as `git show`.
    #[command(after_long_help = cli_help::GIT_READS)]
    #[command(disable_help_flag = true)]
    Show {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Object contents, type or size, as `git cat-file` (`--batch` reads names from stdin).
    #[command(after_long_help = cli_help::GIT_READS)]
    #[command(name = "cat-file", disable_help_flag = true)]
    CatFile {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// List a tree with Git modes and object ids, as `git ls-tree`.
    #[command(after_long_help = cli_help::GIT_READS)]
    #[command(name = "ls-tree", disable_help_flag = true)]
    LsTree {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Resolve revisions and object names to ids, as `git rev-parse`.
    #[command(after_long_help = cli_help::GIT_READS)]
    #[command(name = "rev-parse", disable_help_flag = true)]
    RevParse {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Paths changed between two commits, or base→stage (`--staged`).
    #[command(after_long_help = cli_help::DIFF)]
    Diff {
        /// A revision (default: HEAD against its parent).
        #[arg(add = ArgValueCandidates::new(complete::branches))]
        from: Option<String>,
        /// A second revision: compare `from` with it.
        #[arg(add = ArgValueCandidates::new(complete::branches))]
        to: Option<String>,
        /// The stage against the base (`--cached` too): `A|M|D <path>` per staged path.
        #[arg(long, alias = "cached")]
        staged: bool,
        /// Paths only.
        #[arg(long)]
        name_only: bool,
        /// NUL-terminated paths (with --name-only), for scripts and hooks. Works for the
        /// stage and for `<from> [<to>]` alike.
        #[arg(short = 'z')]
        nul: bool,
        /// Only these change kinds, Git-style letters: A (added), M (modified), D (deleted).
        #[arg(long)]
        diff_filter: Option<String>,
    },
    /// List a directory of a commit (`-r` for everything under it, `-l` for sizes).
    #[command(after_long_help = cli_help::LS)]
    Ls {
        /// `<rev>`, `<rev>:<dir>` or a directory of HEAD.
        target: Option<String>,
        /// Everything under the directory.
        #[arg(short, long)]
        recursive: bool,
        /// With sizes.
        #[arg(short, long)]
        long: bool,
    },
    /// Delete unreachable objects older than the grace period.
    #[command(after_long_help = cli_help::GC)]
    Gc {
        /// The store (default: this repository's).
        store: Option<String>,
        /// Keep unreachable objects younger than this.
        #[arg(long, default_value_t = 168.0)]
        grace_hours: f64,
        /// Report what would be deleted.
        #[arg(long)]
        dry_run: bool,
        /// How long to wait for running commits to finish (new ones wait for gc).
        #[arg(long, default_value_t = 600)]
        wait_secs: u64,
    },
    /// Browse the physical data model in a browser: commits, pages, packs, chunks.
    #[command(after_long_help = cli_help::INSPECT)]
    Inspect {
        /// Address to serve on.
        #[arg(long, default_value = "127.0.0.1:7777")]
        addr: String,
        /// Don't open a browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Fill the local content cache (every file of the given commits, default HEAD).
    #[command(after_long_help = cli_help::FETCH)]
    Fetch {
        /// Revisions whose files to fetch (default: HEAD).
        #[arg(add = ArgValueCandidates::new(complete::branches))]
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
    #[command(subcommand, arg_required_else_help = true, after_long_help = cli_help::CACHE)]
    Cache(CacheCmd),
    /// Extra working copies that share file contents and build output copy-on-write.
    #[command(subcommand, arg_required_else_help = true, after_long_help = cli_help::WORKTREE)]
    Worktree(WtCmd),
    /// Print a file from a commit: `<path>` (HEAD) or `<rev>:<path>`.
    #[command(after_long_help = cli_help::CAT)]
    Cat {
        /// `<path>` in HEAD, or `<rev>:<path>`.
        path: String,
        /// The revision (or write `<rev>:<path>`).
        #[arg(long, add = ArgValueCandidates::new(complete::branches))]
        rev: Option<String>,
    },
    /// Which credential a store uses, and whether it works.
    #[command(subcommand, arg_required_else_help = true, after_long_help = cli_help::AUTH)]
    Auth(AuthCmd),
    /// Serve a store over https:// to people without bucket credentials (a stateless proxy).
    #[command(after_long_help = cli_help::SERVE)]
    Serve {
        /// The store to serve; repositories are paths under it (gs://bucket → https://host/<repo>).
        #[arg(long)]
        store: String,
        /// Address to listen on.
        #[arg(long, default_value = "127.0.0.1:8787")]
        addr: String,
        /// Command that authorizes each request (PAIL_AUTH_TOKEN, PAIL_AUTH_REPO, PAIL_AUTH_OP).
        #[arg(long)]
        auth_command: Option<String>,
        /// Allow requests without checking tokens (trusted networks and tests only).
        #[arg(long, conflicts_with = "auth_command")]
        allow_anonymous: bool,
    },
    /// Commit files straight to a branch, without a working copy (creates the branch).
    #[command(after_long_help = cli_help::PUT)]
    Put {
        /// The branch to commit to.
        #[arg(add = ArgValueCandidates::new(complete::branches))]
        branch: String,
        /// The commit message.
        #[arg(short, long)]
        message: String,
        /// `PATH=SRC`: store SRC (a file, or `-` for stdin) at PATH in the branch.
        #[arg(long = "file", value_name = "PATH=SRC")]
        files: Vec<String>,
        /// Remove PATH from the branch.
        #[arg(long = "delete", value_name = "PATH")]
        deletes: Vec<String>,
        /// Fail (exit 4) if any PATH already exists: create-only, e.g. to claim an id.
        #[arg(long)]
        if_absent: bool,
    },
    /// Stream ref changes as JSON lines, from the store's own notifications or by polling.
    #[command(after_long_help = cli_help::EVENTS)]
    Events {
        /// Only branches matching this glob (`*` matches anything; repeatable).
        #[arg(long = "branch", value_name = "GLOB")]
        branches: Vec<String>,
        /// Pull the bucket's GCS notifications from this Pub/Sub subscription
        /// (projects/<p>/subscriptions/<s>); default: poll.
        #[arg(long)]
        gcs_subscription: Option<String>,
        /// Polling interval in milliseconds.
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        /// Emit every matching branch once at start.
        #[arg(long)]
        initial: bool,
    },
    /// Branch policy: who may write what; the pieces a broker or lander is built from.
    #[command(subcommand, arg_required_else_help = true, after_long_help = cli_help::POLICY)]
    Policy(PolicyCmd),
    /// Print a shell completion script that completes branch names live.
    #[command(after_long_help = cli_help::COMPLETION)]
    Completion {
        /// The shell to complete for.
        #[arg(value_parser = ["bash", "zsh", "fish", "elvish", "powershell"])]
        shell: String,
    },
    /// Print a command's full help and examples.
    Help {
        /// The command (and subcommand), e.g. `land` or `worktree add`.
        command: Vec<String>,
    },
}

#[derive(clap::Args, Clone)]
struct AsIdentity {
    /// The caller's principal, e.g. user:alice@acme.dev, or lander.
    #[arg(long = "as", value_name = "PRINCIPAL")]
    principal: String,
    /// A group or role the caller holds (repeatable), e.g. group:eng, role:admin.
    #[arg(long = "group", value_name = "GROUP")]
    groups: Vec<String>,
}

#[derive(Subcommand)]
enum PolicyCmd {
    /// Print the repository's policy (the `_policy` branch's policy.json).
    Show,
    /// Validate a policy file and commit it to the `_policy` branch.
    Set {
        file: PathBuf,
        #[arg(short, long, default_value = "Update policy")]
        message: String,
    },
    /// May this caller read, write or delete this object? Exit 0 if allowed, 1 if not.
    Check {
        #[command(flatten)]
        who: AsIdentity,
        /// read, write or delete.
        #[arg(value_parser = ["read", "write", "delete"])]
        op: String,
        /// An object key in the repository (refs/main, packs/…), or a branch name.
        key: String,
    },
    /// A branch's rule as JSON: writers, approvals, required checks (what a lander enforces).
    Requirements {
        #[arg(add = ArgValueCandidates::new(complete::branches))]
        branch: String,
    },
    /// The GCS access boundary that encodes the policy for one caller (JSON).
    Boundary {
        #[command(flatten)]
        who: AsIdentity,
    },
    /// Mint a token for one caller: the boundary, exchanged at Google STS for a downscoped
    /// token. Prints a pail-credential/1 answer: this command is a broker's core.
    Token {
        #[command(flatten)]
        who: AsIdentity,
        /// Command printing the broad source token (the broker's service account).
        #[arg(long, default_value = "gcloud auth print-access-token")]
        source_token_cmd: String,
    },
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Installed credential providers (pail-credential-* extensions) and where they came from.
    List,
    /// Install a provider from its GitHub repository (like `gh extension install`) or a file.
    Install {
        /// `gcloud` (ArchAstro/pail-credential-gcloud), `owner/pail-credential-<name>[@ref]`,
        /// or a path to an executable.
        source: String,
        /// Install under this name (default: from the file name).
        #[arg(long)]
        name: Option<String>,
        /// Replace an installed provider of the same name.
        #[arg(long)]
        force: bool,
    },
    /// Update providers installed from GitHub to their latest release (pinned ones stay).
    Upgrade {
        /// A provider's name (default: with --all, every one).
        name: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Uninstall a provider from pail's extensions directory.
    Remove { name: String },
    /// Use a provider for a store: writes credential.<url>.provider.
    Use {
        /// The provider's name (`ambient` for the cloud SDK's own chain).
        #[arg(add = ArgValueCandidates::new(complete::providers))]
        provider: String,
        /// Store URL or prefix it applies to (default: this repository's store; with
        /// --global and no --for, every store).
        #[arg(long = "for")]
        url: Option<String>,
        /// Write the user's config instead of this repository's.
        #[arg(long)]
        global: bool,
    },
    /// Show the provider for a store (default: this repository's), fetch a credential and
    /// check that it can read the store.
    Status { store: Option<String> },
    /// Forget cached credentials.
    Logout,
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
    Ok((num
        .trim()
        .parse::<f64>()
        .with_context(|| format!("bad size {s}"))?
        * mult as f64) as u64)
}

fn dir_size(p: &std::path::Path) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    for e in ignore::WalkBuilder::new(p)
        .standard_filters(false)
        .build()
        .flatten()
    {
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
        /// Directory for the new working copy.
        path: PathBuf,
        /// Create this branch at the current base and check it out.
        #[arg(short = 'b', long = "create")]
        create: Option<String>,
        /// Check out this existing branch at its head (default: this working copy's branch and base).
        #[arg(long, add = ArgValueCandidates::new(complete::branches))]
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
    Remove {
        /// The working copy to delete.
        path: PathBuf,
        /// Delete even with uncommitted changes.
        #[arg(long)]
        force: bool,
    },
}

fn cwd() -> Result<PathBuf> {
    Ok(std::fs::canonicalize(std::env::current_dir()?)?)
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
    if n >= 1e9 {
        format!("{:.2} GB", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1} MB", n / 1e6)
    } else {
        format!("{:.0} KB", n / 1e3)
    }
}

fn main() {
    // Shell completion requests (COMPLETE=<shell> pail …) are answered here and exit.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();
    let cli = Cli::parse();
    if cli.skill {
        print!("{}", include_str!("skill.md"));
        return;
    }
    let Some(cmd) = cli.cmd else {
        Cli::command().print_help().expect("help");
        return;
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let cli = Cli {
        cmd: Some(cmd),
        ..cli
    };
    if let Err(e) = rt.block_on(run(cli)) {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
        }
        if let Some(f) = e.downcast_ref::<pail::gitlog::Fatal>() {
            eprintln!("fatal: {f}");
            std::process::exit(128);
        }
        if let Some(h) = e.downcast_ref::<pail::hooks::HookRefused>() {
            eprintln!("error: {h}");
            std::process::exit(pail::hooks::REFUSED_EXIT);
        }
        if let Some(n) = e
            .chain()
            .find_map(|c| c.downcast_ref::<pail::httpstore::NeedsSignIn>())
        {
            eprintln!("error: {n}");
            std::process::exit(1);
        }
        let text = format!("{e:#}");
        if text.contains("refs/")
            && (text.contains("403") || text.contains("lacked the necessary privileges"))
        {
            // The store's answer repeats itself several times; the first clause names what failed.
            let what = text.split(": ").next().unwrap_or(&text);
            eprintln!("error: {what}: permission denied (403)");
            eprintln!(
                "hint: the branch is protected by the repository's policy (`pail policy requirements <branch>`); push to a branch you may write, such as pr/*, and land it through the lander"
            );
            std::process::exit(1);
        }
        if let Some(c) = e.downcast_ref::<pail::repo::ConflictError>() {
            if e.to_string() == c.to_string() {
                eprintln!("error: {c}");
                eprintln!(
                    "hint: another writer changed the same paths; `pail sync`, then try again"
                );
            } else {
                // A rebase step: which commit, then the paths.
                eprintln!("error: {e:#}");
                eprintln!(
                    "hint: nothing was published; change the plan (or the commits) and retry"
                );
            }
            std::process::exit(4);
        }
        if let Some(m) = e.downcast_ref::<pail::repo::MovedError>() {
            eprintln!("error: {m}");
            std::process::exit(4);
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
        Output::open_with(no_pager, None)
    }

    /// Pager: `PAIL_PAGER`, then `core.pager`, then `PAGER`, then `less`.
    fn open_with(no_pager: bool, cfg: Option<&Config>) -> Output {
        use std::io::IsTerminal;
        let tty = std::io::stdout().is_terminal();
        let pager = std::env::var("PAIL_PAGER")
            .ok()
            .or_else(|| cfg.and_then(|c| c.get("core.pager")))
            .or_else(|| std::env::var("PAGER").ok())
            .unwrap_or_else(|| "less".into());
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
                return Output {
                    w: Box::new(std::io::BufWriter::new(stdin)),
                    pager: Some(child),
                    tty,
                };
            }
        }
        Output {
            w: Box::new(std::io::BufWriter::new(std::io::stdout())),
            pager: None,
            tty,
        }
    }

    fn finish(mut self) -> Result<()> {
        let flushed = self.w.flush();
        drop(self.w);
        if let Some(mut p) = self.pager.take() {
            let _ = p.wait();
        }
        flushed.or_else(|e| {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(e)
            }
        })?;
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
    let mut o = LogOpts::parse_with(args, &ctx.config)?;
    let words = std::mem::take(&mut o.revs);
    let mut paths = vec![];
    for w in words {
        if !paths.is_empty() {
            paths.push(w);
            continue;
        }
        let is_rev = w.contains("..")
            || w.starts_with('^')
            || ctx.try_resolve(&w).await.unwrap_or(None).is_some();
        if is_rev {
            o.revs.push(w);
            continue;
        }
        let exists = match ctx.head().await? {
            Some(h) => pail::snapshot::Snapshot::open(&ctx.repo, h)
                .await?
                .stat(&w)
                .await?
                .is_some(),
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
        let rel = cwd()?
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !rel.is_empty() {
            paths = paths.into_iter().map(|p| format!("{rel}/{p}")).collect();
        }
    }
    o.paths = paths;
    let spec = Pathspec::new(&o.paths)?;
    let commits = walk(ctx, &o, &spec, o.diff.any_output()).await?;
    let any_merge = commits.iter().any(|c| {
        c.commit
            .meta
            .get("git.parents")
            .is_some_and(|p| p.contains(' '))
    });
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
    let cmd = cli.cmd.expect("a command");
    // The Git-compatible commands parse their own arguments; `--help` (or a lone `-h`) still
    // means pail's help for them.
    let passthrough = match &cmd {
        Cmd::Log { args } => Some(("log", args)),
        Cmd::Show { args } => Some(("show", args)),
        Cmd::CatFile { args } => Some(("cat-file", args)),
        Cmd::LsTree { args } => Some(("ls-tree", args)),
        Cmd::RevParse { args } => Some(("rev-parse", args)),
        _ => None,
    };
    if let Some((name, args)) = passthrough {
        if args.iter().any(|a| a == "--help") || args.as_slice() == ["-h"] {
            return print_help(&[name.to_string()]);
        }
    }
    match cmd {
        Cmd::Completion { shell } => complete::registration(&shell)?,
        Cmd::Policy(cmd) => policy_cmd(cmd, cli.store, cli.repo, ids).await?,
        Cmd::Put {
            branch,
            message,
            files,
            deletes,
            if_absent,
        } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let repo = &ctx.repo;
            let base = repo.read_ref(&branch).await?.map(|(h, _)| h);
            let mut inputs = vec![];
            for f in &files {
                let (path, src) = f
                    .split_once('=')
                    .with_context(|| format!("--file wants PATH=SRC, got {f}"))?;
                let data = if src == "-" {
                    use std::io::Read;
                    let mut b = vec![];
                    std::io::stdin().read_to_end(&mut b)?;
                    b
                } else {
                    std::fs::read(src).with_context(|| format!("reading {src}"))?
                };
                inputs.push(pail::repo::FileInput {
                    path: path.trim_matches('/').to_string(),
                    kind: pail::manifest::Kind::File,
                    data: data.into(),
                });
            }
            if inputs.is_empty() && deletes.is_empty() {
                bail!("nothing to put: pass --file PATH=SRC or --delete PATH");
            }
            if if_absent && base.is_some() {
                let root = repo.root_of(base).await?;
                let present: Vec<String> = {
                    let mut v = vec![];
                    for i in &inputs {
                        if repo.get_entry(root, &i.path).await?.is_some() {
                            v.push(i.path.clone());
                        }
                    }
                    v
                };
                if !present.is_empty() {
                    return Err(pail::repo::ConflictError {
                        branch: branch.clone(),
                        paths: present,
                    }
                    .into());
                }
            }
            // The files must stay stored until the commit naming them lands.
            let writing = repo.writing().await?;
            let mut delta: Delta = BTreeMap::new();
            for e in repo
                .write_files(&inputs, &std::collections::HashMap::new())
                .await?
            {
                delta.insert(e.path.clone(), Some(e));
            }
            for d in deletes {
                delta.insert(d.trim_matches('/').to_string(), None);
            }
            let who = ctx.config.identity("author")?;
            let date = now_date();
            let out = repo
                .commit(CommitRequest {
                    branch: branch.clone(),
                    base,
                    delta,
                    author: who.clone(),
                    author_date: date.clone(),
                    committer: ctx.config.identity("committer")?,
                    committer_date: date,
                    message: message.clone(),
                    meta: BTreeMap::new(),
                })
                .await?;
            writing.done().await?;
            println!(
                "[{branch} {}] {}",
                out.commit.short(),
                message.lines().next().unwrap_or("")
            );
        }
        Cmd::Events {
            branches,
            gcs_subscription,
            interval_ms,
            initial,
        } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let source = match gcs_subscription {
                Some(sub) => {
                    let provider = pail::creds::choose(&ctx.repo.store.url, &ctx.config)?.provider;
                    let token = pail::creds::Source::new(&provider, &ctx.repo.store.url).context(
                        "GCS events need a credential provider for the store (e.g. `pail auth use gcloud --for <store>`)",
                    )?;
                    pail::events::Source::GcsPubSub {
                        subscription: sub,
                        token,
                    }
                }
                None => pail::events::Source::Poll {
                    interval: std::time::Duration::from_millis(interval_ms),
                },
            };
            use std::io::Write as _;
            pail::events::watch(&ctx.repo, source, &branches, initial, |e| {
                let mut out = std::io::stdout().lock();
                writeln!(out, "{}", serde_json::to_string(&e)?)?;
                out.flush()?;
                Ok(())
            })
            .await?;
        }
        Cmd::Auth(AuthCmd::Logout) => println!(
            "forgot {} cached credential(s)",
            pail::creds::clear_cache()?
        ),
        Cmd::Auth(AuthCmd::List) => {
            let installed = pail::creds::installed();
            for (name, exe) in &installed {
                let what = pail::creds::description(exe).unwrap_or_default();
                let from = match pail::extensions::installed_manifest(name) {
                    Some(m) if m.source == "github" => format!(
                        "{}@{}{}",
                        m.repo.unwrap_or_default(),
                        m.r#ref.unwrap_or_default(),
                        if m.pinned { " (pinned)" } else { "" }
                    ),
                    Some(m) => format!("local: {}", m.path.unwrap_or_default()),
                    None => exe.display().to_string(),
                };
                println!("{name:16} {what}\n{:16} {from}", "");
            }
            if installed.is_empty() {
                println!("no credential providers installed (the ambient cloud SDK chain is used)");
                println!(
                    "install one: pail auth install gcloud   (or owner/pail-credential-<name>, or a file)"
                );
            }
        }
        Cmd::Auth(AuthCmd::Install {
            source,
            name,
            force,
        }) => auth_install(&source, name, force).await?,
        Cmd::Auth(AuthCmd::Upgrade { name, all }) => auth_upgrade(name, all).await?,
        Cmd::Auth(AuthCmd::Remove { name }) => {
            let p = pail::extensions::install_dir(&name)?;
            if p.is_dir() {
                std::fs::remove_dir_all(&p)?;
            } else {
                std::fs::remove_file(&p)
                    .with_context(|| format!("{name} is not installed ({})", p.display()))?;
            }
            println!("removed {}", p.display());
        }
        Cmd::Auth(AuthCmd::Use {
            provider,
            url,
            global,
        }) => auth_use(&provider, url, global)?,
        Cmd::Auth(AuthCmd::Status { store }) => auth_status(store).await?,
        Cmd::Serve {
            store,
            addr,
            auth_command,
            allow_anonymous,
        } => {
            pail::serve::serve(
                &pail::rev::store_url(&store)?,
                &addr,
                auth_command,
                allow_anonymous,
            )
            .await?
        }
        Cmd::Help { command } => print_help(&command)?,
        Cmd::Log { args } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let mut out = Output::open_with(cli.no_pager, Some(&ctx.config));
            let r = cmd_log(&ctx, &args, &mut out).await;
            done(out, r)?;
        }
        Cmd::Inspect { addr, no_open } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            pail::inspect::serve(ctx, &addr, !no_open).await?;
        }
        Cmd::Show { args } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let mut out = Output::open_with(cli.no_pager, Some(&ctx.config));
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
            let cfg = Config::load(None);
            let branch = branch.unwrap_or_else(|| cfg.default_branch());
            pail::objects::validate_branch(&branch)?;
            let store = pail::rev::store_url(&store)?;
            let repo = Repo::remote(&store)?;
            if repo.read_ref(&branch).await?.is_some() {
                bail!("{store} already has branch {branch}");
            }
            // The default branch a bare read (`--store`) starts from.
            let head = serde_json::to_vec(&pail::rev::StoreHead {
                branch: branch.clone(),
            })?;
            repo.store
                .put(pail::rev::STORE_HEAD_KEY, head.into())
                .await?;
            let who = cfg.identity("author")?;
            let committer = cfg.identity("committer")?;
            let date = now_date();
            let out = repo
                .commit(CommitRequest {
                    branch: branch.clone(),
                    base: None,
                    delta: Delta::new(),
                    author: who,
                    author_date: date.clone(),
                    committer,
                    committer_date: date,
                    message: "Initial empty commit".into(),
                    meta: BTreeMap::new(),
                })
                .await?;
            println!("initialized {store} ({branch} @ {})", out.commit.short());
        }
        Cmd::Clone {
            store,
            dir,
            branch,
            at,
            bare,
        } => {
            let store = pail::rev::store_url(&store)?;
            // The store's default branch unless one is named.
            let repo = Repo::remote(&store)?;
            let head = repo.store.get_opt(pail::rev::STORE_HEAD_KEY).await?;
            let default = match &head {
                Some((d, _)) => serde_json::from_slice::<pail::rev::StoreHead>(d)?.branch,
                None => Config::load(None).default_branch(),
            };
            let branch = branch.unwrap_or_else(|| default.clone());
            if (store.starts_with("https://") || store.starts_with("http://"))
                && head.is_none()
                && repo.read_ref_value(&branch).await?.is_none()
            {
                // Servers answer 404 for private repositories on purpose.
                bail!("{store}: the repository doesn't exist, or you don't have access to it");
            }
            let name = store
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("repo")
                .to_string();
            if bare {
                let dir = dir.unwrap_or_else(|| PathBuf::from(format!("{name}.pail")));
                if dir.exists() && std::fs::read_dir(&dir)?.next().is_some() {
                    bail!("{} exists and is not empty", dir.display());
                }
                let local = LocalRepo::create(&dir, &store, true)?;
                std::fs::write(local.dir.join("HEAD"), &default)?;
                let repo = local.repo()?;
                let tip = repo.read_ref(&branch).await?;
                println!(
                    "bare repository {} for {store}{}",
                    local.dir.display(),
                    tip.map(|(h, _)| format!(" ({branch} @ {})", h.short()))
                        .unwrap_or_default()
                );
                return Ok(());
            }
            let dir = dir.unwrap_or_else(|| PathBuf::from(&name));
            let t = std::time::Instant::now();
            let mut co = Checkout::default();
            let at = match at {
                Some(rev) => Some(
                    Ctx::open(Some(store.clone()), None, ids)
                        .await?
                        .resolve(&rev)
                        .await?,
                ),
                None => None,
            };
            let wt = Worktree::clone(&store, None, &dir, &branch, at, &mut co).await?;
            std::fs::write(wt.local.dir.join("HEAD"), &default)?;
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
            wt.hooks().notify(
                "post-checkout",
                &[&pail::hooks::id(None), &wt.head.base.unwrap().hex(), "1"],
                None,
            );
        }
        Cmd::Status => {
            let wt = Worktree::open(&cwd()?)?;
            let st = wt.status().await?;
            println!(
                "on {} @ {}",
                wt.head.branch,
                wt.head
                    .base
                    .map(|h| h.short())
                    .unwrap_or_else(|| "(empty)".into())
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
            let spec = if all || paths.is_empty() {
                vec![String::new()]
            } else {
                wt.resolve(&cwd()?, &paths)?
            };
            if !all && paths.is_empty() {
                bail!("nothing specified (use `pail add <path>` or `pail add -A`)");
            }
            let n = wt.add(&spec, false).await?;
            println!("staged {n} change(s)");
        }
        Cmd::Unstage { paths } => {
            let mut wt = Worktree::open(&cwd()?)?;
            let spec = if paths.is_empty() {
                vec![String::new()]
            } else {
                wt.resolve(&cwd()?, &paths)?
            };
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
        Cmd::Commit {
            message,
            all,
            no_verify,
        } => {
            let mut wt = Worktree::open(&cwd()?)?;
            if all {
                wt.add(&[String::new()], true).await?;
            }
            let (out, message) = wt.commit(&message, no_verify).await?;
            if let Some(r) = &wt.rewritten {
                note_rewritten(r);
            }
            let retried = if out.attempts > 1 {
                format!(", landed after {} attempts", out.attempts)
            } else {
                String::new()
            };
            println!(
                "[{} {}] {}{retried}",
                wt.head.branch,
                out.commit.short(),
                message.lines().next().unwrap_or("")
            );
        }
        Cmd::Sync => {
            let mut wt = Worktree::open(&cwd()?)?;
            let n = wt.sync().await?;
            if let Some(r) = &wt.rewritten {
                note_rewritten(r);
            }
            println!(
                "{} @ {} ({n} path(s) updated)",
                wt.head.branch,
                wt.head.base.unwrap().short()
            );
        }
        Cmd::Branch {
            name,
            delete: _,
            default: true,
        } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            match name {
                Some(n) => {
                    ctx.set_default_branch(&n).await?;
                    println!("default branch is now {n}");
                }
                None => println!("{}", ctx.default_branch().await?),
            }
        }
        Cmd::Branch {
            name,
            delete,
            default: false,
        } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            match name {
                Some(n) if delete => {
                    if ctx.default_branch().await? == n {
                        bail!("{n} is the default branch; make another the default first");
                    }
                    ctx.repo.delete_branch(&n).await?;
                    println!("deleted {n}");
                }
                Some(n) => {
                    let base = ctx.head().await?.context("nothing to branch from")?;
                    ctx.repo.create_branch(&n, base).await?;
                    println!("created {n} @ {}", base.short());
                }
                None => {
                    let current = ctx.branch().await?.to_string();
                    for (b, h) in ctx.repo.list_refs().await? {
                        let mark = if b == current { "*" } else { " " };
                        println!("{mark} {b:30} {}", h.short());
                    }
                }
            }
        }
        Cmd::Config {
            global,
            local,
            list,
            get,
            unset,
            show_scope,
            key,
            value,
        } => {
            let start = match cli
                .repo
                .clone()
                .or_else(|| std::env::var_os("PAIL_DIR").map(PathBuf::from))
            {
                Some(d) => std::fs::canonicalize(d)?,
                None => cwd()?,
            };
            let repo_dir = pail::local::discover(&start).ok().map(|f| match f {
                pail::local::Found::Worktree(_, r) | pail::local::Found::Bare(r) => r.dir,
            });
            let mut cfg = Config::load(repo_dir.as_deref());
            let scope_name = |s: Scope| {
                if s == Scope::Global {
                    "global"
                } else {
                    "local"
                }
            };
            if list {
                for (sc, k, v) in cfg.list() {
                    if (global && sc != Scope::Global) || (local && sc != Scope::Local) {
                        continue;
                    }
                    if show_scope {
                        println!("{}\t{k}={v}", scope_name(sc));
                    } else {
                        println!("{k}={v}");
                    }
                }
                return Ok(());
            }
            let key = key.context("usage: pail config [--global|--local] <key> [<value>]")?;
            let scope = if global { Scope::Global } else { Scope::Local };
            if unset {
                let f = cfg.file_mut(scope)?;
                if !f.unset(&key)? {
                    std::process::exit(5);
                }
                f.save()?;
                return Ok(());
            }
            match value {
                Some(v) if !get => {
                    let f = cfg.file_mut(scope)?;
                    f.set(&key, &v)?;
                    f.save()?;
                }
                _ => {
                    let v = if global {
                        Config {
                            global: cfg.global.clone(),
                            local: None,
                        }
                        .get(&key)
                    } else if local {
                        Config {
                            global: Default::default(),
                            local: cfg.local.clone(),
                        }
                        .get(&key)
                    } else {
                        cfg.get(&key)
                    };
                    match v {
                        Some(v) => println!("{v}"),
                        None => std::process::exit(1),
                    }
                }
            }
        }
        Cmd::Switch { branch, create } => {
            let mut wt = Worktree::open(&cwd()?)?;
            if create {
                let base = wt.head.base.context("nothing to branch from")?;
                wt.repo.create_branch(&branch, base).await?;
            }
            let n = wt.switch(&branch).await?;
            println!(
                "switched to {branch} @ {} ({n} path(s) changed)",
                wt.head.base.unwrap().short()
            );
        }
        Cmd::Land {
            branch,
            onto,
            stack: true,
            no_verify,
            ..
        } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let onto = match onto {
                Some(o) => o,
                None => ctx.default_branch().await?.to_string(),
            };
            let hooks = ctx_hooks(&ctx).with_env("BRANCH", onto.clone());
            let pre_publish = publish_hook(&hooks, "land");
            let check: Option<pail::repo::PublishCheck> =
                if !no_verify && hooks.find("pre-publish").is_some() {
                    Some(&pre_publish)
                } else {
                    None
                };
            let out = ctx
                .repo
                .land_stack_with(
                    &branch,
                    &onto,
                    &ctx.config.identity("committer")?,
                    &now_date(),
                    check,
                )
                .await?;
            if out.landed.is_empty() {
                println!("{branch} has nothing to land");
                return Ok(());
            }
            // Each landed commit names the stack commit it was rewritten from.
            let mut pairs = String::new();
            for h in &out.landed {
                if let Some(from) = ctx.repo.commit_meta(h).await?.meta.get("rewritten.from") {
                    pairs.push_str(&format!("{from} {}\n", h.hex()));
                }
            }
            if !pairs.is_empty() {
                hooks.notify("post-rewrite", &["land"], Some(pairs.as_bytes()));
            }
            println!(
                "landed {} commit(s) from {branch} onto {onto} @ {} ({})",
                out.landed.len(),
                out.tip.short(),
                if out.fast_forward {
                    "fast-forward"
                } else {
                    "replayed on the new head"
                }
            );
            for h in &out.landed {
                println!(
                    "  {} {}",
                    h.short(),
                    ctx.repo.commit_meta(h).await?.subject()
                );
            }
        }
        Cmd::Land {
            branch,
            onto,
            message,
            stack: false,
            no_verify,
        } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let onto = match onto {
                Some(o) => o,
                None => ctx.default_branch().await?.to_string(),
            };
            let repo = &ctx.repo;
            let (src, _) = repo
                .read_ref(&branch)
                .await?
                .with_context(|| format!("no branch {branch}"))?;
            repo.read_ref(&onto)
                .await?
                .with_context(|| format!("no branch {onto}"))?;
            let anc = repo
                .fork_point(src, &onto)
                .await?
                .context("branches share no history")?;
            let from = repo.root_of(Some(anc)).await?;
            let to = repo.root_of(Some(src)).await?;
            let delta: Delta = repo
                .diff(from, to)
                .await?
                .into_iter()
                .map(|c| (c.path, c.new))
                .collect();
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
            let msg =
                message.unwrap_or_else(|| format!("Land {branch}\n\n{}", subjects.join("\n")));
            let who = ctx.config.identity("author")?;
            let committer = ctx.config.identity("committer")?;
            let date = now_date();
            let hooks = ctx_hooks(&ctx).with_env("BRANCH", onto.clone());
            let pre_publish = publish_hook(&hooks, "land");
            let check: Option<pail::repo::PublishCheck> =
                if !no_verify && hooks.find("pre-publish").is_some() {
                    Some(&pre_publish)
                } else {
                    None
                };
            let out = repo
                .commit_with(
                    CommitRequest {
                        branch: onto.clone(),
                        base: Some(anc),
                        delta,
                        author: who.clone(),
                        author_date: date.clone(),
                        committer,
                        committer_date: date,
                        message: msg,
                        meta: BTreeMap::from([("landed.from".into(), branch.clone())]),
                    },
                    check,
                )
                .await?;
            println!("landed {branch} onto {onto} @ {}", out.commit.short());
        }
        Cmd::Rebase {
            upstream,
            branch,
            onto,
            interactive,
            autosquash,
            plan,
            print_plan,
            dry_run,
            json,
            undo,
            rewrite_default,
            no_verify,
        } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            rebase(
                &ctx,
                RebaseArgs {
                    upstream,
                    branch,
                    onto,
                    interactive,
                    autosquash,
                    plan,
                    print_plan,
                    dry_run,
                    json,
                    undo,
                    rewrite_default,
                    no_verify,
                },
            )
            .await?;
        }
        Cmd::Diff {
            from,
            to,
            staged,
            name_only,
            nul,
            diff_filter,
        } => {
            let wt = Worktree::open(&cwd()?)?;
            // Git's rules: upper-case letters select, lower-case letters exclude.
            let keep = |c: char| match &diff_filter {
                None => true,
                Some(f) if f.chars().any(|x| x.is_ascii_uppercase()) => f.contains(c),
                Some(f) => !f.contains(c.to_ascii_lowercase()),
            };
            use std::io::Write as _;
            let mut out = std::io::stdout().lock();
            let mut emit = |code: char, p: &str| -> Result<()> {
                if keep(code) {
                    match (name_only, nul) {
                        (true, true) => write!(out, "{p}\0")?,
                        (true, false) => writeln!(out, "{p}")?,
                        (false, _) => writeln!(out, "{code} {p}")?,
                    }
                }
                Ok(())
            };
            if staged {
                let base = wt.base_entries().await?;
                for (p, s) in &wt.stage {
                    let code = match (s, base.contains_key(p)) {
                        (pail::worktree::Staged::Del, _) => 'D',
                        (_, true) => 'M',
                        (_, false) => 'A',
                    };
                    emit(code, p)?;
                }
                return Ok(());
            }
            let ctx = Ctx::open(None, cli.repo, ids).await?;
            let a = match from {
                Some(r) => ctx.resolve(&r).await?,
                None => wt.head.base.context("empty history")?,
            };
            let b = match to {
                Some(r) => Some(ctx.resolve(&r).await?),
                None => None,
            };
            let changes = match b {
                Some(b) => {
                    let (ra, rb) = (
                        wt.repo.root_of(Some(a)).await?,
                        wt.repo.root_of(Some(b)).await?,
                    );
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
                emit(code, &c.path)?;
            }
        }
        Cmd::Ls {
            target,
            recursive,
            long,
        } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let (rev, dir) = match target.as_deref() {
                Some(t) if t.contains(':') => t
                    .split_once(':')
                    .map(|(r, d)| (r.to_string(), d.to_string()))
                    .unwrap(),
                Some(t) if ctx.try_resolve(t).await.unwrap_or(None).is_some() => {
                    (t.to_string(), String::new())
                }
                Some(t) => ("HEAD".into(), t.to_string()),
                None => ("HEAD".into(), String::new()),
            };
            let h = if rev.is_empty() {
                ctx.head_commit().await?
            } else {
                ctx.resolve(&rev).await?
            };
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
                            writeln!(
                                out.w,
                                "{:4} {:>12}  {}",
                                kind(&e.kind.into()),
                                e.size,
                                e.path
                            )?;
                        } else {
                            writeln!(out.w, "{}", e.path)?;
                        }
                    }
                } else {
                    for n in snap.list_dir(dir).await? {
                        let slash = if n.kind == pail::snapshot::NodeKind::Dir {
                            "/"
                        } else {
                            ""
                        };
                        if long {
                            writeln!(
                                out.w,
                                "{:4} {:>12}  {}{slash}",
                                kind(&n.kind),
                                n.size,
                                n.name()
                            )?;
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
        Cmd::Gc {
            store,
            grace_hours,
            dry_run,
            wait_secs,
        } => {
            let repo = match store {
                Some(s) => Repo::remote(&pail::rev::store_url(&s)?)?,
                None => Ctx::open(None, cli.repo, ids).await?.repo,
            };
            let r = pail::gc::gc_with(
                &repo,
                &pail::gc::GcOptions {
                    grace_secs: (grace_hours * 3600.0) as i64,
                    dry_run,
                    wait: std::time::Duration::from_secs(wait_secs),
                    now: None,
                },
            )
            .await?;
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
        Cmd::Fetch {
            revs,
            history,
            max_count,
            all,
        } => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let blobs = ctx.repo.blobs.as_ref().context(
                "fetch fills a local repository; make one with `pail clone --bare <store>`",
            )?;
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
            let mut want: std::collections::HashMap<Hash, pail::manifest::Entry> =
                Default::default();
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
                if history {
                    format!(" across {commits} commit(s)")
                } else {
                    String::new()
                }
            );
        }
        Cmd::Cache(CacheCmd::Stats) => {
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let local = ctx.local.as_ref().context("not in a local repository")?;
            println!(
                "repository {} ({})",
                local.dir.display(),
                local.config.store
            );
            for (name, what) in [
                ("blobs", "file contents"),
                ("objects", "commits, pages"),
                ("graph", "commit graph"),
                ("index", "Git ids"),
            ] {
                let (n, bytes) = dir_size(&local.dir.join(name));
                println!(
                    "  {name:8} {:>9} files  {:>10}  {what}",
                    n,
                    fmt_bytes(bytes)
                );
            }
        }
        Cmd::Cache(CacheCmd::Prune { max_size }) => {
            let max = parse_size(&max_size)?;
            let ctx = Ctx::open(cli.store, cli.repo, ids).await?;
            let b = ctx
                .repo
                .blobs
                .as_ref()
                .context("not in a local repository")?;
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
            println!(
                "removed {removed} file(s), {} → {}",
                fmt_bytes(before),
                fmt_bytes(total)
            );
        }
        Cmd::Worktree(WtCmd::Add {
            path,
            create,
            branch,
            seed_from,
            no_seed,
        }) => {
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
            let mut co = Checkout {
                donor: donor.clone(),
                ..Default::default()
            };
            let wt =
                Worktree::clone(&local.config.store, Some(&local), &path, &br, at, &mut co).await?;
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
            wt.hooks().notify(
                "post-checkout",
                &[&pail::hooks::id(None), &wt.head.base.unwrap().hex(), "1"],
                None,
            );
        }
        Cmd::Worktree(WtCmd::List) => {
            let ctx = Ctx::open(None, cli.repo, ids).await?;
            let local = ctx.local.clone().context("not in a local repository")?;
            if local.config.bare {
                println!("  {:50} (bare)", local.dir.display());
            }
            for p in local.worktrees().into_keys() {
                let mark = if ctx.worktree.as_ref() == Some(&p) {
                    "*"
                } else {
                    " "
                };
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
                bail!(
                    "{} has uncommitted changes (use --force to delete anyway)",
                    root.display()
                );
            }
            if cwd()?.starts_with(&root) {
                bail!("can't remove the working copy you are in");
            }
            // The first working copy's .pail/ is the repository others link to.
            let holds_repo = wt.local.dir == root.join(".pail");
            if holds_repo && wt.local.worktrees().keys().any(|p| *p != root) {
                bail!(
                    "{} holds the repository other working copies use; remove those first",
                    root.display()
                );
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
            let h = if rev.is_empty() {
                ctx.head_commit().await?
            } else {
                ctx.resolve(&rev).await?
            };
            let data = pail::snapshot::Snapshot::open(&ctx.repo, h)
                .await?
                .read(&path)
                .await?;
            use std::io::Write;
            std::io::stdout().write_all(&data)?;
        }
    }
    Ok(())
}

/// Hooks for a command run through a context: the working copy's, else the repository's
/// (run in the repository directory); none for a bare `--store`.
fn ctx_hooks(ctx: &Ctx) -> pail::hooks::Hooks {
    match (&ctx.local, &ctx.worktree) {
        (Some(local), Some(root)) => pail::hooks::Hooks::new(&ctx.config, &local.dir, root),
        (Some(local), None) => pail::hooks::Hooks::new(&ctx.config, &local.dir, &local.dir),
        (None, _) => pail::hooks::Hooks::off(&cwd().unwrap_or_default()),
    }
}

/// The pre-publish hook as a publish check: `<branch> <mode>`, stdin `<branch> <old> <new>`.
fn publish_hook<'a>(
    hooks: &'a pail::hooks::Hooks,
    mode: &'a str,
) -> impl Fn(&pail::repo::Publish) -> Result<()> + Sync + 'a {
    move |p| {
        let line = format!("{} {} {}\n", p.branch, pail::hooks::id(p.old), p.new.hex());
        hooks
            .run("pre-publish", &[&p.branch, mode], Some(line.as_bytes()))
            .map(|_| ())
    }
}

struct RebaseArgs {
    upstream: Option<String>,
    branch: Option<String>,
    onto: Option<String>,
    interactive: bool,
    autosquash: bool,
    plan: Option<String>,
    print_plan: bool,
    dry_run: bool,
    json: bool,
    undo: bool,
    rewrite_default: bool,
    no_verify: bool,
}

/// `pail rebase`: build a plan (defaults, `--autosquash`, `-i`, or `--plan`), carry it out in
/// memory, then publish it as one rewrite of the branch.
async fn rebase(ctx: &Ctx, a: RebaseArgs) -> Result<()> {
    use pail::rebase::{self as rb, Action, Plan, Step};
    let repo = &ctx.repo;
    let plan_file: Option<rb::PlanFile> = match &a.plan {
        Some(f) => {
            let text = if f == "-" {
                std::io::read_to_string(std::io::stdin())?
            } else {
                std::fs::read_to_string(f).with_context(|| format!("reading {f}"))?
            };
            Some(serde_json::from_str(&text).with_context(|| format!("parsing plan {f}"))?)
        }
        None => None,
    };
    let branch = match (&a.branch, &plan_file) {
        (Some(b), Some(p)) if *b != p.branch => {
            bail!("the plan is for {}, not {b}", p.branch)
        }
        (Some(b), _) => b.clone(),
        (None, Some(p)) => p.branch.clone(),
        (None, None) => ctx.branch().await?.to_string(),
    };
    let default = ctx.default_branch().await?.to_string();
    let publishes = !a.print_plan && !a.dry_run;
    if branch == default && !a.rewrite_default && publishes {
        bail!(
            "{branch} is the default branch: everyone builds on it. Rewrite a feature branch, or pass --rewrite-default"
        );
    }
    let committer = ctx.config.identity("committer")?;
    let date = now_date();
    let hooks = ctx_hooks(ctx).with_env("BRANCH", branch.clone());
    let pre_publish = publish_hook(&hooks, "rebase");
    let check: Option<pail::repo::PublishCheck> =
        if !a.no_verify && hooks.find("pre-publish").is_some() {
            Some(&pre_publish)
        } else {
            None
        };
    let (tip_ref, _) = repo
        .read_ref_value(&branch)
        .await?
        .with_context(|| format!("no branch {branch}"))?;
    let tip = tip_ref.commit;

    if a.undo {
        let id = tip_ref
            .lineage
            .rewrite
            .clone()
            .with_context(|| format!("{branch} has not been rewritten"))?;
        let rec = repo.read_rewrite(&branch, &id).await?;
        if a.dry_run || a.print_plan {
            println!(
                "would undo the {} of {branch}: {} -> {}",
                if rec.op.is_empty() {
                    "rewrite"
                } else {
                    &rec.op
                },
                rec.new.short(),
                rec.old.short()
            );
            return Ok(());
        }
        let out = repo
            .rewrite_ref(
                pail::repo::RewriteRequest::undo(&branch, &rec, committer, date),
                check,
            )
            .await?;
        report_rewrite(ctx, &branch, &out, 0, &[], a.json).await?;
        hooks.notify(
            "post-rewrite",
            &["rebase"],
            Some(rewrite_pairs(&out).as_bytes()),
        );
        if !a.json {
            stacked_hint(ctx, &branch, &out.record).await?;
        }
        follow_rewrite(ctx, &branch).await;
        return Ok(());
    }

    let mut plan: Plan = match plan_file {
        Some(f) => {
            let expected = ctx.resolve(&f.expect).await?;
            let base = ctx.resolve(&f.base).await?;
            let onto = ctx.resolve(&f.onto).await?;
            let range = rb::range(repo, base, expected).await?;
            let mut steps = vec![];
            for s in &f.steps {
                let (action, rev) = s.verb()?;
                steps.push(Step {
                    action,
                    commit: resolve_in(ctx, &range, rev).await?,
                    message: s.message.clone(),
                });
            }
            Plan {
                branch: branch.clone(),
                expected,
                base,
                onto,
                steps,
            }
        }
        None => {
            let up = a.upstream.clone().unwrap_or_else(|| default.clone());
            let (base, up_tip) = match repo.read_ref(&up).await? {
                Some((h, _)) if pail::objects::validate_branch(&up).is_ok() => {
                    if up == branch {
                        bail!(
                            "{branch} is its own upstream: name where its commits start, e.g. `pail rebase -i HEAD~3`"
                        );
                    }
                    let fp = repo
                        .fork_point(tip, &up)
                        .await?
                        .with_context(|| format!("{branch} and {up} share no history"))?;
                    (fp, h)
                }
                _ => {
                    let u = ctx.resolve(&up).await?;
                    let mb = repo
                        .merge_base(tip, u)
                        .await?
                        .with_context(|| format!("{branch} and {up} share no history"))?;
                    (mb, u)
                }
            };
            let onto = match &a.onto {
                Some(o) => ctx.resolve(o).await?,
                None => up_tip,
            };
            let mut plan = rb::default_plan(repo, &branch, tip, base, onto).await?;
            if a.autosquash {
                rb::autosquash(repo, &mut plan).await?;
            }
            plan
        }
    };
    if plan.steps.is_empty() && plan.onto == plan.base {
        println!("{branch} has no commits after {}", plan.base.short());
        return Ok(());
    }
    if a.interactive {
        let text = rb::todo_text(repo, &plan).await?;
        let edited = edit_text(ctx, true, "rebase-todo", &text)?;
        let lines = rb::parse_todo(&edited)?;
        if lines.is_empty() {
            bail!("nothing to do: the todo list is empty (nothing changed)");
        }
        let range = rb::range(repo, plan.base, plan.expected).await?;
        let mut steps = vec![];
        for l in &lines {
            let commit = resolve_in(ctx, &range, l.rev).await?;
            let mut message = None;
            if l.action == Action::Reword {
                let c = repo.commit_meta(&commit).await?;
                message = Some(if !l.text.is_empty() && l.text != c.subject() {
                    let body = c.message.split_once('\n').map(|(_, b)| b).unwrap_or("");
                    if body.is_empty() {
                        format!("{}\n", l.text)
                    } else {
                        format!("{}\n{body}", l.text)
                    }
                } else {
                    edit_message(ctx, &c.message)?
                });
            }
            steps.push(Step {
                action: l.action,
                commit,
                message,
            });
        }
        // A squash opens the editor on the combined message, as in Git; the melded commits
        // then add nothing more.
        let mut i = 0;
        while i < steps.len() {
            let mut j = i + 1;
            while j < steps.len() && matches!(steps[j].action, Action::Squash | Action::Fixup) {
                j += 1;
            }
            if steps[i + 1..j].iter().any(|s| s.action == Action::Squash) {
                let lead = repo.commit_meta(&steps[i].commit).await?;
                let mut msg = steps[i]
                    .message
                    .clone()
                    .unwrap_or_else(|| lead.message.clone());
                for s in &steps[i + 1..j] {
                    if s.action == Action::Squash {
                        let c = repo.commit_meta(&s.commit).await?;
                        msg = format!("{}\n\n{}", msg.trim_end(), c.message.trim_end());
                    }
                }
                steps[i].message = Some(edit_message(ctx, &msg)?);
                for s in &mut steps[i + 1..j] {
                    s.action = Action::Fixup;
                }
            }
            i = j;
        }
        plan.steps = steps;
    }
    if a.print_plan {
        println!("{}", serde_json::to_string_pretty(&plan.to_file())?);
        return Ok(());
    }
    let built = rb::build(repo, &plan, &committer, &date).await?;
    if built.tip == plan.expected {
        println!("{branch} is unchanged: nothing to rebase");
        return Ok(());
    }
    if a.dry_run {
        let would = pail::repo::RewriteOutcome {
            old: plan.expected,
            new: built.tip,
            lineage: pail::objects::Lineage {
                epoch: tip_ref.lineage.epoch + 1,
                rewrite: None,
            },
            record: pail::objects::Rewrite {
                old: plan.expected,
                new: built.tip,
                map: built.map.clone(),
                ..Default::default()
            },
            carried: vec![],
            attempts: 0,
        };
        if !a.json {
            println!("dry run: nothing published");
        }
        report_rewrite(ctx, &branch, &would, built.created(), &built.empty, a.json).await?;
        return Ok(());
    }
    // The new commits must stay stored until the branch names them.
    let writing = repo.writing().await?;
    rb::store(repo, &built).await?;
    let out = repo
        .rewrite_ref(
            pail::repo::RewriteRequest {
                branch: branch.clone(),
                expected: plan.expected,
                new: built.tip,
                op: "rebase".into(),
                committer,
                date,
                map: built.map.clone(),
            },
            check,
        )
        .await?;
    writing.done().await?;
    report_rewrite(ctx, &branch, &out, built.created(), &built.empty, a.json).await?;
    hooks.notify(
        "post-rewrite",
        &["rebase"],
        Some(rewrite_pairs(&out).as_bytes()),
    );
    if !a.json {
        stacked_hint(ctx, &branch, &out.record).await?;
    }
    follow_rewrite(ctx, &branch).await;
    Ok(())
}

/// A commit of a plan's range by id prefix, else any revision.
async fn resolve_in(ctx: &Ctx, range: &[Hash], rev: &str) -> Result<Hash> {
    let lower = rev.to_ascii_lowercase();
    if lower.len() >= 4 && lower.bytes().all(|b| b.is_ascii_hexdigit()) {
        let found: Vec<&Hash> = range
            .iter()
            .filter(|h| h.hex().starts_with(&lower))
            .collect();
        match found.as_slice() {
            [one] => return Ok(**one),
            [] => {}
            _ => bail!("{rev} is ambiguous among the branch's commits"),
        }
    }
    ctx.resolve(rev).await
}

/// `<old> <new>` lines for post-rewrite.
fn rewrite_pairs(out: &pail::repo::RewriteOutcome) -> String {
    out.record
        .map
        .iter()
        .filter_map(|(o, n)| n.map(|n| format!("{} {}\n", o.hex(), n.hex())))
        .collect()
}

async fn report_rewrite(
    ctx: &Ctx,
    branch: &str,
    out: &pail::repo::RewriteOutcome,
    created: usize,
    empty: &[Hash],
    json: bool,
) -> Result<()> {
    let map = &out.record.map;
    if json {
        let v = serde_json::json!({
            "branch": branch,
            "old": out.old,
            "new": out.new,
            "epoch": out.lineage.epoch,
            "rewrite": out.lineage.rewrite,
            "op": out.record.op,
            "map": map,
            "empty": empty,
            "carried": out.carried,
        });
        println!("{v}");
        return Ok(());
    }
    let dropped = map.values().filter(|n| n.is_none()).count();
    let op = if out.record.op.is_empty() {
        "rebased"
    } else if out.record.op == "undo" {
        "undid the last rewrite of"
    } else {
        "rebased"
    };
    println!(
        "{op} {branch}: {} -> {} ({created} new commit(s), {dropped} dropped{}; epoch {})",
        out.old.short(),
        out.new.short(),
        if out.carried.is_empty() {
            String::new()
        } else {
            format!(", {} carried from others", out.carried.len())
        },
        out.lineage.epoch
    );
    // In history order, oldest first (the map itself is ordered by id).
    let mut rows = vec![];
    for (old, new) in map {
        rows.push((ctx.repo.commit_meta(old).await?, old, new));
    }
    rows.sort_by_key(|(c, _, _)| c.generation);
    for (c, old, new) in rows {
        let what = match new {
            Some(n) => n.short(),
            None if empty.contains(old) => "dropped (already applied)".into(),
            None => "dropped".into(),
        };
        println!("  {} -> {what}  {}", old.short(), c.subject());
    }
    Ok(())
}

/// Branches built on the commits a rewrite replaced still hold the old history: name them.
async fn stacked_hint(ctx: &Ctx, branch: &str, rec: &pail::objects::Rewrite) -> Result<()> {
    let mut stale = vec![];
    for (b, h) in ctx.repo.list_refs().await? {
        if b == branch || b.starts_with('_') {
            continue;
        }
        if let Some(mb) = ctx.repo.merge_base(h, rec.old).await?
            && rec.map.contains_key(&mb)
        {
            stale.push(b);
        }
    }
    for b in stale {
        println!("hint: {b} is built on the old {branch}: `pail rebase {branch} --branch {b}`");
    }
    Ok(())
}

/// Bring this working copy along when it is on the rewritten branch.
async fn follow_rewrite(ctx: &Ctx, branch: &str) {
    let Some(root) = &ctx.worktree else { return };
    let Ok(mut wt) = Worktree::open(root) else {
        return;
    };
    if wt.head.branch != branch {
        return;
    }
    match wt.sync().await {
        Ok(n) => println!(
            "working copy now at {} ({n} path(s) updated)",
            wt.head.base.unwrap().short()
        ),
        Err(e) => eprintln!(
            "warning: the working copy is still on the old history ({e:#}); commit or restore, then `pail sync`"
        ),
    }
}

/// Say so when a commit or sync found the branch rewritten since the working copy's base.
fn note_rewritten(r: &pail::worktree::Rewritten) {
    let who = if r.by.is_empty() {
        String::new()
    } else {
        format!(" by {}", r.by)
    };
    let to = match r.successor {
        Some(s) => format!("its place in the new history is {}", s.short()),
        None => "none of it survived".into(),
    };
    eprintln!(
        "note: {} was rewritten{who} (epoch {} -> {}); your base {} is gone from it, {to}",
        r.branch,
        r.from.epoch,
        r.to.epoch,
        r.base.map(|b| b.short()).unwrap_or_default(),
    );
}

/// Edit `text` in the user's editor and return the result. The sequence editor (todo lists)
/// is `PAIL_SEQUENCE_EDITOR` or `sequence.editor`, falling back to the message editor:
/// `PAIL_EDITOR`, `core.editor`, `VISUAL`, `EDITOR`, then `vi`.
fn edit_text(ctx: &Ctx, sequence: bool, name: &str, text: &str) -> Result<String> {
    let from_env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let seq = if sequence {
        from_env("PAIL_SEQUENCE_EDITOR").or_else(|| ctx.config.get("sequence.editor"))
    } else {
        None
    };
    let editor = seq
        .or_else(|| from_env("PAIL_EDITOR"))
        .or_else(|| ctx.config.get("core.editor"))
        .or_else(|| from_env("VISUAL"))
        .or_else(|| from_env("EDITOR"))
        .unwrap_or_else(|| "vi".into());
    let dir = match &ctx.local {
        Some(l) => l.dir.clone(),
        None => std::env::temp_dir(),
    };
    let file = dir.join(format!("{name}-{}", std::process::id()));
    std::fs::write(&file, text)?;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(&editor)
        .arg(&file)
        .status()
        .with_context(|| format!("running editor {editor}"));
    let edited = std::fs::read_to_string(&file);
    let _ = std::fs::remove_file(&file);
    if !status?.success() {
        bail!("editor {editor} failed: nothing changed");
    }
    Ok(edited?)
}

/// A commit message through the editor: `#` lines dropped; empty aborts.
fn edit_message(ctx: &Ctx, msg: &str) -> Result<String> {
    let text = format!(
        "{}\n\n# Edit the message. Lines starting with '#' are ignored; an empty message aborts.\n",
        msg.trim_end()
    );
    let out = edit_text(ctx, false, "REBASE_MSG", &text)?;
    let kept: Vec<&str> = out.lines().filter(|l| !l.starts_with('#')).collect();
    let m = kept.join("\n").trim().to_string();
    if m.is_empty() {
        bail!("empty message: nothing changed");
    }
    Ok(format!("{m}\n"))
}

/// A command's long help: `pail help worktree add`.
fn print_help(command: &[String]) -> Result<()> {
    let mut c = Cli::command();
    c.build(); // full names in usage lines ("pail worktree add")
    let mut cur = &mut c;
    for name in command {
        cur = cur
            .find_subcommand_mut(name)
            .with_context(|| format!("no command `{}`", command.join(" ")))?;
    }
    cur.print_long_help()?;
    Ok(())
}

async fn auth_status(store: Option<String>) -> Result<()> {
    let (url, dir) = match store {
        Some(s) => (pail::rev::store_url(&s)?, None),
        None => match pail::local::discover(&cwd()?)? {
            pail::local::Found::Worktree(_, l) | pail::local::Found::Bare(l) => {
                (l.config.store.clone(), Some(l.dir))
            }
        },
    };
    let cfg = Config::load(dir.as_deref());
    let choice = pail::creds::choose(&url, &cfg)?;
    println!("store     {url}");
    match &choice.provider {
        pail::creds::Provider::Ambient => println!(
            "provider  {}   [{}]",
            choice.provider.describe(),
            choice.source
        ),
        p => {
            println!("provider  {}   [{}]", p.describe(), choice.source);
            let src = pail::creds::Source::new(p, &url).expect("not ambient");
            let c = tokio::task::spawn_blocking(move || src.get()).await??;
            let kind = if c.aws.is_some() {
                "aws keys"
            } else {
                "bearer token"
            };
            let left = c.expires_at.saturating_sub(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs(),
            );
            println!("credential {kind}, expires in {}m", left / 60);
        }
    }
    let s = pail::store::Store::open_in(&url, dir.as_deref())?;
    let t = std::time::Instant::now();
    match s.list("refs/").await {
        Ok(refs) => println!(
            "access    ok: read {} branch(es) in {} ms",
            refs.len(),
            t.elapsed().as_millis()
        ),
        Err(e) => {
            println!("access    failed: {e:#}");
            std::process::exit(1);
        }
    }
    Ok(())
}

async fn auth_install(source: &str, name: Option<String>, force: bool) -> Result<()> {
    use pail::extensions::{Installed, Source};
    let (name, body, m, describe) = match pail::extensions::parse_source(source)? {
        Source::Local(p) => {
            let body = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
            let file = p
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            let n = name.unwrap_or_else(|| {
                file.strip_prefix(pail::creds::PREFIX)
                    .unwrap_or(&file)
                    .to_string()
            });
            let path = std::fs::canonicalize(&p).unwrap_or(p).display().to_string();
            let m = Installed {
                name: n.clone(),
                source: "local".into(),
                repo: None,
                r#ref: None,
                commit: None,
                pinned: true,
                kind: "script".into(),
                path: Some(path.clone()),
                sha256: String::new(),
                installed_at: 0,
            };
            (n, body, m, format!("from {path}"))
        }
        Source::GitHub { owner, repo, r#ref } => {
            let f = pail::extensions::fetch(&owner, &repo, r#ref.as_deref()).await?;
            let what = format!(
                "from {}@{} ({}, {} {})",
                f.manifest.repo.as_deref().unwrap_or(""),
                f.manifest.r#ref.as_deref().unwrap_or(""),
                &f.manifest.commit.as_deref().unwrap_or("")
                    [..12.min(f.manifest.commit.as_deref().unwrap_or("").len())],
                f.manifest.kind,
                f.manifest.path.as_deref().unwrap_or("")
            );
            (name.unwrap_or(f.name), f.body, f.manifest, what)
        }
    };
    let exe = pail::extensions::write_install(&name, &body, m, force)?;
    println!("installed {name} {describe}");
    println!("  {}", exe.display());
    if let Some(d) = pail::creds::description(&exe) {
        println!("  {d}");
    }
    println!("use it: pail auth use {name} [--for <store-url>] [--global]");
    Ok(())
}

async fn auth_upgrade(name: Option<String>, all: bool) -> Result<()> {
    let names: Vec<String> = match (name, all) {
        (Some(n), false) => vec![n],
        (None, true) => pail::creds::installed()
            .into_iter()
            .map(|(n, _)| n)
            .collect(),
        _ => bail!("name a provider, or pass --all"),
    };
    for n in names {
        let Some(m) = pail::extensions::installed_manifest(&n) else {
            println!("{n:16} skipped: not installed by `pail auth install` from GitHub");
            continue;
        };
        let (Some(repo), "github") = (m.repo.clone(), m.source.as_str()) else {
            println!("{n:16} skipped: installed from a file");
            continue;
        };
        let (owner, r) = repo.split_once('/').context("bad repo in manifest")?;
        let want = m.pinned.then(|| m.r#ref.clone()).flatten();
        let f = pail::extensions::fetch(owner, r, want.as_deref()).await?;
        if f.manifest.commit == m.commit && pail::extensions::sha256_hex(&f.body) == m.sha256 {
            println!("{n:16} up to date ({repo}@{})", m.r#ref.unwrap_or_default());
            continue;
        }
        let to = f.manifest.r#ref.clone().unwrap_or_default();
        pail::extensions::write_install(&n, &f.body, f.manifest, true)?;
        println!("{n:16} {} -> {to}", m.r#ref.unwrap_or_default());
    }
    Ok(())
}

fn auth_use(provider: &str, url: Option<String>, global: bool) -> Result<()> {
    if provider != "ambient" && pail::creds::find_extension(provider).is_none() {
        bail!(
            "credential provider `{provider}` is not installed (pail auth list; pail auth install {provider})"
        );
    }
    let local = pail::local::discover(&cwd()?).ok().map(|f| match f {
        pail::local::Found::Worktree(_, l) | pail::local::Found::Bare(l) => l,
    });
    let url = match (url, &local) {
        (Some(u), _) => Some(pail::rev::store_url(&u)?),
        (None, Some(l)) if !global => Some(l.config.store.clone()),
        (None, _) if global => None,
        (None, None) => bail!("not in a pail repository: pass --for <store-url> (or --global)"),
        _ => unreachable!(),
    };
    let key = match &url {
        Some(u) => format!("credential.{}.provider", u.trim_end_matches('/')),
        None => "credential.provider".to_string(),
    };
    let mut cfg = Config::load(local.as_ref().map(|l| l.dir.as_path()));
    let scope = if global || local.is_none() {
        Scope::Global
    } else {
        Scope::Local
    };
    let f = cfg.file_mut(scope)?;
    f.set(&key, provider)?;
    f.save()?;
    println!(
        "{key} = {provider}   ({})",
        if matches!(scope, Scope::Global) {
            "user config"
        } else {
            "repository config"
        }
    );
    Ok(())
}

async fn policy_cmd(
    cmd: PolicyCmd,
    store: Option<String>,
    repo: Option<PathBuf>,
    ids: Ids,
) -> Result<()> {
    use pail::policy::{Identity, Op, Policy};
    let ctx = Ctx::open(store, repo, ids).await?;
    let load = || async {
        Policy::load(&ctx.repo)
            .await?
            .context("this repository has no policy (`pail policy set <file>`)")
    };
    let id = |w: &AsIdentity| Identity::new(&w.principal, &w.groups);
    match cmd {
        PolicyCmd::Show => println!("{}", serde_json::to_string_pretty(&load().await?)?),
        PolicyCmd::Set { file, message } => {
            let data =
                std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            Policy::parse(&data)?;
            let writing = ctx.repo.writing().await?;
            let es = ctx
                .repo
                .write_files(
                    &[pail::repo::FileInput {
                        path: pail::policy::FILE.into(),
                        kind: pail::manifest::Kind::File,
                        data: data.into(),
                    }],
                    &Default::default(),
                )
                .await?;
            let base = ctx
                .repo
                .read_ref(pail::policy::BRANCH)
                .await?
                .map(|(h, _)| h);
            let date = now_date();
            let out = ctx
                .repo
                .commit(CommitRequest {
                    branch: pail::policy::BRANCH.into(),
                    base,
                    delta: es.into_iter().map(|e| (e.path.clone(), Some(e))).collect(),
                    author: ctx.config.identity("author")?,
                    author_date: date.clone(),
                    committer: ctx.config.identity("committer")?,
                    committer_date: date,
                    message,
                    meta: BTreeMap::new(),
                })
                .await?;
            writing.done().await?;
            println!(
                "[{} {}] policy updated",
                pail::policy::BRANCH,
                out.commit.short()
            );
        }
        PolicyCmd::Check { who, op, key } => {
            let p = load().await?;
            let op = match op.as_str() {
                "read" => Op::Read,
                "write" => Op::Write,
                _ => Op::Delete,
            };
            let key = if key.contains('/')
                && [
                    "refs/", "intents/", "reflog/", "commits/", "pages/", "packs/", "chunks/",
                ]
                .iter()
                .any(|p| key.starts_with(p))
                || key == "HEAD"
            {
                key
            } else {
                format!("refs/{key}")
            };
            let d = p.decide(&id(&who), op, &key);
            println!(
                "{} {} {key}: {}",
                if d.allow { "allow" } else { "deny" },
                who.principal,
                d.reason
            );
            if !d.allow {
                std::process::exit(1);
            }
        }
        PolicyCmd::Requirements { branch } => {
            let p = load().await?;
            let out = match p.rule_for(&branch) {
                Some((glob, r)) => {
                    serde_json::json!({"branch": branch, "rule": glob, "protected": p.is_protected(&branch),
                    "writers": r.writers, "approvals": r.approvals.unwrap_or(0), "required_checks": r.required_checks})
                }
                None => {
                    serde_json::json!({"branch": branch, "rule": null, "protected": true, "writers": [], "approvals": 0, "required_checks": []})
                }
            };
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        PolicyCmd::Boundary { who } => {
            let p = load().await?;
            let (bucket, prefix) = gcs_location(&ctx.repo.store.url)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&p.gcs_boundary(&id(&who), &bucket, &prefix)?)?
            );
        }
        PolicyCmd::Token {
            who,
            source_token_cmd,
        } => {
            let p = load().await?;
            let (bucket, prefix) = gcs_location(&ctx.repo.store.url)?;
            let boundary = p.gcs_boundary(&id(&who), &bucket, &prefix)?;
            let source = std::process::Command::new("sh")
                .args(["-c", &source_token_cmd])
                .output()?;
            if !source.status.success() {
                bail!(
                    "source token command failed: {}",
                    String::from_utf8_lossy(&source.stderr).trim()
                );
            }
            let source = String::from_utf8(source.stdout)?.trim().to_string();
            let (token, expires_at) = pail::policy::gcs_downscope(&source, &boundary).await?;
            println!(
                "{}",
                serde_json::json!({"token": token, "expires_at": expires_at})
            );
        }
    }
    Ok(())
}

/// (bucket, prefix) of a gs:// store.
fn gcs_location(url: &str) -> Result<(String, String)> {
    let rest = url
        .strip_prefix("gs://")
        .context("token boundaries are for gs:// stores")?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    Ok((bucket.to_string(), prefix.trim_matches('/').to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    /// The grouped top-level help is written by hand: every command must be in it.
    #[test]
    fn top_help_lists_every_command() {
        for sub in Cli::command().get_subcommands() {
            let name = sub.get_name();
            let listed = cli_help::TOP.lines().any(|l| {
                l.starts_with("  ")
                    && l.split_whitespace()
                        .next()
                        .is_some_and(|w| w.trim_end_matches(',') == name)
                    || l.trim_start().starts_with(&format!("{name},"))
                    || l.contains(&format!(", {name} "))
            });
            assert!(listed, "`{name}` is missing from the top-level help");
        }
    }
}
