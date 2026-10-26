# Scenario tests

Each `*.t` file is a transcript of a shell session using pail, often beside the same session
in Git. `cargo test --test scenarios` replays every file against the freshly built `pail` and
fails when any output differs. It's the format Mercurial and Sapling test their own VCS with
(cram): the test is also readable documentation of the behavior.

```
Prose explains the scenario (any line not indented two spaces).

  $ cd alice && commit a.txt v1 "add a"       command (bash; cwd and variables persist)
  [main #1] add a                             expected output: stdout and stderr, in order
  $ pail land clash --stack
  error: conflict: 1 path(s) changed on main since your base: app.txt
  ...
  [4]                                         exit status, when not 0
  $ todo <<EOF                                `  > ` continues a command
  > pick one
  > EOF
```

## Writing one

1. Copy the closest scenario. Write the prose and the `$` commands; leave the output empty.
2. `PAIL_BLESS=1 cargo test --test scenarios` fills in the output. `PAIL_SCENARIO=stacked`
   runs only the files whose name contains `stacked`.
3. **Read every line it wrote.** Blessing records what pail does, right or wrong. Each output
   should be what you'd tell a user to expect. If it isn't, you found a bug: fix pail, or keep
   the output and say in the prose that it's a known gap (see `04-stacked-prs.t`).
4. Commit the `.t` file. CI runs it with the other tests.

## The environment

Every file runs as one `bash` session in a fresh temporary directory (`$TMP`), with `pail` and
`extensions/` (`$EXTENSIONS`) on `PATH`, and nothing from the user's environment:

| Name | What it is |
|---|---|
| `$STORE` | `file://$TMP/store`: `pail init $STORE`, `pail clone $STORE alice` |
| `as <name>` | the next commits are by `<name> <name@example.com>`, in pail and Git |
| `commit <path> <text> <msg>` | write the file, stage everything, commit; pail or Git by cwd |
| `ginit <dir>` | a Git repository that starts like a pail one (an empty first commit) |
| `state [<rev>]` | history subjects, then every file and its first line, the same in pail and Git |
| `todo <<EOF` | the next `rebase -i` (pail or Git) gets this todo: `<verb> <subject>[\|<text>]` |
| `#require git python3` | a prose line: skip the file when a tool is missing |

Dates are fixed (`PAIL_DATE`, `GIT_*_DATE`), so ids depend only on content. Editors are `true`,
so nothing waits for input.

## Output normalization

- `$TMP` replaces the temporary directory.
- Every id (hex, 7 or more characters, with a digit) becomes `#n`, numbered by first
  appearance and keyed by its first 7 characters. So a short and a full id of the same
  commit get the same label. Transcripts show *which* commits match, not their hashes.
- An expected line ending in ` (glob)` matches with `*` as a wildcard.

## Parity with Git

Run the same steps in a pail working copy and a Git repository, then compare. Use absolute
paths: a `cd` that fails inside `<( … )` gives empty output on both sides, which would
compare equal:

```
  $ for d in p g; do (cd $d; commit one.txt 1 "one"; …) >/dev/null; done
  $ diff <(cd $TMP/p && state) <(cd $TMP/g && state) && echo same
  same
```

Where pail differs from Git on purpose (commits publish, conflicts don't leave a stopped
state), show both and say why in the prose (`02-concurrent-writers.t`,
`06-rebase-conflicts.t`).

## Coverage

| File | Git operation | What it pins down |
|---|---|---|
| `01-stage-and-commit` | `add`, `commit -a`, `status` | the stage is a snapshot; parity with Git |
| `02-concurrent-writers` | `push` / `pull --rebase` | disjoint commits replay, same-path commits fail with exit 4, recovery; Git stops mid-rebase |
| `03-feature-branch-vs-main` | `merge --squash`, `rebase main` + fast-forward | land squashed / `--stack`, landing twice is a no-op, conflicts, catching up with `rebase` |
| `04-stacked-prs` | stacked PRs, `rebase --update-refs` | fixup + `--autosquash` on the bottom PR, restacking, bottom-up merges; pail-pr doesn't retarget stacked PRs yet |
| `05-rebase-interactive` | `rebase -i` | reorder / squash / fixup / drop with identical results in Git; reword from the todo |
| `06-rebase-conflicts` | `rebase -i` conflict, already-applied commits | exit 4, nothing published, vs Git's stopped rebase; empty commits are dropped |
| `07-rewritten-under-you` | someone force-pushed your branch | sync/commit notes with the epoch, files follow, `--undo` carries later commits |
| `08-rewrite-safety` | `push --force-with-lease` | default branch refused, JSON plans, `--dry-run`, carried commits, a plan stale after a rewrite fails, reflog records |
