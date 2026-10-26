The stage, as in Git: `add` snapshots content, later edits stay unstaged until added again,
and `commit -a` stages tracked files only. Run the same session in Git and compare.

  $ pail init $STORE >/dev/null && pail clone $STORE p >/dev/null && ginit g

  $ for d in p g; do (cd $d
  >   commit README v1 "readme"
  >   printf 'v2\n' > README
  >   printf 'new\n' > new.txt
  >   if [ -d .git ]; then git commit -q -a -m "tracked only"; else pail commit -a -m "tracked only"; fi
  > ) >/dev/null; done

`commit -a` took README's edit, not the untracked file:

  $ cd p && state
  tracked only
  readme
  Initial empty commit
  --
  README: v2
  new.txt: new
  $ cd ../g && state
  tracked only
  readme
  Initial empty commit
  --
  README: v2
  new.txt: new
  $ cd .. && diff <(cd $TMP/p && state) <(cd $TMP/g && state) && echo same
  same

Staged content is the snapshot from `add`, not what is on disk at commit time:

  $ cd p && printf 'staged\n' > README && pail add README && printf 'later\n' > README
  staged 1 change(s)
  $ pail status
  on main @ #1
  
  staged:
    M README
  
  not staged:
    M README
  
  untracked:
    ? new.txt
  $ pail commit -m "snapshot" && pail cat README
  [main #2] snapshot
  staged
  $ pail status
  on main @ #2
  
  not staged:
    M README
  
  untracked:
    ? new.txt
