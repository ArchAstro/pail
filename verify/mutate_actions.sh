#!/bin/sh
# Flip each design switch of actions.qnt (the protocol without that fix) in a scratch copy and
# check that the simulation finds a violation of `simSafety`. (The models live in `actions/`: a
# `.qnt` file beside `scenarios.qnt` would change the digest of every generated artifact.)
set -e
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for sw in CAS IF_ABSENT GUARD_READS LEASE_CREATE_ONLY FENCE FENCE_ON_JOB RESULT_PER_LEASE SWEEP \
  NEEDS_CHECK APPROVAL_EXCLUDES_ACTOR CANCEL_IN_PROGRESS RERUN_NEW_ATTEMPT CHECK_FROM_JOB; do
  cp actions/actions.qnt actions/actions_sim.qnt "$tmp"/
  sed "s/pure val $sw = true/pure val $sw = false/" actions/actions.qnt > "$tmp/actions.qnt"
  if cmp -s actions/actions.qnt "$tmp/actions.qnt"; then echo "no such switch: $sw"; exit 1; fi
  if (cd "$tmp" && quint run actions_sim.qnt --main=actionsSim --init=simInit --step=simStep --invariant=simSafety --max-steps=60 --max-samples=300000 | grep -q '^\[violation\]'); then
    echo "caught: $sw = false"
  else
    echo "not caught: $sw = false"
    exit 1
  fi
done
# At most one accepted result per job is held twice over (the fence, and the guard on the job
# file): it takes both off to break it.
sed "s/pure val CAS = true/pure val CAS = false/; s/pure val FENCE = true/pure val FENCE = false/" actions/actions.qnt > "$tmp/actions.qnt"
if (cd "$tmp" && quint run actions_sim.qnt --main=actionsSim --init=simInit --step=simStep --invariant=atMostOneAccepted --max-steps=60 --max-samples=300000 | grep -q '^\[violation\]'); then
  echo "caught: CAS = false and FENCE = false"
else
  echo "not caught: CAS = false and FENCE = false"
  exit 1
fi
