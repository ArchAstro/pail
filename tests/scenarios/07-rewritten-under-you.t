Someone rewrites a branch you have checked out. Nobody is asked first; your next sync or
commit says what happened, and your files move by tree diff, local edits kept.

  $ pail init $STORE >/dev/null && pail clone $STORE alice >/dev/null && cd alice && as alice
  $ commit base.txt base "base" >/dev/null && pail switch -c feat >/dev/null
  $ commit one.txt 1 "one" >/dev/null && commit two.txt 2 "two" >/dev/null
  $ cd .. && pail clone $STORE bob -b feat >/dev/null && cd bob && as bob
  $ printf 'wip\n' > wip.txt

Alice drops "two" and rewords "one".

  $ cd ../alice && as alice && todo <<EOF
  > reword one|One
  > EOF
  $ pail rebase -i main
  rebased feat: #1 -> #2 (1 new commit(s), 1 dropped; epoch 1)
    #3 -> #2  one
    #1 -> dropped  two
  working copy now at #2 (1 path(s) updated)

Bob syncs: told, files follow, his untracked work stays.

  $ cd ../bob && as bob && pail sync
  note: feat was rewritten by alice <alice@example.com> (epoch 0 -> 1); your base #1 is gone from it, its place in the new history is #2
  feat @ #2 (1 path(s) updated)
  $ state
  One
  base
  Initial empty commit
  --
  base.txt: base
  one.txt: 1
  wip.txt: wip

Bob commits from a stale base again later: replayed, and told again.

  $ cd ../alice && as alice && commit three.txt 3 "three" >/dev/null && todo <<EOF
  > reword One|ONE
  > pick three
  > EOF
  $ pail rebase -i main >/dev/null
  $ cd ../bob && as bob && pail add wip.txt >/dev/null && pail commit -m "bob: wip"
  note: feat was rewritten by alice <alice@example.com> (epoch 1 -> 2); your base #2 is gone from it, its place in the new history is #4
  [feat #5] bob: wip, landed after 2 attempts
  $ pail log --format='%an %s'
  bob bob: wip
  alice three
  alice ONE
  alice base
  Tester Initial empty commit

Alice undoes her last rewrite; bob's commit, made since, is carried.

  $ cd ../alice && pail rebase --undo && pail log --format='%an %s'
  undid the last rewrite of feat: #5 -> #6 (0 new commit(s), 0 dropped, 1 carried from others; epoch 3)
    #4 -> #2  ONE
    #7 -> #8  three
    #5 -> #6  bob: wip
  working copy now at #6 (1 path(s) updated)
  bob bob: wip
  alice three
  alice One
  alice base
  Tester Initial empty commit
