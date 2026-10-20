# pail

Version control that lives in a bucket. Clients talk to S3, GCS or a local directory
directly. There is no server.

- **Linear history.** Branches move only by compare-and-swap (S3 `If-Match`, GCS
  `ifGenerationMatch`). There are no merge commits; landing a branch squashes it.
- **Git-style stage.** `add`, `add -A`, `commit -a` and `restore` behave as in Git. The stage
  stores only the staged delta, so its size depends on the change, not the repo.
- **Local repositories like `.git`.** Everything kept on a machine lives in a repository
  directory: a bare `name.pail/`, or a working copy's `.pail/`. Nothing is global.

## Data model

### The store (authoritative, shared)

```
<store>/
├─ HEAD                  {"branch": "main"}: the default branch
├─ refs/<branch>         {"commit": "<id>"}: the only mutable objects, moved by CAS
├─ reflog/<branch>/…     written only when a branch is force-moved
├─ commits/<blake3>      commit objects (JSON)
├─ pages/<blake3>        manifest pages: a prolly tree over sorted full paths
├─ packs/<blake3>.pack   small files, concatenated, read with range GETs
└─ chunks/<blake3>       large files (≥1 MB) as FastCDC chunks, deduped globally
```

Everything but `HEAD`, `refs/` and `reflog/` is immutable and named by the BLAKE3 hash of its
bytes, so any copy of it anywhere is valid forever.

A **commit** is:

```
parent        previous commit id (linear history: one parent at most)
root          manifest root page: the full snapshot
generation    depth from the first commit (fast ancestor checks)
files, bytes  snapshot totals
author, author_date, committer, committer_date, message     (Git's exact bytes)
meta          free-form; mirrored commits keep git.commit, git.parents, git.tree, …
changes       the change list: path → {old: kind/size/hash, new: kind/size/hash/location}
              (Mononoke's Bonsai model), computed against the parent it lands on
```

A **manifest** maps every path to its entry `(kind, size, content hash, location)`, where the
location is inline bytes (≤64 B, symlinks), a pack range, or a chunk list. Pages split at
key-derived boundaries, so identical subtrees share pages across commits and diffs skip them.

### The local repository (derived, per machine)

```
name.pail/                 bare: pail clone --bare <store> name.pail
work/.pail/                or the first working copy's .pail/
├─ config                  Git-format config: core.store, core.bare, plus any local options
├─ HEAD                    the store's default branch, remembered
├─ objects/{commits,pages} verified copies of store objects
├─ blobs/<hh>/<blake3>     file contents: read cache and the source checkouts clone from
├─ graph/                  commit graph: metadata, changed paths, id index (mmap)
├─ index/                  Git blob ids by content hash, Git tree ids by manifest root
└─ worktrees.json          working copies attached to this repository

work/.pail/                a working copy's own state, next to (or instead of) the repository:
├─ HEAD.json               {"branch", "base"}
├─ stage.json, pending/    the stage: staged delta + snapshotted contents
├─ statcache.bin           stat cache for fast status
└─ repo                    linked working copies only: path of the shared repository
```

Everything in it is derived from immutable store objects and verified on read, so it never
goes stale and any part can be deleted. Reading a store with `--store <url>` keeps nothing.

## Configuration

Git's format and layering: `~/.config/pail/config` (global; `PAIL_CONFIG_GLOBAL` moves it),
then the repository's `config` (local).

```sh
pail config --global user.name "Ada Lovelace"
pail config --global user.email ada@example.com
pail config --global init.defaultBranch trunk
pail config user.name "Ada (work)"               # this repository only
pail config --list --show-scope | pail config --unset log.date
```

| key | used for |
|---|---|
| `user.name`, `user.email` | author and committer (required, as in Git; `PAIL_AUTHOR_NAME`/`_EMAIL`, `PAIL_COMMITTER_*` override) |
| `init.defaultBranch` | the first branch of `pail init` (else `main`) |
| `core.pager`, `color.ui`, `core.abbrev` | output (`PAIL_PAGER` overrides the pager) |
| `log.date`, `log.decorate`, `diff.renames` | `log`/`show` defaults; flags override |

No branch name is assumed: the store's `HEAD` names its default branch (set by `init`,
changed with `pail branch --default <name>`). `clone` checks it out and `land` lands onto it
unless `--branch`/`--onto` say otherwise.

## CLI

```sh
pail init s3://bucket/repo                        # or gs://…, file://…, a directory
pail clone s3://bucket/repo work && cd work       # working copy; its .pail/ is the repository
echo hi > a.txt && pail add -A && pail commit -m "first"
pail status | pail diff [--staged] | pail log | pail ls | pail cat <path>
pail switch -c feature … pail commit -a -m wip
pail land feature && pail sync                    # onto the default branch
pail clone --bare s3://bucket/repo repo.pail      # repository without a working copy
pail --repo repo.pail log --oneline               # or cd repo.pail; or PAIL_DIR
```

A commit that loses the CAS race is replayed on the new head when the paths don't overlap.
If they overlap, the commit stops with a conflict.

Help, agents and completion:

1. `pail --help` groups the commands; `pail help <command>` (or `<command> --help`) gives
   options and examples. Exit codes: 0 ok, 1 error, 2 usage, 3 a hook refused, 4 conflict
   (`pail sync`, then retry), 128 fatal from a Git-compatible command.
2. `pail --skill` prints a SKILL.md for coding agents (rules that differ from Git: commit
   publishes, no merges, exit codes, safety).
3. `pail completion bash|zsh|fish|elvish|powershell` prints a script that asks pail as you
   type, so branch names come from the live store: `source <(pail completion zsh)`.

### Stacks

Commit as often as you like on a branch, then land the whole stack on main:

```sh
pail switch -c feature
pail commit -a -m "step 1" … pail commit -a -m "step 2" … pail commit -a -m "step 3"
pail land feature --stack            # every commit, in order, onto the default branch
pail land feature                    # or: one squashed commit (--onto <branch> to choose)
```

1. Main untouched since the stack started: main fast-forwards to the stack (same commits).
2. Main moved: each stack commit is replayed on the new head from its change list, keeping
   author, date and message (committer: the lander; `rewritten.from` names the original).
3. A stack commit touching a path main changed meanwhile is a conflict: nothing lands.
4. One CAS: the replayed commits are written first (content-addressed, create-only), then
   main moves once. That is the only ref write: the branch and the reflog are untouched. A
   lost race retries from the new head.
5. Idempotent and incremental: commits main already has (by `rewritten.from`) are skipped,
   so landing twice is a no-op and commits added to the stack later land on top of their
   landed predecessors.

Built from two store primitives: `write_commit` (store a commit on any parent without moving a
ref) and `advance_ref` (one CAS from the tip the caller saw). `commit` is one of each;
`land --stack` is N `write_commit`s and one `advance_ref`.

S3 settings come from the usual `AWS_*` variables (`AWS_ENDPOINT`, `AWS_ALLOW_HTTP` for
emulators). GCS settings come from `GOOGLE_*`.

## Inspecting the physical model

```sh
pail inspect                       # in a working copy or bare repository; opens a browser
pail --store s3://bucket/repo inspect --addr 127.0.0.1:7777 --no-open
```

Three panes: the **logical** view (the files and folders of a commit) on the left, the
**physical** objects that store them in the middle, details on the right. Click a folder
and the graph shows the index pages and file lists covering its paths and the packs holding
its bytes; select a page and the files it holds light up on the left. "How it works" walks
through the model step by step on your repository's real objects.

The graph draws objects in columns by how they reference each other:
branches → commits (parent chain) → manifest pages by level → leaf pages → files → packs and
chunks. Click an object to expand what it references; big fan-outs open 30 at a time.
Objects are drawn once, so pages that several commits share show up as one node with a
"shared by N" tag. Dashed edges are a commit's change list. "Trace" expands the whole chain
from a commit down to one file's byte range in its pack. The side panel shows each object's
key, size and role, a pack's byte map, and file contents.

## Worktrees: many checkouts, one copy on disk

```
             bucket ──fetch once──▶ <repo>/blobs/<blake3>   (read-only, one copy per content)
                                         │ clone   │ clone   │ clone      APFS clonefile / Linux FICLONE
                                         ▼         ▼         ▼
                                       wt/a      wt/b      wt/c           share blocks until edited
seed: wt/a/target, node_modules … ──clone──▶ wt/b/…                       warm build, no extra disk
```

```sh
pail worktree add ../feature -b feature      # new branch at this base, seeded from here
pail worktree add ../review --branch main --seed-from ~/src/other-checkout
pail --repo repo.pail worktree add ../wt     # from a bare repository
pail worktree list | pail worktree remove ../feature
```

1. Working copies attach to one repository and their files are copy-on-write clones of its
   `blobs/`, so extra worktrees cost inodes, not bytes. Editing a file in place is safe: the
   clone gets its own blocks. A working copy on another volume (or a filesystem without
   clones) gets copies.
2. `--seed-from` (default: the current worktree) clones ignored build output into the new
   worktree: `target/`, `node_modules/`, `_build/`, `.next/`, `.env*` … (override with a
   `.pailseed` file in gitignore syntax). Nested checkouts and other ignored paths are skipped.
3. Files identical to the seed source get its mtimes, so cargo/make see the seeded build as up
   to date. Files that differ get a fresh mtime and rebuild normally.
4. The first clone's `.pail/` holds the repository, so it can't be removed while linked
   working copies use it (as Git's main worktree).

Measured on firstlanding (28,877 files, 459 MB; APFS):

| | real disk | time |
|---|---|---|
| plain clone | 529 MB | 4.9s |
| `worktree add` | 26 MB | 2.4s |
| `worktree add` seeded with archdev's 10 GB `target/` | 29 MB | 3.6s |
| `cargo build` in it (cold build: 446 crates, 147s) | 0 MB, 0 crates | 0.6s |
| same, but target cloned without mtime matching | 5.6 GB, 8 crates | 112s |
| seeded from a real checkout: 140 `node_modules`/`_build`/`.next`, 5.3 GB apparent | 157 MB | 15s |

## Reading history: Git's interface, any commit

Read commands run in a working copy, in a bare repository (`--repo`, `PAIL_DIR`, or from
inside it), or straight against a store (`--store <url>`, `PAIL_STORE`; nothing is kept). A
bare read's HEAD is the store's default branch.

```sh
pail --repo repo.pail log --oneline -20 -- services/rust
pail --repo repo.pail log -p --stat main~10..main
pail --repo repo.pail show HEAD~3 --name-status
pail --repo repo.pail ls-tree -r -l HEAD:src
pail --repo repo.pail cat-file -p HEAD:package.json
printf 'HEAD:Cargo.toml\nHEAD:src\n' | pail --repo repo.pail cat-file --batch
pail --repo repo.pail rev-parse --short=12 HEAD HEAD:src
pail --store s3://bucket/repo ls -l HEAD:src  |  pail --store … cat HEAD~2:README.md
```

1. `log`, `show`, `cat-file`, `ls-tree`, `rev-parse` take Git's arguments and print Git's
   output: revisions (`~`, `^`, `A..B`, `A...B`, `^A`), formats (`--oneline`, `--pretty=…`,
   `--format` placeholders, `--date=…`), filters (`-n`, `--skip`, `--since/--until`,
   `--author`, `--grep`, `--merges`, paths/globs), diffs (`--name-status`, `--raw`,
   `--numstat`, `--stat`, `--shortstat`, `--summary`, `-p`, `-U<n>`, rename detection),
   pager and colors on a terminal.
2. Line diffs come from a port of Git's xdiff (Myers, indent heuristic), so patches and
   stats are Git's, not merely similar. Git object ids (blobs, trees, commits) are computed
   from pail data; `--ids=git` prints the Git commit ids of mirrored history.
3. `snapshot::Snapshot` is the library API behind it: `stat`, `list_dir`, `walk`, `read`,
   `read_range` for any commit, reading only the manifest pages and packs it needs.

### Filling a repository

Like `git fetch` into a bare repository: reads keep what they fetch, and `fetch` fills ahead.

```sh
pail --repo repo.pail fetch                    # every file at HEAD
pail --repo repo.pail fetch --history          # every version along HEAD's history
pail --repo repo.pail fetch --all --history -n 500
pail --repo repo.pail cache stats              # what the repository holds
pail --repo repo.pail cache prune --max-size 20G   # drop least recently used contents
```

On firstlanding (2,231 commits), `fetch --history` pulled 36,664 file versions (313 MB) in 27s.

### Local indexes

Derived, rebuildable, never stale (everything they index is immutable):

1. Commit graph (per repository, like Git's commit-graph): every commit's metadata and changed
   paths in one append-only file plus a sorted, memory-mapped index by pail id and Git id.
   Walks, `A..B`, id/prefix lookups and abbreviation lengths read it, never commit objects;
   `log -- <path>` filters on its changed paths.
2. HEAD resolves only when used; the store's default branch is remembered.
3. Git ids: blob ids in one index by content hash; root tree ids by manifest root (mirrored
   commits record `git.tree`, checked by the proof).
4. Line diffs for a batch of commits are computed in parallel, then printed in order.

Same range as `git log --first-parent` (2,221 commits), warm caches, median of 15:

| ms | before | pail | git |
|---|---|---|---|
| `log --oneline -1` | 77 | 6 | 12 |
| `log --format=%s` | 56 | 13 | 32 |
| `log --name-status -n 200` | 169 | 20 | 42 |
| `log --stat -n 200` | 343 | 93 | 112 |
| `log -p -n 200` | 507 | 126 | 122 |
| `log -- services/rust` | 1,623 | 14 | 44 |
| `cat-file -p <id>:path` | 26 | 6 | 10 |
| `cat-file -p <commit>` | 83 | 5 | 9 |
| `rev-parse <short id>` | 233 | 5 | 10 |

Proof: `scripts/gitlog-compat.sh` and `scripts/gitinspect-compat.sh` run each command with the
same arguments through `git` and `pail` on mirrored history and compare bytes. On firstlanding
(2,192 commits; a second window with 139 merges): 55/55 `log` cases, including `-p` over
the whole history (45 MB of patches, identical), and 127/127 `show`/`cat-file`/`ls-tree`/
`rev-parse` cases at four depths.

Differences from Git:

1. History is first-parent, so output matches `git log --first-parent`. `show` on a merge
   prints no combined (`--cc`) diff.
2. The default abbreviation is 7 characters (longer if needed to stay unique). Git sizes
   its default from the local clone's object count, so pass `--abbrev=<n>` when the length
   matters.
3. Not yet: `--graph`, `--follow`, `-L`, `-S/-G`, `--word-diff`, notes, mailmap (`%aN` is
   `%an`), `.gitattributes` diff drivers, `%w()`/`%<()` layout (accepted, not applied).

## pail-mirror: proving it on a real repository

`pail-mirror` watches a GitHub branch, forwards each first-parent commit into pail, and
proves every forwarded commit byte for byte. It rebuilds the Git commit id from data read
back from the store and requires it to match:

1. Every file's bytes give its Git blob id.
2. The manifest's paths, modes and blob ids give the Git tree id.
3. The commit's author, committer, dates, message, Git parents and extra headers, plus that
   tree id, give the Git commit id.

Periodically it also does a cold `pail clone` and compares every file with Git's raw objects.

```sh
source env.sh   # AWS_* for the store
pail-mirror run --github ArchAstro/firstlanding \
  --git-dir ~/.cache/pail-mirror/firstlanding.git --reference ~/archastro/firstlanding \
  --store s3://pail/firstlanding --state-dir ./state \
  --backfill 2000 --poll 30 --deep-every 250 --serve 127.0.0.1:8790
pail-mirror verify --git-dir … --store … --state-dir ./state   # re-prove the whole branch
```

The dashboard at `--serve` shows sync state, per-commit import and proof times, upload
costs, and deep-check results. `state/mirror.jsonl` holds one record per commit.

## Auth: credential providers are extensions

Most people have no bucket credentials. Pail asks an installed **credential provider** for
short-lived, delegated ones; pail itself only knows the protocol. Without a provider it uses
the cloud SDK's chain (gcloud ADC, `GOOGLE_*`, `AWS_*`).

```
                         ┌ pail-credential-gcloud           your gcloud login
pail ─▶ provider for ────┼ pail-credential-gcs-downscoped   gcloud, narrowed to one repo (CAB)
        this store URL   ├ pail-credential-archdev          your ArchDev login → pail.archdev.ai/<org>/<repo>
        (config)         └ pail-credential-<yours>          any SSO / broker / vault
   │
   ├─ gs://, s3://  ─▶ token or keys, straight to the bucket (no server)
   └─ https://host  ─▶ bearer token to `pail serve`, a stateless proxy holding the bucket
                       credentials and asking --auth-command per (token, repo, read|write)
```

```sh
pail auth list                               # installed providers, bundled ones
pail auth install gcs-downscoped             # bundled; or a path: ./pail-credential-acme
pail auth use gcs-downscoped --for gs://acme-vcs --global
pail auth status                             # provider, expiry, and a read of the store
pail auth logout                             # forget cached credentials
```

1. **Extension:** an executable `pail-credential-<name>` in `$PAIL_EXTENSIONS_DIR`, else
   `~/.local/share/pail/extensions` (where `pail auth install` puts it), else `$PATH`.
   `get` reads `{"protocol":"pail-credential/1","store":…,"scheme":…}` on stdin and prints
   `{"token":…,"expires_at":…}` (GCS, https), `{"aws":{…},"expires_at":…}` (S3) or a bare
   token. `describe` (optional) prints one line for `pail auth list`.
2. **Choice:** `[credential "<url-prefix>"] provider = <name>`, most specific prefix first, then
   `[credential]`; `helper = <cmd>` runs an uninstalled command. `PAIL_CREDENTIAL_PROVIDER` /
   `PAIL_CREDENTIAL_HELPER` override.
3. **Cache:** until a minute before `expires_at`, 0600 in `~/.cache/pail/credentials`.
4. **Scope is the provider's job.** `gcs-downscoped` shows how: it exchanges the user's token at
   Google STS for one bound by a Credential Access Boundary to `gs://<bucket>/<repo>/`. On a
   real bucket, pail works fully with it, and the same token gets 403 writing or listing
   outside the prefix.
5. **Proxy:** `pail serve --store gs://acme-vcs --auth-command <cmd>` serves
   `https://host/<repo>` (GET/HEAD/PUT with If-Match/If-None-Match, DELETE, list). The CAS
   contract tests pass through it. The auth command gets `PAIL_AUTH_TOKEN`, `PAIL_AUTH_REPO`
   and `PAIL_AUTH_OP`: exit 0 allows, 3 forbids (403), anything else rejects the token (401).
   Answers are cached a minute. On a 401, pail drops its cached credential, asks the provider
   again and retries once.

## Hooks

Executables pail runs at fixed points, like Git's, but shaped by pail's model: a commit is
published the moment it is made, so pre-commit and pre-push become one flow, and the
publish check sees the exact commit before the single CAS that publishes it.

```
pail commit   pre-commit ─▶ commit-msg $1 ─▶ [objects written] ─▶ pre-publish ─▶ CAS
              ─▶ post-rewrite replay (only if another writer landed after the check) ─▶ post-commit
pail land     pre-publish ─▶ CAS ─▶ post-rewrite land
switch, clone, worktree add   post-checkout <prev> <new> 1
sync                          post-sync <old> <new>
```

| Hook | Args / stdin | Can refuse | Git counterpart |
|---|---|---|---|
| `pre-commit` | none; may restage with `pail add` | yes | pre-commit |
| `commit-msg` | `$1` = message file, may be edited | yes | commit-msg |
| `pre-publish` | `<branch> commit\|land`; stdin `<branch> <old> <new>` | yes | pre-push |
| `post-commit` | none | no | post-commit |
| `post-rewrite` | `replay\|land`; stdin `<old> <new>` lines | no | post-rewrite |
| `post-checkout` | `<prev> <new> 1` | no | post-checkout |
| `post-sync` | `<old> <new>` (and `ORIG_HEAD`) | no | post-merge |

1. **Where:** `core.hooksPath` (relative to the working copy, so checked-in `.githooks`-style
   dirs work); default `<repo>/hooks`; empty or `/dev/null` turns hooks off. Only files named
   exactly after a hook and executable run, so helpers like `_lib` can sit beside them.
2. **Skip:** `pail commit -n/--no-verify` skips pre-commit, commit-msg and pre-publish;
   `pail land --no-verify` skips pre-publish. post-* hooks always run and never fail a command.
3. **Environment:** the caller's, plus `PAIL_HOOK=<name>`, `PAIL_HOOK_BRANCH`,
   `PAIL_HOOK_WORKTREE`, with the running pail first on `PATH`. Pail exports nothing that
   locates a repository, so a hook's build/test children cannot be retargeted at the real
   repository (the `GIT_DIR` problem firstlanding's `_git-scrubbed` works around).
4. **Results:** hook stdout+stderr go to pail's stderr. A refusing hook fails the command with
   **exit code 3**; `PAIL_TRACE_HOOKS=<file>` appends `{"hook","args","exit","ms"}` per run,
   so callers tell a hook refusal from any other failure without parsing text.
5. **Replays:** a commit on a stale base is built on the current head *before* the check, so
   pre-publish normally sees exactly what is published. Only a writer landing between the
   check and the CAS causes a replay; it is not re-checked (it adds only others' disjoint
   changes) and is reported through post-rewrite.
6. **Helpers for hook scripts:** `pail diff --staged --name-only -z --diff-filter=AM`,
   `pail rev-parse --show-toplevel`, `pail rev-parse ORIG_HEAD`.

### firstlanding's hooks in pail

| `.githooks/` | In pail |
|---|---|
| `pre-commit` (aster format → restage, lint, build, related tests) | `pre-commit`; restage with `pail add`; staged list from `pail diff --staged --name-only -z --diff-filter=ACMRT` |
| `pre-push` (changed archdev e2e tests, opt-in Dialyzer) | `pre-publish`; base is `$old` from stdin instead of `merge-base HEAD origin/main` |
| `post-checkout`, `post-merge`, `post-rewrite` → `_clear-stale-expert` | `post-checkout`, `post-sync`, `post-rewrite`; `pail diff --name-only <old> <new>` |
| `_git-scrubbed` | not needed: pail exports no repository-locating variables |
| archdev's `post-receive` on the daemon's bare mirror | no server to run it; a watcher on the store's refs and reflog is the equivalent (not built yet) |

## Benchmarks: pail-bench

`pail-bench` imports a real snapshot (firstlanding: 28.9k files, 460 MB) once, then runs
each scenario on its own branch and counts time, requests and bytes uploaded. Git runs the same edits in
working copies pushing to a bare remote (concurrent writers retry with `pull --rebase`).

```sh
pail-bench --git-dir ~/.cache/pail-mirror/firstlanding.git --store s3://pail/bench-1 \
  --work /tmp/bench [--only small,wide,writers,hot,stacks,bigfile,reads,wc] [--no-git] [--out r.json]
```

Results (2026-10-20, M-series laptop under load; pail on the local S3 emulator, Git local):

| Scenario | pail (S3) | Git (local bare remote) |
|---|---|---|
| Small commit, library, p50 | 27 ms | n/a |
| Small commit, CLI working copy (add + commit + publish), p50 | 340 ms | 2,587 ms (add 906, commit 203, push 1,465) |
| Edit 2,000 files, one commit | 0.25 s, 53 PUTs | 4.2 s |
| Add 5,000 files | 0.27 s | 5.7 s |
| Delete 3,200-file dir / rename 2,100-file dir | 0.04 s / 0.05 s | 1.1 s / 0.8 s |
| 1 / 4 / 16 writers, one branch (commits/s) | 37 / 11 / 7 | 1.6 / 0.6 / 0.1 |
| 16 writers: attempts per commit, p95 | 5.1, 12 s | 11.4 pushes, 596 s |
| Land 40 stacks while main moves, p50 | 118 ms | n/a |
| 8 stacks landing at once, p50 | 2.2 s (4.25 attempts) | n/a |
| Change 1 MB of a 256 MB file | 3.8 MB uploaded | n/a |
| `log` 240 commits / `log -- path` | 20 ms / 30 ms | n/a |
| Clone a working copy (29k files) | 22 s | 9.8 s (`--shared`) |

What the numbers say:

1. Writes are cheap and proportional to the change: a few pages per commit, content reused on rename.
2. One branch is one CAS slot. Every commit to it wins a CAS round trip, so contention, not
   bandwidth, is the ceiling (16 writers: 5 attempts per commit, each rewriting its pages).
   Batching writers into one CAS (a group commit, the merge queue in the spec) is the fix.
3. A clone writes each file twice (into the shared content cache, then a CoW clone into the
   worktree). That is the price of worktrees costing ~0 extra disk.

## Tests

```sh
cargo test --release
# CAS contract against real object-store APIs:
PAIL_TEST_STORES="s3://pail/test" AWS_ENDPOINT=http://localhost:19000 AWS_ALLOW_HTTP=true \
  AWS_ACCESS_KEY_ID=x AWS_SECRET_ACCESS_KEY=x AWS_REGION=us-east-1 cargo test --release --test backends
```
