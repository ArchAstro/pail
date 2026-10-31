# pail

Version control that lives in a bucket. Clients talk to S3, GCS or a local directory
directly. There is no server.

```sh
curl -fsSL https://pail.build/install.sh | sh      # macOS and Linux, arm64 and x86_64
pail auth install gcloud                           # a credential provider (see Auth below)
```

Or build from source: `cargo install --git https://github.com/ArchAstro/pail pail`.
Releases: tag `vX.Y.Z` (matching `Cargo.toml`) and `.github/workflows/release.yml` builds the
four binaries, `SHA256SUMS` and the GitHub release that `install.sh` downloads from.

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
├─ refs/<branch>         {"commit": "<id>", "epoch": n}: the only mutable objects, moved by CAS
├─ reflog/<branch>/…     one record per rewrite: old → new tip and each old commit's successor
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

### Rewriting history: rebase

Interactive and scripted rebases are one thing: a **plan** (Git's todo verbs) carried out in
memory from each commit's change list, then published in one CAS. No working copy, no
stopped state, no `--continue`: a conflict (exit 4) publishes nothing.

```sh
pail rebase                              # onto the default branch's tip
pail rebase -i HEAD~3                    # pick / reword / squash / fixup / drop, reorder
pail rebase --autosquash                 # `fixup! <subject>` commits meld into their targets
pail rebase --print-plan > plan.json     # agents: edit the JSON, then
pail rebase --plan plan.json --dry-run --json
pail rebase --undo                       # itself a rewrite, so it can be undone too
```

```
plan ─▶ build (prepare commits, check each path) ─▶ store ─▶ reflog record ─▶ CAS: epoch + 1
                     │ conflict: nothing written            (old → new map)    (commits added
                     ▼                                                          meanwhile are
                  exit 4                                                        carried over)
```

1. **Conflicts are per path:** a commit conflicts when a path it changes is not, on the new
   history, as its parent had it (or already as the commit leaves it). A commit whose changes
   are all there already is dropped; an unchanged commit on its original parent is reused.
2. **Versioned refs:** every rewrite bumps the branch's `epoch` and writes a record mapping each
   old commit to its successor (or none: dropped). Fast-forwards carry the epoch unchanged.
3. **Nobody has to be told first.** There is no list of who has a working copy (no server, and
   readers may not write), so instead every client can tell afterwards:
   1. `pail sync` / `pail commit` from a base of an older epoch say the branch was rewritten
      and where the base went; the files move by tree diff as always, local changes kept.
   2. A branch built on the old history (a stack) starts where it left it: `land` and
      `rebase <upstream>` take the fork point from the upstream's rewrite records, not the
      merge base, so the old commits are not counted as the stack's own. `pail rebase` names
      the branches it left behind (`hint: fb is built on the old fa: …`).
   3. Copies of copies are recognised: `rewritten.from` (several ids for a squash) is followed
      through every rewrite, so landing a stack after its base was rebased and landed skips it.
4. **Safety:** commits added on top of the planned tip meanwhile are carried onto the new
   history (or the rewrite conflicts); any other move of the branch fails the rewrite. The
   default branch is refused without `--rewrite-default`; policy still decides who may write.

S3 settings come from the usual `AWS_*` variables (`AWS_ENDPOINT`, `AWS_ALLOW_HTTP` for
emulators). GCS settings come from `GOOGLE_*`.

## Repacking: many small packs, few requests

Every commit writes one pack, so a branch built from many small commits keeps its files in
many small packs, and a cold checkout costs a request per pack. `pail maintenance repack`
writes one ordinary commit on the branch with the same files (same Git tree ids, nothing
in the change list) whose entries point into a few path-ordered packs of about 4 MiB:

```sh
pail maintenance repack --dry-run   # packs and files it would move, requests before and after
pail maintenance repack             # tiered: only files in packs under 1 MiB move
pail maintenance repack --full      # every packed file (don't schedule it)
```

Stored bytes are copied as they are, after checking each blob's hash. The old packs stay
alive through history (gc keeps what a commit names), so a repack adds the bytes it moves
to the store; the tier keeps that to the small packs. If a concurrent commit changes a file
being moved, that file is left out and the rest is published on the new tip. The protocol is
`commit` with the packs as uploads, modelled in `verify/` ("Packs and repacking").

`clone` (and `status`, at most daily) warns when 100 or more of the tip's packs hold under
256 KiB of it. `maintenance.warnSmallPacks` sets that count; 0 turns the warning off.

Ordinary commits also compact as they go. When the commit's base has 16 or more small packs
(under 1 MiB of the tip each), `commit` appends a relocation of old files to the pack it
uploads anyway: whole packs, smallest first, up to as many bytes as the commit adds (at least
256 KiB, at most 4 MiB). The bytes come from clean local files whose hash matches the tip's
entry; a missing, edited or sparse-excluded file is skipped, and nothing is fetched. A
relocation never makes a commit conflict, fail or retry more: if the branch moved a relocated
path meanwhile, that relocation is dropped. The old packs stay (history names them), so a
commit stores at most its budget of extra bytes. `maintenance.amortize = false` turns it off;
`maintenance.amortizeSmallPacks` sets the trigger (0 = off) and `maintenance.amortizeFactor`
the multiple of new bytes. Modelled in `verify/` ("Amortized compaction").

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

Every commit writes a pack of its own files, so replaying a long history leaves a pack per
commit. `--pack-batch N` stores the files of up to N pending commits in one pack, in path
order (the first of them uploads it, the rest point into it): fewer, larger packs, with every
commit still proved. `pail maintenance repack` merges the packs of history imported without
it. A mirror's resume and `verify` step over repack commits.

## Extending pail: conventions plus events

Features like pull requests and CI are programs that agree on names, not server code:
`pail put` writes records to a `_meta` branch atomically, `pail events` streams ref (and
sibling-prefix object) changes from the bucket's own notifications (GCS Pub/Sub) or by
polling, resumes from a `--cursor`, and treats every notification as a hint (a lost one
only delays an event; modelled in `verify/events/`), and `pail clone --at`
checks out the exact commit an event names. `extensions/pail-pr` and `extensions/pail-ci`
are working prototypes; see [docs/extending.md](docs/extending.md).

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
pail auth list                               # installed providers and their source
pail auth install gcs-downscoped             # ArchAstro/pail-credential-gcs-downscoped
pail auth install acme/pail-credential-sso@v1.2   # any GitHub repo; or a path: ./pail-credential-acme
pail auth upgrade --all                      # newest releases; pinned installs stay
pail auth use gcs-downscoped --for gs://acme-vcs --global
pail auth status                             # provider, expiry, and a read of the store
pail auth logout                             # forget cached credentials
```

1. **Extension:** its own GitHub repository `pail-credential-<name>`, installed like a gh
   extension. The repository's `pail-extension.json` names a script
   (`{"name","protocol":"pail-credential/1","description","script":"<path>"}`) or release
   binaries per platform (`"binaries": {"darwin-arm64": "<asset>", …}`); without one, an
   executable `pail-credential-<name>` at the root, else a release asset
   `pail-credential-<name>-<os>-<arch>`. The ref is `@ref` (pinned), else the latest release,
   else the default branch, resolved to a commit first. Installs land in
   `~/.local/share/pail/extensions/pail-credential-<name>/` (or `$PAIL_EXTENSIONS_DIR`) with a
   `manifest.json` (repo, ref, commit, sha256); `$PATH` is searched too. Private repositories
   work with `GH_TOKEN`, `GITHUB_TOKEN` or a `gh` login; `PAIL_GITHUB_API` points at GitHub
   Enterprise. Official providers: `ArchAstro/pail-credential-{gcloud,gcs-downscoped,archdev}`.
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

## Protecting branches: who may push, what landing requires

One policy (`_policy:policy.json`: readers, admins, gc, per-branch writers, approvals,
required checks) and three enforcement points, none stateful:

1. `pail serve --auth-command` checks every request against it (any backend).
2. A broker runs `pail policy token --as <principal>`: a GCS token whose Credential Access
   Boundary encodes the policy, so **GCS** refuses `refs/main` to anyone but its writers.
3. A lander (`pail policy requirements main`, then `pail land`) is the only writer of
   protected branches and checks reviews and checks first.

`pail policy show|set|check|requirements|boundary|token`. How to build each piece, IAM setup
and STS limits: [docs/protecting-branches.md](docs/protecting-branches.md).

## Hooks

Executables pail runs at fixed points, like Git's, but shaped by pail's model: a commit is
published the moment it is made, so pre-commit and pre-push become one flow, and the
publish check sees the exact commit before the single CAS that publishes it.

```
pail commit   pre-commit ─▶ commit-msg $1 ─▶ [objects written] ─▶ pre-publish ─▶ CAS
              ─▶ post-rewrite replay (only if another writer landed after the check) ─▶ post-commit
pail land     pre-publish ─▶ CAS ─▶ post-rewrite land
pail rebase   pre-publish ─▶ CAS ─▶ post-rewrite rebase
switch, clone, worktree add   post-checkout <prev> <new> 1
sync                          post-sync <old> <new>
```

| Hook | Args / stdin | Can refuse | Git counterpart |
|---|---|---|---|
| `pre-commit` | none; may restage with `pail add` | yes | pre-commit |
| `commit-msg` | `$1` = message file, may be edited | yes | commit-msg |
| `pre-publish` | `<branch> commit\|land\|rebase`; stdin `<branch> <old> <new>` | yes | pre-push |
| `post-commit` | none | no | post-commit |
| `post-rewrite` | `replay\|land\|rebase`; stdin `<old> <new>` lines | no | post-rewrite |
| `post-checkout` | `<prev> <new> 1` | no | post-checkout |
| `post-sync` | `<old> <new>` (and `ORIG_HEAD`) | no | post-merge |

1. **Where:** `core.hooksPath` (relative to the working copy, so checked-in `.githooks`-style
   dirs work); default `<repo>/hooks`; empty or `/dev/null` turns hooks off. Only files named
   exactly after a hook and executable run, so helpers like `_lib` can sit beside them.
2. **Skip:** `pail commit -n/--no-verify` skips pre-commit, commit-msg and pre-publish;
   `pail land --no-verify` and `pail rebase --no-verify` skip pre-publish. post-* hooks always run and never fail a command.
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

### Against github.com

The comparison that matters: the same edits through each CLI (`git add/commit/push` to a
private github.com repository, `pail add/commit` to a GCS bucket), N writers on one branch,
20 commits each, from a residential client in Phoenix (RTT ~90 ms to github.com, ~35 ms to
storage.googleapis.com), 2026-10-20.

| Writers, one branch | git → github.com | pail → GCS |
|---|---|---|
| 1 | 0.64–0.74 commits/s, p50 1.4 s | **2.0 commits/s, p50 0.45 s** |
| 4 | 0.65–0.66 | **2.1** |
| 16 | 0.68–0.70 (7.9 pushes per commit, p95 44 s) | **3.6, p95 7.4 s, 0 failed** |

Every pail commit reported as landed was checked on main afterwards (exactly once each). The
library path, without a process per commit, reaches 7.1 commits/s with 16 writers. Both are
dominated by round trips to distant regions; from a machine in the bucket's region they are
tens of milliseconds each.

How a commit gets there:

```
uncontended   [pages ∥ commit object ∥ packs] ──▶ CAS ref        two round trips
              (the working copy remembers the ref's version: no read first)
contended     after two lost swaps, the writer PUTs an intent (its delta) under
              intents/<branch>/; the writer of the oldest pending intent combines:
              GET ref ∥ LIST intents → one commit per intent, in order, on the head
              → [all objects] → ONE CAS lands the batch, recording the intents in the
              ref's `recent` list (so none lands twice); waiting writers watch the ref,
              and take over if it stands still for 1.5 s
```

1. **Linear history, nothing squashed:** each intent is its own commit; an intent whose
   paths changed since its base is skipped and its writer gets the conflict (exit 4).
2. **No starvation:** a direct attempt lists pending intents while its objects upload, and
   joins the batch instead of racing past it. (Before this, 16 writers lost 5 of 320
   commits to a 60 s give-up.)
3. **Remembered heads are exact:** a commit that landed inside a batch does not become a
   known head (the ref's version belongs to the batch's tip); a regression test drives
   concurrent CLI writers and checks every reported commit is on main.

### Local measurements

Earlier results (2026-10-20, laptop under load; pail on the local S3 emulator, Git to a
**local** bare remote, so the Git push cost here is far below github.com's):

| Scenario | pail (S3 emulator) | Git (local bare remote) |
|---|---|---|
| Edit 2,000 files, one commit | 0.25 s, 53 PUTs | 4.2 s |
| Add 5,000 files | 0.27 s | 5.7 s |
| Delete 3,200-file dir / rename 2,100-file dir | 0.04 s / 0.05 s | 1.1 s / 0.8 s |
| Land 40 stacks while main moves, p50 | 118 ms | n/a |
| Change 1 MB of a 256 MB file | 3.8 MB uploaded | n/a |
| `log` 240 commits / `log -- path` | 20 ms / 30 ms | n/a |
| Clone a working copy (29k files) | 22 s | 9.8 s (`--shared`) |

Notes:

1. Writes are proportional to the change: a few pages per commit, content reused on rename.
2. A clone writes each file twice (into the shared content cache, then a CoW clone into the
   worktree). That is the price of worktrees costing ~0 extra disk.
3. `PAIL_TRACE_STORE=1` prints every store request with its timing; `PAIL_TRACE_COMBINE=1`
   prints each combining round.

## Tests

```sh
cargo test --release
# CAS contract against real object-store APIs:
PAIL_TEST_STORES="s3://pail/test" AWS_ENDPOINT=http://localhost:19000 AWS_ALLOW_HTTP=true \
  AWS_ACCESS_KEY_ID=x AWS_SECRET_ACCESS_KEY=x AWS_REGION=us-east-1 cargo test --release --test backends
```

Scenario tests (`tests/scenarios/*.t`) are shell transcripts of common Git workflows run in pail
(often beside Git itself): conflicts, feature branches, stacked PRs, interactive rebase.
`PAIL_BLESS=1 cargo test --test scenarios` records new output; see
[tests/scenarios/README.md](tests/scenarios/README.md).

Publishing is also modelled in Quint ([verify/](verify/README.md)): the model's properties are
checked over random interleavings of writers and lost CAS replies, and `tests/refinement.rs`
runs real clients through the model's scenarios one store request at a time. Store backends
and branch policy are checked against their models the same way (`tests/store_refinement.rs`,
`tests/policy_refinement.rs`).

## License

MIT. See [LICENSE](LICENSE).
