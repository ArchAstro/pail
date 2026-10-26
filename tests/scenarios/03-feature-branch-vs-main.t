A feature branch while main moves on: land it squashed or commit by commit, see what
conflicts, and bring it up to date with `rebase` (Git: `rebase main`, then a fast-forward).

  $ pail init $STORE >/dev/null && pail clone $STORE w >/dev/null && cd w
  $ commit app.txt v1 "app" >/dev/null
  $ pail switch -c feature >/dev/null
  $ commit feature.txt f1 "feature: one" >/dev/null && commit feature.txt f2 "feature: two" >/dev/null
  $ pail switch main >/dev/null && commit other.txt o1 "main moves" >/dev/null

Landing the stack replays it on main's new head, one atomic write; landing again is a no-op.

  $ pail land feature --stack
  landed 2 commit(s) from feature onto main @ #1 (replayed on the new head)
    #2 feature: one
    #1 feature: two
  $ pail land feature --stack
  feature has nothing to land
  $ pail sync >/dev/null && pail log --format=%s
  feature: two
  feature: one
  main moves
  app
  Initial empty commit

Squashed instead: one commit naming the branch's subjects.

  $ pail switch -c sq >/dev/null && commit sq.txt s1 "sq: one" >/dev/null && commit sq.txt s2 "sq: two" >/dev/null
  $ pail switch main >/dev/null && pail land sq && pail sync >/dev/null
  landed sq onto main @ #3
  $ pail log --format=%B -1
  Land sq
  
  * sq: two
  * sq: one

A branch and main both change the same file: landing conflicts (exit 4), main is untouched.

  $ pail switch -c clash >/dev/null && commit app.txt from-branch "clash: edit app" >/dev/null
  $ pail switch main >/dev/null && commit app.txt from-main "main: edit app" >/dev/null
  $ pail land clash --stack
  error: conflict: 1 path(s) changed on main since your base: app.txt
  hint: another writer changed the same paths; `pail sync`, then try again
  [4]
  $ pail log --format=%s -1
  main: edit app

Updating a branch from main is `rebase`: its own commits replayed on main's tip. Disjoint
work replays cleanly, then lands as a fast-forward (main's commits are the branch's).

  $ pail switch -c late >/dev/null && commit late.txt l1 "late: one" >/dev/null
  $ pail switch main >/dev/null && commit main2.txt m2 "main moves again" >/dev/null
  $ pail rebase main --branch late
  rebased late: #4 -> #5 (1 new commit(s), 0 dropped; epoch 1)
    #4 -> #5  late: one
  $ pail land late --stack
  landed 1 commit(s) from late onto main @ #5 (fast-forward)
    #5 late: one
