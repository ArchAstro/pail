// Package fakepail writes a shell script that stands in for `pail` in tests: it implements
// the fixed `pail --store URL obj --area A ls|get|put|rm` contract over a local directory
// next to a file:// store, and execs the real pail (PAIL_FAKE_REAL) for every other command.
// It lets tests exercise PailStore before, or without, a pail binary that has `obj`.
package fakepail

import (
	"os"
	"path/filepath"
)

const script = `#!/bin/sh
if [ "${3:-}" != obj ]; then exec "$PAIL_FAKE_REAL" "$@"; fi
url=$2
shift 5
cmd=$1; shift
root="${url#file://}.fakeactions"
mkdir -p "$root/o" "$root/v"
while ! mkdir "$root/lock" 2>/dev/null; do sleep 0.01; done
trap 'rmdir "$root/lock"' EXIT
next() { n=$(cat "$root/gen" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$root/gen"; echo "g$n"; }
case $cmd in
ls)
  prefix=$1
  cd "$root/o" && find . -type f | sed 's|^\./||' | sort | while read -r k; do
    case $k in "$prefix"*)
      printf '{"key":"%s","size":%s,"version":"%s","modified":"2026-01-01T00:00:00Z"}\n' "$k" "$(wc -c < "$k" | tr -d ' ')" "$(cat "$root/v/$k")";;
    esac
  done ;;
get)
  key=$1; shift
  [ -f "$root/o/$key" ] || exit 3
  if [ "${1:-}" = -o ]; then cp "$root/o/$key" "$2"; cat "$root/v/$key"; else cat "$root/o/$key"; fi ;;
put)
  key=$1; src=$2; shift 2
  tmp="$root/tmp.$$"
  if [ "$src" = - ]; then cat > "$tmp"; else cp "$src" "$tmp"; fi
  while [ $# -gt 0 ]; do
    case $1 in
    --if-absent) [ -e "$root/o/$key" ] && { rm -f "$tmp"; exit 4; } ;;
    --if-version) shift; { [ -f "$root/v/$key" ] && [ "$(cat "$root/v/$key")" = "$1" ]; } || { rm -f "$tmp"; exit 4; } ;;
    esac
    shift
  done
  mkdir -p "$root/o/$(dirname "$key")" "$root/v/$(dirname "$key")"
  mv "$tmp" "$root/o/$key"
  v=$(next); echo "$v" > "$root/v/$key"; echo "$v" ;;
rm)
  key=$1; shift
  [ -f "$root/o/$key" ] || exit 0
  if [ "${1:-}" = --if-version ] && [ "$(cat "$root/v/$key")" != "$2" ]; then exit 4; fi
  rm -f "$root/o/$key" "$root/v/$key" ;;
*) echo "fake pail obj: unknown $cmd" >&2; exit 2 ;;
esac
`

// Write creates the script in dir and returns its path. Non-obj commands run PAIL_FAKE_REAL
// (use "/bin/false" if none are needed); the caller must set it in the environment of the
// process that runs the script.
func Write(dir string) (string, error) {
	p := filepath.Join(dir, "pail")
	return p, os.WriteFile(p, []byte(script), 0o755)
}
