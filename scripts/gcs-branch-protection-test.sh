#!/bin/bash
# Verifies that GCS enforces pail's branch protection through a downscoped token (Credential
# Access Boundary): read everything in the repository, create-only content, full write on
# refs/pr/*, and nothing on refs/main, refs/release/*, intents/main/ or other repositories.
# Usage: scripts/gcs-branch-protection-test.sh BUCKET   (uses your gcloud login as the broad
# credential; in production the broker's service account is, and users hold no bucket IAM).
set -euo pipefail
B=${1:?bucket}; R=acl-test-$(date +%s); SRC=$(gcloud auth print-access-token)
enc() { python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=''))" "$1"; }
up() { curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $1" --data-binary x "https://storage.googleapis.com/upload/storage/v1/b/$B/o?uploadType=media&name=$R/$2$3"; }
for k in refs/main commits/c1 packs/p1.pack refs/pr/old; do up "$SRC" "$k" "" >/dev/null; done
P="projects/_/buckets/$B/objects/$R/"
boundary=$(jq -cn --arg b "$B" --arg p "$P" --arg lp "$R/" '{accessBoundary:{accessBoundaryRules:[
 {availableResource:("//storage.googleapis.com/projects/_/buckets/"+$b), availablePermissions:["inRole:roles/storage.objectViewer"],
  availabilityCondition:{expression:("resource.name.startsWith(\""+$p+"\") || api.getAttribute(\"storage.googleapis.com/objectListPrefix\", \"\").startsWith(\""+$lp+"\")")}},
 {availableResource:("//storage.googleapis.com/projects/_/buckets/"+$b), availablePermissions:["inRole:roles/storage.objectCreator"],
  availabilityCondition:{expression:("resource.name.startsWith(\""+$p+"commits/\") || resource.name.startsWith(\""+$p+"pages/\") || resource.name.startsWith(\""+$p+"packs/\")")}},
 {availableResource:("//storage.googleapis.com/projects/_/buckets/"+$b), availablePermissions:["inRole:roles/storage.objectAdmin"],
  availabilityCondition:{expression:("resource.name.startsWith(\""+$p+"refs/pr/\")")}}]}}')
T=$(curl -sS https://sts.googleapis.com/v1/token --data-urlencode grant_type=urn:ietf:params:oauth:grant-type:token-exchange \
  --data-urlencode subject_token_type=urn:ietf:params:oauth:token-type:access_token \
  --data-urlencode requested_token_type=urn:ietf:params:oauth:token-type:access_token \
  --data-urlencode "subject_token=$SRC" --data-urlencode "options=$boundary" | jq -r .access_token)
gen=$(curl -s -H "Authorization: Bearer $SRC" "https://storage.googleapis.com/storage/v1/b/$B/o/$(enc "$R/refs/main")" | jq -r .generation)
fail=0
check() { local want=$1 got=$2 what=$3; printf '%-34s want %s got %s\n' "$what" "$want" "$got"; [ "$want" = "$got" ] || fail=1; }
check 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $T" "https://storage.googleapis.com/storage/v1/b/$B/o/$(enc "$R/refs/main")?alt=media")" "read refs/main"
check 403 "$(up "$T" refs/main "&ifGenerationMatch=$gen")" "CAS refs/main"
check 403 "$(up "$T" refs/main "")" "overwrite refs/main"
check 200 "$(up "$T" refs/pr/new "&ifGenerationMatch=0")" "create refs/pr/new"
check 200 "$(up "$T" refs/pr/old "")" "overwrite refs/pr/old"
check 200 "$(up "$T" commits/c2 "&ifGenerationMatch=0")" "create commits/c2"
check 403 "$(up "$T" commits/c1 "")" "overwrite commits/c1"
check 403 "$(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H "Authorization: Bearer $T" "https://storage.googleapis.com/storage/v1/b/$B/o/$(enc "$R/packs/p1.pack")")" "delete packs/p1.pack"
check 403 "$(up "$T" refs/release/x "&ifGenerationMatch=0")" "create refs/release/x"
check 403 "$(up "$T" intents/main/i1 "&ifGenerationMatch=0")" "create intents/main/i1"
check 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $T" "https://storage.googleapis.com/storage/v1/b/$B/o?prefix=$R/")" "list repository"
check 403 "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $T" --data-binary x "https://storage.googleapis.com/upload/storage/v1/b/$B/o?uploadType=media&name=other-repo/refs/main")" "write another repository"
[ $fail = 0 ] && echo "all enforced" || { echo "MISMATCH"; exit 1; }
