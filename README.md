# pail

Version control that lives in a bucket. Clients talk to S3, GCS or a local directory
directly. There is no server.

- **Linear history.** Branches move only by compare-and-swap (S3 `If-Match`, GCS
  `ifGenerationMatch`). There are no merge commits; landing a branch squashes it.
- **Git-style stage.** `add`, `add -A`, `commit -a` and `restore` behave as in Git. The stage
  stores only the staged delta, so its size depends on the change, not the repo.
- **Five-prefix layout.** Everything except `refs/` is immutable and content-addressed.

```
<store>/
├─ packs/<blake3>.pack   small files, concatenated, read with range GETs
├─ chunks/<blake3>       large files, FastCDC chunks, deduped globally
├─ pages/<blake3>        manifest pages (prolly tree over sorted paths)
├─ commits/<blake3>      JSON commit objects, each with its change list
├─ refs/<branch>         {"commit": "<hash>"}, updated only by CAS
└─ reflog/<branch>/…     written only when a branch is force-moved
```

## CLI

```sh
pail init file:///tmp/bucket                  # or s3://bucket/prefix, gs://bucket/prefix
pail clone file:///tmp/bucket work && cd work
echo hi > a.txt && pail add -A && pail commit -m "first"
pail status | pail diff [--staged] | pail log | pail ls | pail cat <path>
pail switch -c feature … pail commit -a -m wip
pail land feature --onto main && pail sync
```

A commit that loses the CAS race is replayed on the new head when the paths don't overlap.
If they overlap, the commit stops with a conflict.

S3 settings come from the usual `AWS_*` variables (`AWS_ENDPOINT`, `AWS_ALLOW_HTTP` for
emulators). GCS settings come from `GOOGLE_*`. `PAIL_CACHE` sets the local cache of
immutable pages and commits (`off` disables it).

## Worktrees: many checkouts, one copy on disk

```
             bucket ──fetch once──▶ ~/.cache/pail/blobs/<blake3>   (read-only, one copy per content)
                                         │ clone   │ clone   │ clone      APFS clonefile / Linux FICLONE
                                         ▼         ▼         ▼
                                       wt/a      wt/b      wt/c           share blocks until edited
seed: wt/a/target, node_modules … ──clone──▶ wt/b/…                       warm build, no extra disk
```

```sh
pail worktree add ../feature -b feature      # new branch at this base, seeded from here
pail worktree add ../review --branch main --seed-from ~/src/other-checkout
pail worktree list | pail worktree remove ../feature
```

1. Files are copy-on-write clones of a machine-local blob store, so extra worktrees cost
   inodes, not bytes. Editing a file in place is safe: the clone gets its own blocks. The store
   is a plain cache (no refcounts), and it lives on the worktree's volume
   (`PAIL_BLOBS`, `PAIL_HOME`). If the filesystem can't clone, files are copied.
2. `--seed-from` (default: the current worktree) clones ignored build output into the new
   worktree: `target/`, `node_modules/`, `_build/`, `.next/`, `.env*` … (override with a
   `.pailseed` file in gitignore syntax). Nested checkouts and other ignored paths are skipped.
3. Files identical to the seed source get its mtimes, so cargo/make see the seeded build as up
   to date. Files that differ get a fresh mtime and rebuild normally.

Measured on firstlanding (28,877 files, 459 MB; APFS):

| | real disk | time |
|---|---|---|
| plain clone | 529 MB | 4.9s |
| `worktree add` | 26 MB | 2.4s |
| `worktree add` seeded with archdev's 10 GB `target/` | 29 MB | 3.6s |
| `cargo build` in it (cold build: 446 crates, 147s) | 0 MB, 0 crates | 0.6s |
| same, but target cloned without mtime matching | 5.6 GB, 8 crates | 112s |
| seeded from a real checkout: 140 `node_modules`/`_build`/`.next`, 5.3 GB apparent | 157 MB | 15s |

## Reading history: Git's interface, any commit, bare or not

Every read command runs against a working copy or straight against a store (a bare
repository): `--store <url>`, `PAIL_STORE`, or a directory path. A bare store's HEAD is its
default branch (set by `pail init`).

```sh
pail --store s3://bucket/repo log --oneline -20 -- services/rust
pail --store s3://bucket/repo log -p --stat main~10..main
pail --store ./repo.pail show HEAD~3 --name-status
pail --store gs://bucket/repo ls-tree -r -l HEAD:src
pail --store gs://bucket/repo cat-file -p HEAD:package.json
printf 'HEAD:Cargo.toml\nHEAD:src\n' | pail --store … cat-file --batch
pail --store … rev-parse --short=12 HEAD HEAD:src
pail --store … ls -l HEAD:src  |  pail --store … cat HEAD~2:README.md
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

### Commits carry their change lists

Like Mononoke's Bonsai changesets, every commit records the files it changed against its
parent: path → old (kind, size, content hash) and new (kind, size, content hash, location),
or a deletion. It is computed at commit time against the parent the commit actually lands on
(so replayed commits are right), covered by the commit hash, and checked by pail-mirror's
proof against the manifest diff for every commit. `log`, `show` and `fetch --history` read
it instead of diffing manifests; commits written before change lists fall back to diffing.
The list stays raw JSON until read, so walking history doesn't parse it.

| (warm, median ms) | manifest diffs | change lists | git |
|---|---|---|---|
| `log --name-status -n 200` | 169 | 24 | 33 |
| `log -- services/rust` (2,221 commits) | 1,623 | 122 | 308 |
| `log --stat -n 200` | 343 | 187 | 124 |

### Local content cache

Like a bare Git repository's object database, reads keep what they fetch: file contents go
into `~/.cache/pail/blobs/<blake3>` (the same store working copies clone from), commits and
manifest pages into `~/.cache/pail/objects`. Everything is content-addressed and verified on
read, so the cache never goes stale.

```sh
pail --store s3://bucket/repo fetch                 # every file at HEAD
pail --store s3://bucket/repo fetch --history       # every version along HEAD's history
pail --store … fetch --all --history -n 500         # all branches, last 500 commits each
pail cache stats                                    # sizes and locations
pail cache prune --max-size 20G                     # drop least recently used contents
```

`PAIL_HOME` moves it; `PAIL_BLOBS=off` turns content caching off.

On firstlanding (2,203 commits), `fetch --history` pulled 8,092 file versions (259 MB) in 22s.
With the cache warm:

| | pail | git |
|---|---|---|
| `log --oneline` (all) | 0.12s | 0.28s |
| `log -p -n 200` | 0.47s | 0.18s |
| `log --name-status -- <dir>` (all) | 1.5s | 0.45s |
| `ls-tree -r HEAD` | 0.19s | 0.08s |
| `cat-file -p HEAD` | 0.15s | 0.05s |

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
