#!/bin/sh
# Flip each design switch of events.qnt (the protocol without that part) in a scratch copy and
# check that the simulation finds a violation of `simSafety`. First the unmutated model, in
# both of its configurations, must pass: with notifications (lossy) and with a poll source.
# (The models live in `events/`: a `.qnt` file beside `scenarios.qnt` would change the digest
# of every generated artifact.)
set -e
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
run() { # run <invariant>: succeeds when the simulation finds a violation
  (cd "$tmp" && quint run events_sim.qnt --main=eventsSim --init=simInit --step=simStep --invariant="$1" --max-steps=60 --max-samples=200000 | grep -q '^\[violation\]')
}
mutate() { # mutate <name> <switch=value>...
  name=$1; shift
  cp events/events.qnt events/events_sim.qnt "$tmp"/
  for kv in "$@"; do
    sw=${kv%=*}; v=${kv#*=}
    sed "s/pure val $sw = [a-z]*/pure val $sw = $v/" "$tmp/events.qnt" > "$tmp/events.new"
    if cmp -s "$tmp/events.qnt" "$tmp/events.new"; then echo "no such switch: $sw = $v"; exit 1; fi
    mv "$tmp/events.new" "$tmp/events.qnt"
  done
}
expect() { # expect <caught|clean> <name> <switch=value>...
  want=$1; name=$2; shift 2
  mutate "$name" "$@"
  if run simSafety; then got=caught; else got=clean; fi
  if [ "$got" != "$want" ]; then echo "WRONG ($got, wanted $want): $name"; exit 1; fi
  by=""
  if [ "$got" = caught ]; then
    for inv in noLoss cursorBehindOut noRedundant intentRecorded noPhantom perKeyOrder cursorMonotone; do
      if run $inv; then by="$by $inv"; fi
    done
  fi
  echo "$got: $name${by:+ (by$by)}"
}
expect clean "the protocol"
expect clean "a poll source" PUSH=false LOSSY=false
expect caught "no rescan on start (events lost while down)" RESCAN_ON_START=false
expect caught "advance the cursor before emitting" CURSOR_AFTER_EMIT=false
expect caught "trust the notification's payload" RE_READ=false
expect caught "no safety poll" SAFETY_POLL=false
expect caught "no dedupe" DEDUPE=false
expect caught "no write-ahead mark (a crash between the line and the cursor)" WRITE_AHEAD=false
