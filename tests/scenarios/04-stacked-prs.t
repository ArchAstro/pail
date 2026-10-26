#require python3
Stacked pull requests with the pail-pr extension: pr/b is built on pr/a, pr/c on pr/b. Review
asks for a change in pr/a; its author amends it with a fixup and rewrites pr/a. The PRs above
it are restacked onto the new pr/a, then everything merges bottom-up, each landing only its
own commits.

`pr` runs the extension against the store. (PAIL_STORE only for it: with PAIL_STORE set,
pail itself talks to the store directly and ignores the working copy.)

  $ export PATH="$EXTENSIONS:$PATH" && pr() { PAIL_STORE=$STORE pail-pr "$@"; }
  $ pail init $STORE >/dev/null && pail clone $STORE dev >/dev/null && cd dev
  $ commit base.txt base "base" >/dev/null
  $ pail switch -c pr/a >/dev/null && commit a.txt a1 "a: parser" >/dev/null
  $ pail switch -c pr/b >/dev/null && commit b.txt b1 "b: uses parser" >/dev/null
  $ pail switch -c pr/c >/dev/null && commit c.txt c1 "c: docs" >/dev/null
  $ pr open pr/a --title "Parser" && pr open pr/b --title "Use it" --base pr/a && pr open pr/c --title "Docs" --base pr/b
  opened PR 1: pr/a → main
  opened PR 2: pr/b → pr/a
  opened PR 3: pr/c → pr/b
  $ pr list
  #1    open    — Parser   (pr/a → main, Tester)
  #2    open    — Use it   (pr/b → pr/a, Tester)
  #3    open    — Docs   (pr/c → pr/b, Tester)

Review feedback on pr/a: a fixup commit, folded in with --autosquash. pail names the PRs left
on the old pr/a.

  $ pail switch pr/a >/dev/null && commit a.txt a2 "fixup! a: parser" >/dev/null
  $ pail rebase main --autosquash
  rebased pr/a: #1 -> #2 (1 new commit(s), 0 dropped; epoch 1)
    #3 -> #2  a: parser
    #1 -> #2  fixup! a: parser
  hint: pr/b is built on the old pr/a: `pail rebase pr/a --branch pr/b`
  hint: pr/c is built on the old pr/a: `pail rebase pr/a --branch pr/c`
  working copy now at #2 (0 path(s) updated)
  $ pail log --format=%s
  a: parser
  base
  Initial empty commit

Restack: each PR's own commits move onto the rewritten one below it (the fork point comes
from pr/a's rewrite record, so pr/a's old commits are not replayed again).

  $ pail rebase pr/a --branch pr/b && pail rebase pr/b --branch pr/c
  rebased pr/b: #4 -> #5 (1 new commit(s), 0 dropped; epoch 1)
    #4 -> #5  b: uses parser
  hint: pr/c is built on the old pr/b: `pail rebase pr/b --branch pr/c`
  rebased pr/c: #6 -> #7 (1 new commit(s), 0 dropped; epoch 1)
    #6 -> #7  c: docs
  $ pail log --format=%s pr/c && pail cat pr/c:a.txt
  c: docs
  b: uses parser
  a: parser
  base
  Initial empty commit
  a2

Merge bottom-up. PR 1 lands onto main with its rewritten commit.

  $ as rev && pr review 1 --approve >/dev/null && as dev
  $ pr merge 1 --force | tail -1
  merged PR 1 into main @ #2

PR 2 still targets pr/a, as recorded when it was opened: pail-pr does not retarget a stacked
PR when its base merges (a forge would). Merging it lands onto pr/a, not main:

  $ pr merge 2 --force | tail -1
  merged PR 2 into pr/a @ #5
  $ pail log --format=%s -1 main
  a: parser

Landing the rest onto main directly lands each branch's own commits only:

  $ pail land pr/c --stack
  landed 2 commit(s) from pr/c onto main @ #7 (fast-forward)
    #5 b: uses parser
    #7 c: docs
  $ pail log --format=%s main
  c: docs
  b: uses parser
  a: parser
  base
  Initial empty commit
