Two people commit to main from the same base. In Git the second push is rejected until they
pull; in pail a commit is the push, and it lands on top of the other's when their paths
differ. When the paths overlap the second commit fails with exit 4 and nothing is published.

  $ pail init $STORE >/dev/null
  $ pail clone $STORE alice >/dev/null && pail clone $STORE bob >/dev/null
  $ cd alice && as alice && commit shared.txt v0 "base"
  [main #1] base
  $ cd ../bob && as bob && pail sync
  main @ #1 (1 path(s) updated)

Disjoint paths: bob's commit is replayed on alice's and alice's file appears in bob's copy.

  $ cd ../alice && commit a.txt alice "alice adds a"
  [main #2] alice adds a
  $ cd ../bob && commit b.txt bob "bob adds b"
  [main #3] bob adds b, landed after 2 attempts
  $ pail log --format='%an %s' && cat a.txt
  bob bob adds b
  bob alice adds a
  alice base
  Tester Initial empty commit
  alice

Same path: the stale writer gets a conflict, and the branch is untouched.

  $ cd ../alice && pail sync >/dev/null && commit shared.txt alice-v1 "alice edits shared"
  [main #4] alice edits shared
  $ cd ../bob && commit shared.txt bob-v1 "bob edits shared"
  error: conflict: 1 path(s) changed on main since your base: shared.txt
  hint: another writer changed the same paths; `pail sync`, then try again
  [4]
  $ pail log --format=%s -1 main
  alice edits shared

Recovery is Git's `pull` then retry: sync refuses while the local edit clashes, so the edit
is set aside (restore), then reapplied on the new base.

  $ pail sync
  error: sync would overwrite local changes in: shared.txt
  [1]
  $ pail unstage shared.txt >/dev/null && pail restore shared.txt >/dev/null && pail sync
  main @ #4 (1 path(s) updated)
  $ commit shared.txt bob-v2 "bob edits shared again" && pail log --format=%s
  [main #5] bob edits shared again
  bob edits shared again
  alice edits shared
  bob adds b
  alice adds a
  base
  Initial empty commit

The same in Git, for comparison: the push is rejected and `pull --rebase` stops on the
conflict, leaving the repository mid-rebase.

  $ cd .. && git init -q --bare remote.git && git clone -q remote.git ga 2>/dev/null && git clone -q remote.git gb 2>/dev/null
  $ cd ga && commit shared.txt v0 base && git push -q origin main 2>/dev/null && cd ../gb && git pull -q 2>/dev/null
  $ cd ../ga && commit shared.txt alice-v1 "alice edits shared" && git push -q origin main 2>/dev/null
  $ cd ../gb && commit shared.txt bob-v1 "bob edits shared" && git push origin main 2>&1 | grep -o '\[rejected\]'
  [rejected]
  $ git pull -q >/dev/null 2>&1; git status --short; test -d .git/rebase-merge && echo "mid-rebase"
  UU shared.txt
  mid-rebase
