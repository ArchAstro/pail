#require git
`rebase -i` with the same todo list in pail and Git: reorder, squash, fixup, drop. The
resulting histories and trees match. (`todo` names commits by subject; see the README.)

  $ pail init $STORE >/dev/null && pail clone $STORE p >/dev/null && ginit g
  $ for d in p g; do (cd $d
  >   commit base.txt base "base"
  >   if [ -d .git ]; then git switch -q -c feat; else pail switch -c feat; fi
  >   commit one.txt 1 "one"
  >   commit two.txt 2 "two"
  >   commit three.txt 3 "three"
  >   commit one.txt 1b "one, again"
  >   commit junk.txt x "junk"
  > ) >/dev/null; done

  $ todo <<EOF
  > pick one
  > fixup one, again
  > pick three
  > squash two
  > drop junk
  > EOF
  $ cd p && pail rebase -i main >/dev/null && echo rebased
  rebased
  $ cd ../g && git rebase -q -i main >/dev/null && cd ..
  $ diff <(cd $TMP/p && state) <(cd $TMP/g && state) && cd p && state
  three
  one
  base
  Initial empty commit
  --
  base.txt: base
  one.txt: 1b
  three.txt: 3
  two.txt: 2

Squash kept both messages, fixup dropped its own, as in Git. One known difference: `pail
commit -m one` stores "one" where Git stores "one\n", so a plain commit's %B is one line
shorter than Git's:

  $ diff <(cd $TMP/p && pail log --format=%B -2) <(cd $TMP/g && git log --format=%B -2) && echo same
  5a6
  > 
  [1]
  $ pail log --format=%B -2
  three
  
  two
  
  one

Rewording from the todo text (pail only: the line's text becomes the subject; Git opens an
editor instead):

  $ todo <<EOF
  > pick one
  > reword three|Three, squashed with two
  > EOF
  $ pail rebase -i main >/dev/null && pail log --format=%s
  Three, squashed with two
  one
  base
  Initial empty commit
