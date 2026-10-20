---
name: pail
description: "Use pail, a version control system that lives in an object store (S3, GCS, file://), to inspect history, stage, commit, branch, land and manage working copies. Use when the user mentions pail, a `.pail/` directory is present, or a repository's store is an s3://, gs:// or file:// URL. Committing publishes immediately: there is no push."
---

# pail

pail is version control whose entire repository lives in an object store. There is no
server and no push: `pail commit` moves the branch in one atomic compare-and-swap that every
other clone sees at once. History reads take Git's options and print Git's output.

```
store (s3://, gs://, file://)                        this machine
refs/<branch> ──▶ commit ──▶ pages ──▶ packs/chunks   .pail/  stage, caches, commit graph
  (one CAS)        (immutable, BLAKE3-named)          working copies (copy-on-write)
```

## Learn the installed CLI

The binary is the authority for syntax:

```bash
pail --help                # grouped commands, exit codes
pail help <command>        # full help with examples, e.g. pail help land
```

Check where you are before acting:

```bash
pail status                # branch, base, staged / unstaged / untracked
pail branch                # branches; * is this working copy's
pail log --oneline -10
```

## Rules that differ from Git

1. **Commit = publish.** `pail commit` is `git commit` + `git push` in one atomic step. Only
   commit what the user asked you to commit, and never "commit to try something".
2. **No merge commits.** History is linear. Bring a branch in with `pail land <branch>`
   (squashed) or `pail land <branch> --stack` (every commit, one atomic write).
3. **Concurrent writers are normal.** If the branch moved since your base, a commit on
   different paths is replayed on top automatically. Exit code 4 means another writer changed
   the same paths: run `pail sync`, re-check the files, and commit again. Do not force.
4. **Stage is a snapshot.** `pail add` captures content at that moment; re-add after edits.
5. **`pail sync`** is a fast-forward pull. It refuses to overwrite local changes.

## Everyday flow

```bash
pail status
pail add src/parser.rs            # or: pail add -A
pail diff --staged                # A|M|D per staged path
pail commit -m "Fix the parser"   # committed AND published
```

Feature branch, then land:

```bash
pail switch -c fix                # new branch at the current base
pail add -A && pail commit -m "Fix"
pail land fix --stack             # onto the default branch; safe to re-run
pail switch main && pail sync
```

Extra working copies share contents and build output copy-on-write, so they are cheap:

```bash
pail worktree add ../app-review --branch feature
pail worktree list
pail worktree remove ../app-review      # refuses with uncommitted changes unless --force
```

## Reading history (Git-compatible)

```bash
pail log --oneline -20 -- src/
pail log -p -1
pail show HEAD~1 --stat
pail diff main feature --name-only
pail cat feature:src/main.rs
pail cat-file -p HEAD:README.md
pail rev-parse HEAD ORIG_HEAD
```

`--ids git` prints the Git ids of commits mirrored from Git. Use `-P` (`--no-pager`) or a pipe
so output is not paged.

## Hooks

`core.hooksPath` (often a checked-in directory) holds executables named after events:
`pre-commit`, `commit-msg`, `pre-publish`, `post-commit`, `post-rewrite`, `post-checkout`,
`post-sync`.

- Exit code **3** means a hook refused. Its output is on stderr; fix what it reports (for
  example formatting or failing tests) and retry. Nothing was published.
- `--no-verify`/`-n` skips the refusing hooks. Use it only when the user explicitly asks.
- `PAIL_TRACE_HOOKS=<file>` appends one JSON line per hook run.

## Exit codes

| Code | Meaning | What to do |
|---|---|---|
| 0 | ok | |
| 1 | error | read stderr |
| 2 | usage error | check `pail help <command>` |
| 3 | a hook refused | fix what the hook reported, retry |
| 4 | conflict: another writer changed the same paths | `pail sync`, re-check, commit again |
| 128 | fatal from a Git-compatible command | as `git` |

## Safety

- Never run `pail gc` without `--dry-run` unless the user asked; it deletes objects.
- Never `pail branch -d` or `pail worktree remove --force` work you did not create.
- `pail config --global` changes the user's identity for every repository; ask first.
- Prefer `pail status` and `pail diff --staged` before every commit.
