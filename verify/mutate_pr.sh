#!/bin/sh
# Flip each design switch of pr.qnt (the protocol before its fixes) in a scratch copy and
# check that the simulation finds a violation of `simSafety`.
set -e
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for sw in CAS CLAIM GUARD PER_COMMIT; do
  cp pr.qnt pr_sim.qnt "$tmp"/
  sed "s/pure val $sw = true/pure val $sw = false/" pr.qnt > "$tmp/pr.qnt"
  if (cd "$tmp" && quint run pr_sim.qnt --main=prSim --init=simInit --step=simStep --invariant=simSafety --max-steps=60 --max-samples=200000 | grep -q '^\[violation\]'); then
    echo "caught: $sw = false"
  else
    echo "not caught: $sw = false"
    exit 1
  fi
done
