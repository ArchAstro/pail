#require git
Reordering two commits that edit the same file conflicts. Git stops mid-rebase and waits for
`--continue` or `--abort`; pail publishes nothing and leaves the branch as it was (exit 4).

  $ pail init $STORE >/dev/null && pail clone $STORE p >/dev/null && ginit g
  $ for d in p g; do (cd $d
  >   commit base.txt base "base"
  >   if [ -d .git ]; then git switch -q -c feat; else pail switch -c feat; fi
  >   commit f.txt 1 "f=1"
  >   commit f.txt 2 "f=2"
  > ) >/dev/null; done
  $ todo <<EOF
  > pick f=2
  > pick f=1
  > EOF

  $ cd p && before=$(pail rev-parse HEAD) && pail rebase -i main
  error: applying #1 f=2: conflict: 1 path(s) changed on feat since your base: f.txt
  hint: nothing was published; change the plan (or the commits) and retry
  [4]
  $ test "$(pail rev-parse HEAD)" = "$before" && pail status && echo unchanged
  on feat @ #1
  nothing to commit, working copy clean
  unchanged

  $ cd ../g && git rebase -q -i main >/dev/null 2>&1; echo "exit $?"
  exit 1
  $ test -d .git/rebase-merge && echo "git is mid-rebase" && git rebase --abort
  git is mid-rebase

A commit whose change the new base already has is dropped (Git: "skipped previously applied
commit"):

  $ cd ../p && pail switch main >/dev/null && commit f.txt 1 "f=1 on main" >/dev/null
  $ pail rebase main --branch feat
  rebased feat: #1 -> #2 (1 new commit(s), 1 dropped; epoch 1)
    #3 -> dropped (already applied)  f=1
    #1 -> #2  f=2
  $ pail log --format=%s feat
  f=2
  f=1 on main
  base
  Initial empty commit
