#require python3
Guard rails: the default branch is refused; a plan is JSON that can be edited and checked
with --dry-run first; commits added after a plan was made are carried; a plan whose tip was
rewritten meanwhile fails (exit 4); every rewrite is a reflog record.

  $ pail init $STORE >/dev/null && pail clone $STORE w >/dev/null && cd w
  $ commit a.txt 1 "one" >/dev/null && commit b.txt 2 "two" >/dev/null
  $ pail rebase -i HEAD~1
  error: main is the default branch: everyone builds on it. Rewrite a feature branch, or pass --rewrite-default
  [1]
  $ pail switch -c feat >/dev/null
  $ commit c.txt 3 "three" >/dev/null && commit d.txt 4 "four" >/dev/null

A plan as JSON: drop "three", check it, nothing moves.

  $ pail rebase --print-plan > plan.json && grep -c '"pick"' plan.json
  2
  $ python3 -c 'import json,sys; p=json.load(open("plan.json")); p["steps"][0]={"drop":p["steps"][0]["pick"]}; json.dump(p,open("plan.json","w"))'
  $ pail rebase --plan plan.json --dry-run
  dry run: nothing published
  rebased feat: #1 -> #2 (1 new commit(s), 1 dropped; epoch 1)
    #3 -> dropped  three
    #1 -> #2  four
  $ pail log --format=%s -1
  four

Someone commits after the plan was made: their commit is carried onto the new history.

  $ commit e.txt 5 "five" >/dev/null && pail rebase --plan plan.json
  rebased feat: #4 -> #5 (1 new commit(s), 1 dropped, 1 carried from others; epoch 1)
    #3 -> dropped  three
    #1 -> #2  four
    #4 -> #5  five
  working copy now at #5 (1 path(s) updated)
  $ pail log --format=%s
  five
  four
  two
  one
  Initial empty commit

A plan made before someone else rewrote the branch no longer applies:

  $ pail rebase --print-plan > old.json && pail rebase --undo >/dev/null
  $ python3 -c 'import json; p=json.load(open("old.json")); p["steps"][0]={"drop":p["steps"][0]["pick"]}; json.dump(p,open("old.json","w"))'
  $ pail rebase --plan old.json
  error: feat moved to #4 since #5 was read, and not by adding commits: plan again
  [4]
  $ pail log --format=%s
  five
  four
  three
  two
  one
  Initial empty commit

Two rewrites so far, each a record naming the one before it:

  $ ls $TMP/store/reflog/feat | wc -l | tr -d ' '
  2
  $ cat $TMP/store/reflog/feat/* | python3 -c 'import json,sys; [print(r["epoch"], r["op"], "prev" in r) for r in map(json.loads, sys.stdin.read().replace("}{","}\n{").splitlines())]'
  1 rebase False
  2 undo True
