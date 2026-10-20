#!/usr/bin/env bash
# Differential test: `pail log` must print exactly what `git log --first-parent` prints.
#   GIT_DIR=<git repo> STORE=<pail store mirrored from it> scripts/gitlog-compat.sh [filter]
# Both sides get the same arguments and the same range (the mirrored history minus its root).
set -uo pipefail
PAIL=${PAIL:-$(dirname "$0")/../target/release/pail}
: "${GIT_DIR:?}" "${STORE:?}"
N=${N:-}           # limit commits for slow (patch) cases
OUT=${OUT:-$(mktemp -d)}; mkdir -p "$OUT"
export GIT_PAGER=cat PAGER=cat COLUMNS=80 TZ=${TZ:-America/Los_Angeles}
unset GIT_CONFIG_GLOBAL; export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
tip=$("$PAIL" --store "$STORE" --ids git log -1 --format=%H main)
root=$("$PAIL" --store "$STORE" --ids git log --reverse --format=%H main | head -1)
range="$root..$tip"
pass=0; fail=0; failed=()
run() {
  local name=$1; shift
  [[ -n "${FILTER:-}" && "$name" != *$FILTER* ]] && return
  # The range goes before any `--`, which starts the paths.
  local opts=() paths=() seen=0
  for a in "$@"; do if [[ $seen == 1 ]]; then paths+=("$a"); elif [[ $a == -- ]]; then seen=1; else opts+=("$a"); fi; done
  git --git-dir="$GIT_DIR" log --first-parent "${opts[@]}" "$range" ${paths[@]+-- "${paths[@]}"} > "$OUT/$name.git" 2>&1
  "$PAIL" --store "$STORE" --ids git log "${opts[@]}" "$range" ${paths[@]+-- "${paths[@]}"} > "$OUT/$name.pail" 2>&1
  if cmp -s "$OUT/$name.git" "$OUT/$name.pail"; then
    pass=$((pass+1)); printf '  ok    %-28s %8s bytes\n' "$name" "$(wc -c < "$OUT/$name.git" | tr -d ' ')"
  else
    fail=$((fail+1)); failed+=("$name")
    printf '  FAIL  %-28s (%s)\n' "$name" "$*"
    diff "$OUT/$name.git" "$OUT/$name.pail" | head -${CONTEXT:-8} | sed 's/^/        /'
  fi
}
A=--abbrev=10
run medium            $A
run oneline           $A --oneline
run short             $A --format=short
run full              $A --format=full
run fuller            $A --format=fuller
run reference         $A --format=reference
run placeholders      $A '--format=%H|%h|%P|%p|%an|%ae|%al|%cn|%ce|%cl|%s|%f'
run dates-ph          $A '--format=%ad|%aD|%at|%ai|%aI|%as|%cd|%cD|%ct|%ci|%cI|%cs'
run body              $A --format=%b
run raw-body          $A --format=%B
run format-sep        $A '--format=format:%h %s%n%+b'
run magic             $A '--format=%h%-b%n% s%x09%x41'
run date-iso          $A --date=iso
run date-iso-strict   $A --date=iso-strict
run date-rfc          $A --date=rfc
run date-short        $A --date=short
run date-raw          $A --date=raw
run date-unix         $A --date=unix
run date-local        $A --date=local
run date-iso-local    $A --date=iso-local
run date-format       $A '--date=format:%Y/%m/%d %H.%M' --format=%ad
run date-relative     $A --date=relative --format='%ad|%ar'
run parents           $A --parents --oneline
run no-abbrev         --oneline --no-abbrev-commit
run abbrev-commit     $A --abbrev-commit
run count-skip        $A --oneline -n 40 --skip=15
run dash-n            $A --oneline -7
run reverse           $A --oneline --reverse -n 25
run author            $A --oneline --author=calvin -i
run grep              $A --oneline --grep=fix
run grep-i            $A --oneline -i --grep=FIX --grep=test
run grep-all          $A --oneline --all-match --grep=fix --grep=test
run grep-invert       $A --oneline --invert-grep --grep=Merge
run grep-ere          $A --oneline -E '--grep=^(fix|feat)(\(|:)'
run no-merges         $A --oneline --no-merges
run merges            $A --merges
run since             $A --oneline --since=2026-10-01
run until             $A --oneline --until=2026-10-10
run path              $A --oneline -- services/rust
run path-glob         $A --oneline -- '*.toml'
run path-file         $A --format=%h -- package.json
run name-only         $A --name-only --no-renames
run name-status       $A --name-status --no-renames
run name-status-ren   $A --name-status
run oneline-ns        $A --oneline --name-status
run raw               $A --raw --no-renames
run summary           $A --summary
run numstat           $A --numstat
run shortstat         $A --shortstat --oneline
run stat              $A --stat
run stat-narrow       $A --stat=60
run path-full-diff    $A --name-status --full-diff -- services/rust
run patch             $A -p ${N:+-n $N}
run patch-stat        $A --stat -p ${N:+-n $N}
run patch-U1          $A -U1 ${N:+-n $N}
echo "$pass passed, $fail failed  (outputs in $OUT)"
[[ $fail -eq 0 ]]
