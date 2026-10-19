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
├─ commits/<blake3>      JSON commit objects
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
