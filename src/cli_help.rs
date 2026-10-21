//! Help text: the grouped top-level page and each command's examples (`pail <cmd> --help`).
//! Every subcommand must appear in [`TOP`]; a test checks that.

pub const TOP: &str = "\
{before-help}pail {version} — version control that lives in a bucket

  in the store (s3://, gs://, file://)              on this machine
  refs/<branch> ─▶ commit ─▶ pages ─▶ packs         .pail/  stage, caches, commit graph
    one CAS        immutable, named by BLAKE3       working copies (copy-on-write)

  A commit is published the moment it is made: there is no push.

{usage-heading} {usage}

Start
  init <store>              Create a repository in a store
  clone <store> [dir]       Check out a branch into a working copy (--bare: no files)

Everyday
  status                    Staged, unstaged and untracked changes
  add <paths>|-A            Stage changes
  unstage <paths>           Take paths off the stage
  restore <paths>           Discard unstaged changes
  commit -m <msg>           Commit and publish the stage (-a: stage tracked changes first)
  sync                      Fast-forward the working copy to its branch's head
  diff [<from> [<to>]]      Paths changed (--staged: the stage)

Branches and landing
  branch [<name>]           List, create (-d delete, --default the default branch)
  switch <branch>           Switch the working copy (-c: create at the current base)
  land <branch>             Land onto the default branch: squashed, or --stack commit by commit

History (Git's interface and output)
  log, show                 History and commits, as `git log` / `git show`
  cat-file, ls-tree         Objects with Git ids, as `git cat-file` / `git ls-tree`
  rev-parse                 Resolve revisions, as `git rev-parse`
  ls [<rev>:<dir>]          List a directory of a commit
  cat [<rev>:]<path>        Print a file from a commit

Working copies and caches
  worktree add|list|remove  More working copies sharing contents and build output
  fetch [<revs>]            Fill the local content cache (as `git fetch` for a bare repo)
  cache stats|prune         Inspect or trim the local content cache

Extend
  put <branch> --file P=SRC Commit files to a branch without a working copy
  events [--branch GLOB]    Stream ref changes (bucket notifications, or polling)

Access
  auth list|install|use     Credential providers (extensions) for stores you can't reach directly
  auth status|logout        Which provider a store uses, and whether it works
  serve --store <url>       Run a proxy so people without bucket credentials can use https://

Repository
  config [<key> [<value>]]  Options, as `git config` (user.name, core.hooksPath, …)
  gc                        Delete unreachable objects past a grace period
  inspect                   Browse the physical data model in a browser

Shell and agents
  completion <shell>        Shell completion with live branch names (bash, zsh, fish, …)
  help <command>            A command's full help and examples (also `<command> --help`)

Options:
{options}

Examples:
  pail clone s3://acme/app && cd app
  pail add -A && pail commit -m \"Fix the parser\"        # committed and published
  pail switch -c fix && pail commit -am \"wip\" && pail land fix --stack
  pail log --oneline -20 -- src/

Exit codes: 0 ok · 1 error · 2 usage · 3 a hook refused · 4 conflict (sync, then retry)
· 128 fatal from a Git-compatible command.
Hooks: see `pail help commit`. Agents: `pail --skill` prints a skill file.
{after-help}";

pub const INIT: &str = "\
Examples:
  pail init s3://acme/app
  pail init gs://acme-vcs/app --branch trunk
  pail init file:///tmp/scratch

The store is the whole repository: refs, commits, manifest pages and packs. Nothing else
needs to run; any number of machines can clone and commit concurrently.";

pub const CLONE: &str = "\
Examples:
  pail clone s3://acme/app                 # ./app on the default branch
  pail clone s3://acme/app work -b feature
  pail clone --bare s3://acme/app          # ./app.pail: caches for reading, a home for worktrees

The working copy's .pail/ holds the stage, the content cache and the commit graph.";

pub const ADD: &str = "\
Examples:
  pail add src/ README.md
  pail add -A                              # everything, including new and deleted files

`add` snapshots the content: later edits are not committed until added again.";

pub const COMMIT: &str = "\
Examples:
  pail commit -m \"Fix the parser\"
  pail commit -am \"Fix the parser\"         # stage tracked changes first
  pail commit -n -m \"wip\"                  # skip pre-commit, commit-msg, pre-publish

Committing publishes: the branch moves in one atomic write (CAS). If another writer moved
the branch since your base and touched different paths, your change is replayed on top of
theirs and their changes appear in your working copy. Overlapping paths fail with exit 4:
`pail sync`, then commit again.

Hooks run in this order (see core.hooksPath):
  pre-commit → commit-msg <file> → [commit written] → pre-publish → publish
  → post-rewrite replay (only if another writer landed after the check) → post-commit
A refusing hook fails the command with exit code 3; PAIL_TRACE_HOOKS=<file> logs each run.";

pub const SYNC: &str = "\
Brings in commits other writers published, updating files you have not changed. Refuses if
an incoming change touches a path you changed locally. Runs the post-sync hook and records
ORIG_HEAD.";

pub const BRANCH: &str = "\
Examples:
  pail branch                              # list, * marks the working copy's branch
  pail branch feature                      # create at the current base
  pail branch -d feature
  pail branch --default                    # show the store's default branch
  pail branch --default trunk              # change it";

pub const SWITCH: &str = "\
Examples:
  pail switch feature
  pail switch -c fix                       # create `fix` at the current base and switch

Requires a clean working copy. Runs post-checkout <prev> <new> 1 and records ORIG_HEAD.";

pub const LAND: &str = "\
Examples:
  pail land feature                        # one squashed commit onto the default branch
  pail land feature --stack                # every commit, in order, in one atomic write
  pail land fix --stack --onto release
  pail land feature -n                     # skip pre-publish

--stack fast-forwards when it can and otherwise replays the stack on the new head; landing
a branch stacked on another lands both. A --stack landing is idempotent: after a failure,
run it again. Overlapping changes fail with exit 4.";

pub const GIT_READS: &str = "\
Takes Git's options and prints Git's output (byte for byte) for pail history; Git's
documentation applies. --ids git prints the Git ids of commits mirrored from Git.
  pail log --oneline -20
  pail log -p --stat -- src/parser.rs
  pail log --format='%h %an %s' main..feature
  pail show HEAD~2 --name-status
  pail cat-file -p HEAD:README.md
  pail ls-tree -r HEAD src/
  pail rev-parse --short HEAD ORIG_HEAD";

pub const DIFF: &str = "\
Examples:
  pail diff                                # what HEAD changed against its parent
  pail diff main feature
  pail diff --staged                       # A|M|D <path> for the stage
  pail diff --staged --name-only -z --diff-filter=AM   # for hooks: added or modified, NUL-separated";

pub const LS: &str = "\
Examples:
  pail ls                                  # top of HEAD
  pail ls -rl main:src                     # everything under src/ with sizes";

pub const CAT: &str = "\
Examples:
  pail cat README.md                       # from HEAD
  pail cat feature:src/main.rs";

pub const WORKTREE: &str = "\
Examples:
  pail worktree add ../app-fix -b fix      # new branch at the current base
  pail worktree add ../app-main --branch main
  pail worktree list
  pail worktree remove ../app-fix

Files are copy-on-write clones of one shared content cache, and ignored build output
(target/, node_modules/, …) is cloned from the current working copy, so a new worktree costs
almost no disk and starts warm.";

pub const FETCH: &str = "\
Examples:
  pail fetch                               # every file of HEAD
  pail fetch --all                         # every branch tip
  pail fetch main --history -n 500         # every file version in main's last 500 commits";

pub const CACHE: &str = "\
Examples:
  pail cache stats
  pail cache prune --max-size 10G";

pub const CONFIG: &str = "\
Git's config format and layering: the repository's .pail/config, then --global
(~/.config/pail/config). Keys pail reads:
  user.name, user.email          identity (or PAIL_AUTHOR=\"Name <email>\")
  init.defaultBranch             branch for `pail init`
  core.hooksPath                 hooks directory (relative to the working copy; empty: off)
  core.pager, color.ui, log.date, log.abbrevCommit, format.pretty, …   as Git

Examples:
  pail config --global user.name \"Ada Lovelace\"
  pail config core.hooksPath .githooks
  pail config -l --show-scope";

pub const GC: &str = "\
Deletes objects no branch reaches, only once they are older than the grace period, so a
commit in flight is never collected.
  pail gc --dry-run
  pail gc --grace-hours 24";

pub const INSPECT: &str = "\
Opens an object graph of the repository: branches, commits, manifest pages, packs and chunks,
with the logical file tree beside it. Click a folder to see which objects store it.
  pail inspect
  pail inspect --addr 127.0.0.1:9000 --no-open";

pub const COMPLETION: &str = "\
Prints a script that asks pail for completions as you type, so branch names (from the live
store) and config keys complete, besides commands and options.

  bash   echo 'source <(pail completion bash)' >> ~/.bashrc
  zsh    echo 'source <(pail completion zsh)' >> ~/.zshrc
  fish   pail completion fish > ~/.config/fish/completions/pail.fish
  elvish pail completion elvish >> ~/.config/elvish/rc.elv
  powershell  pail completion powershell >> $PROFILE";

pub const AUTH: &str = "\
Most people have no bucket credentials, so pail asks a credential provider for short-lived,
delegated ones. Providers are extensions you install: executables named
pail-credential-<name> (in ~/.local/share/pail/extensions or on PATH) that speak the
pail-credential/1 protocol. Without one, pail uses the cloud SDK's chain (ambient).

  pail auth list                           # installed and bundled providers
  pail auth install archdev                # bundled: your ArchDev login (pail.archdev.ai)
  pail auth install gcloud                 # bundled: your gcloud login
  pail auth install gcs-downscoped         # bundled: gcloud, narrowed to one repository
  pail auth install ./pail-credential-acme # anything you or your company ships
  pail auth use acme --for gs://acme-vcs   # [credential \"gs://acme-vcs\"] provider = acme
  pail auth use gcloud --global            # every store without a closer match
  pail auth status                         # which provider, and can it read the store?
  pail auth logout                         # forget cached credentials

Writing a provider: `<exe> get` reads a JSON request on stdin and prints a credential.
  stdin   {\"protocol\":\"pail-credential/1\",\"store\":\"gs://acme-vcs/app\",\"scheme\":\"gs\"}
  stdout  {\"token\":\"…\",\"expires_at\":<unix secs>}             GCS and https:// proxies
          {\"aws\":{\"access_key_id\":…,\"secret_access_key\":…,\"session_token\":…},\"expires_at\":…}
          or a bare token line
`<exe> describe` optionally prints one line for `pail auth list`. Credentials are cached
(0600, ~/.cache/pail/credentials) until a minute before they expire.
An uninstalled command works too: credential.<url>.helper = <cmd> (run as `<cmd> get`).
PAIL_CREDENTIAL_PROVIDER=<name> or PAIL_CREDENTIAL_HELPER=<cmd> override config.";

pub const SERVE: &str = "\
A stateless proxy: it holds the bucket credentials, and clients use https://<host>/<repo>
with a bearer token from their credential helper. Run any number of instances (Cloud Run,
Lambda behind a URL, a VM); nothing is kept between requests.

Each request is authorized by --auth-command with PAIL_AUTH_TOKEN, PAIL_AUTH_REPO and
PAIL_AUTH_OP (read|write) in its environment: exit 0 allows, exit 3 forbids (403), any other
exit rejects the token (401, so the client fetches a new one). Answers are cached a minute.

Examples:
  pail serve --store gs://acme-vcs --addr 0.0.0.0:8080 --auth-command 'acme-sso verify'
  pail serve --store file:///srv/pail --addr 127.0.0.1:8787 --allow-anonymous
  pail clone https://vcs.acme.dev/app      # a client, with credential.helper set for it";

pub const PUT: &str = "\
The write primitive for tools and extensions: an atomic commit straight to a branch, created
if missing, with the same concurrency as `pail commit` (replay on disjoint paths, exit 4 on a
conflict).

Examples:
  pail put _meta -m \"Open PR 12\" --file prs/12.json=pr.json --if-absent   # claim id 12
  echo ok | pail put _meta -m \"check\" --file checks/9f2c/test.json=-
  pail put _meta -m \"close\" --delete prs/12/draft.json";

pub const EVENTS: &str = "\
One JSON line per ref change: {\"branch\",\"commit\",\"old\",\"source\",\"at\"} (commit null:
deleted). With --gcs-subscription the events are the bucket's own notifications; otherwise
refs are polled. Notifications are at-least-once and unordered, so each one is checked
against the ref and duplicates are dropped: consumers should still key work by commit.

Wire a GCS bucket once:
  gcloud pubsub topics create pail-refs
  gcloud storage buckets notifications create gs://BUCKET --topic=pail-refs \\
      --object-prefix=REPO/refs/ --event-types=OBJECT_FINALIZE,OBJECT_DELETE
  gcloud pubsub subscriptions create pail-ci --topic=pail-refs

Examples:
  pail events --branch 'pr/*' --branch main
  pail --store gs://acme/app events --gcs-subscription projects/acme/subscriptions/pail-ci";
