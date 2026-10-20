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
├─ config.json             {"store": "s3://bucket/repo", "bare": true}
├─ HEAD                    the store's default branch, remembered
├─ objects/{commits,pages} verified copies of store objects
├─ blobs/<hh>/<blake3>     file contents: read cache and the source checkouts clone from
├─ graph/                  commit graph: metadata, changed paths, id index (mmap)
├─ index/                  Git blob ids by content hash, Git tree ids by manifest root
└─ worktrees.json          working copies attached to this repository

work/.pail/                a working copy's own state, next to (or instead of) the repository:
├─ HEAD.json               {"branch", "base"}
├─ stage.json, pending/    the stage: staged delta + snapshotted contents
├─ statcache.json          stat cache for fast status
└─ repo                    linked working copies only: path of the shared repository
```

Everything in it is derived from immutable store objects and verified on read, so it never
goes stale and any part can be deleted. Reading a store with `--store <url>` keeps nothing.

## CLI

```sh
pail init s3://bucket/repo                        # or gs://…, file://…, a directory
pail clone s3://bucket/repo work && cd work       # working copy; its .pail/ is the repository
echo hi > a.txt && pail add -A && pail commit -m "first"
pail status | pail diff [--staged] | pail log | pail ls | pail cat <path>
pail switch -c feature … pail commit -a -m wip
pail land feature --onto main && pail sync
pail clone --bare s3://bucket/repo repo.pail      # repository without a working copy
pail --repo repo.pail log --oneline               # or cd repo.pail; or PAIL_DIR
```

A commit that loses the CAS race is replayed on the new head when the paths don't overlap.
If they overlap, the commit stops with a conflict.

### Stacks

Commit as often as you like on a branch, then land the whole stack on main:

```sh
pail switch -c feature
pail commit -a -m "step 1" … pail commit -a -m "step 2" … pail commit -a -m "step 3"
pail land feature --stack            # every commit, in order, onto main
pail land feature                    # or: one squashed commit
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

## Tests

```sh
cargo test --release
# CAS contract against real object-store APIs:
PAIL_TEST_STORES="s3://pail/test" AWS_ENDPOINT=http://localhost:19000 AWS_ALLOW_HTTP=true \
  AWS_ACCESS_KEY_ID=x AWS_SECRET_ACCESS_KEY=x AWS_REGION=us-east-1 cargo test --release --test backends
```
