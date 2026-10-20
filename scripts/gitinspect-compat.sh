#!/usr/bin/env bash
# Differential test for inspection commands: `pail show|cat-file|ls-tree|rev-parse` must
# print exactly what Git prints for the same arguments, on several commits of mirrored history.
#   GIT_DIR=<git repo> STORE=<pail store mirrored from it> scripts/gitinspect-compat.sh
set -uo pipefail
PAIL=${PAIL:-$(dirname "$0")/../target/release/pail}
: "${GIT_DIR:?}" "${STORE:?}"
# pail reads through a local bare repository for the store (REPO, else one made under OUT).
OUT=${OUT:-$(mktemp -d)}; mkdir -p "$OUT"
REPO=${REPO:-$OUT/repo.pail}
[[ -d $REPO ]] || "$PAIL" clone --bare "$STORE" "$REPO" >/dev/null
export GIT_PAGER=cat PAGER=cat COLUMNS=80 GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
P() { "$PAIL" --repo "$REPO" --ids git "$@"; }
G() { git --git-dir="$GIT_DIR" "$@"; }
pass=0; fail=0
run() {
  local name=$1; shift
  [[ -n "${FILTER:-}" && "$name" != *$FILTER* ]] && return
  G "$@" > "$OUT/$name.git" 2>&1 < "${STDIN:-/dev/null}"
  P "$@" > "$OUT/$name.pail" 2>&1 < "${STDIN:-/dev/null}"
  if cmp -s "$OUT/$name.git" "$OUT/$name.pail"; then
    pass=$((pass+1)); printf '  ok    %-34s %9s bytes\n' "$name" "$(wc -c < "$OUT/$name.git" | tr -d ' ')"
  else
    fail=$((fail+1)); printf '  FAIL  %-34s (%s)\n' "$name" "$*"
    diff "$OUT/$name.git" "$OUT/$name.pail" | head -${CONTEXT:-8} | sed 's/^/        /'
  fi
}
tip=$(P log -1 --format=%H main)
for back in ${DEPTHS:-0 37 400 1500}; do
  c=$(P rev-parse "$tip~$back" 2>/dev/null) || { echo "  skip  history is shorter than $back commits"; continue; }
  t=c$back
  run $t-rev-parse          rev-parse "$c" "$c~3" "$c:package.json" "$c^{tree}" "$c:services/rust" "$c:"
  run $t-rev-parse-short    rev-parse --short=12 "$c" "$c:services"
  run $t-cat-commit         cat-file -p "$c"
  run $t-cat-commit-ts      cat-file -s "$c"
  run $t-cat-tree-root      cat-file -p "$c^{tree}"
  run $t-cat-tree-dir       cat-file -p "$c:services/rust"
  run $t-cat-tree-size      cat-file -s "$c:services"
  run $t-cat-blob           cat-file -p "$c:package.json"
  run $t-cat-blob-type      cat-file blob "$c:.gitignore"
  run $t-cat-types          cat-file -t "$c:services"
  run $t-ls-tree            ls-tree "$c"
  run $t-ls-tree-r          ls-tree -r "$c"
  run $t-ls-tree-rt         ls-tree -r -t "$c" services/rust
  run $t-ls-tree-d          ls-tree -d "$c"
  run $t-ls-tree-rd         ls-tree -r -d "$c" services
  run $t-ls-tree-l          ls-tree -l "$c" services/rust/
  run $t-ls-tree-rl         ls-tree -r -l --abbrev=10 "$c" services/rust/archdev/src/
  run $t-ls-tree-names      ls-tree --name-only -r "$c" infra
  run $t-ls-tree-paths      ls-tree "$c" services services/rust/ package.json services/go/archdev/go.mod
  run $t-ls-tree-subtree    ls-tree "$c:services/rust/archdev"
  run $t-ls-tree-z          ls-tree -z -r "$c" docs
  run $t-ls-tree-obj        ls-tree --object-only "$c" services/
  run $t-show-commit        show --abbrev=10 "$c"
  run $t-show-stat          show --stat --format=fuller "$c"
  run $t-show-s             show -s --oneline --abbrev=10 "$c" "$c~1"
  run $t-show-tree          show "$c:services/rust" "$c:"
  run $t-show-blob          show "$c:package.json"
  run $t-show-mixed         show --abbrev=10 "$c:services/rust" "$c" "$c:.gitignore"
  run $t-show-path          show --name-status "$c" -- services
  G ls-tree -r --name-only "$c" | awk 'NR % 97 == 1' | sed "s|^|$c:|" > "$OUT/names-$t"
  printf '%s\n' "$c" "$c^{tree}" "$c:services" "$c:nope" >> "$OUT/names-$t"
  STDIN="$OUT/names-$t" run $t-batch-check cat-file --batch-check
  STDIN="$OUT/names-$t" run $t-batch       cat-file --batch
done
run err-path    cat-file -p "$tip:nope"
run err-rev     rev-parse nope
run err-ls-tree ls-tree nope
echo "$pass passed, $fail failed  (outputs in $OUT)"
[[ $fail -eq 0 ]]
