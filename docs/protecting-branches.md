# Protecting branches: who may push, and what landing requires

Pail ships the **mechanisms**; you assemble the flow. There is one policy format, and three
places that enforce it. None of them needs a stateful server.

```
                         _policy:policy.json   (versioned, protected by its own rule)
                                  │ read by
        ┌─────────────────────────┼──────────────────────────────┐
        ▼                         ▼                              ▼
 1. pail serve              2. broker                      3. lander
    https proxy                mints GCS tokens               the only identity that
    checks every request       whose access boundary          writes protected branches;
    (any store backend)        IS the policy                  checks reviews + checks first
        │                         │ (STS token exchange)          │
        ▼                         ▼                              ▼
     bucket ◀──────────── GCS enforces per object ──────────── pail land
```

1. **Proxy** (`pail serve --auth-command`): works for every backend; enforcement is in pail.
2. **Broker** (`pail policy token`): clients talk to GCS directly; **Google** enforces it.
3. **Lander** (`pail policy requirements` + `pail land`): turns "reviews and checks pass"
   into a branch move. Both 1 and 2 make it the only way onto `main`.

## 1. The policy

`policy.json` on the repository's `_policy` branch:

```json
{
  "version": 1,
  "readers": ["*"],
  "admins":  ["role:admin"],
  "gc":      ["role:gc"],
  "branches": {
    "main":      {"writers": ["lander"], "approvals": 1, "required_checks": ["test"]},
    "release/*": {"writers": ["lander"], "approvals": 2},
    "_policy":   {"writers": ["role:admin"]},
    "_meta":     {"writers": ["*"]},
    "*":         {"writers": ["*"]}
  }
}
```

1. **Identities** are strings: `user:…`, `group:…`, `role:…`, any name (`lander`), or `*`.
   A caller is one principal plus groups; a rule names it if it names either.
2. **Branch patterns**: exact names, or one trailing `*`. Most specific wins (exact, then the
   longest prefix). A branch no rule names is writable by **nobody**.
3. **What each object needs** (keys relative to the repository):

   | Object | Write needs |
   |---|---|
   | `refs/<b>`, `intents/<b>/…`, `reflog/<b>/…` | a writer of `<b>` (so combining stays among `<b>`'s writers) |
   | `commits/ pages/ packs/ chunks/` | create: a writer of *any* branch. Delete/overwrite: `gc` |
   | `HEAD` (default branch) | an admin |
   | anything, read | a reader |

   Content is create-only and content-addressed, so letting anyone who writes some branch
   upload objects is safe: an object is reachable only once a ref points at it.
4. `approvals` and `required_checks` are **not** enforced by storage. They are what the
   lander checks before it moves the branch. Storage enforces "only the lander may move it".

Commands:

```sh
pail policy set policy.json            # a commit to _policy (needs a _policy writer)
pail policy show
pail policy check --as user:alice --group group:eng write main   # exit 1 on deny, prints why
pail policy check --as lander write refs/main
pail policy requirements main          # {"branch","rule","protected","writers","approvals","required_checks"}
pail policy boundary --as user:alice   # the GCS access boundary for alice (JSON)
pail policy token --as user:alice      # a downscoped GCS token: {"token","expires_at"}
```

With no `_policy` branch, nothing is checked beyond the store's own credentials.

## 2. Enforcement point: `pail serve` (any backend)

```
client ──https + bearer──▶ pail serve ──(its own creds)──▶ s3:// gs:// file://
                             │ 1. AUTH_CMD(token) → principal, groups   (cached 60 s)
                             │ 2. policy from _policy                 (cached 30 s)
                             │ 3. policy.decide(identity, op, key)    → 403 "policy: …"
```

The auth command gets `PAIL_AUTH_TOKEN`, `PAIL_AUTH_REPO`, `PAIL_AUTH_OP` and prints the
principal on the first line, then one group per line. Exit 3 answers 403; any other nonzero
exit answers 401 (the client fetches a new credential).

```sh
cat > auth.sh <<'EOF'
#!/bin/sh   # verify the bearer however you like: JWT, OIDC introspection, a lookup
case "$PAIL_AUTH_TOKEN" in
  tok-alice)  echo user:alice; echo group:eng ;;
  tok-lander) echo lander ;;
  *) exit 1 ;;
esac
EOF
pail serve --store gs://acme-vcs --addr 0.0.0.0:8080 --auth-command ./auth.sh
```

1. The proxy is stateless: run as many replicas as you like (Cloud Run, Fargate, k8s).
2. Unconditional PUT or DELETE of content counts as a delete (needs `gc`).
3. Clients see: `error: refs/main: permission denied (403)` plus a hint naming the policy.

## 3. Enforcement point: a broker (GCS enforces it)

Clients keep talking to the bucket directly (fastest path); they just can't get a token
that does more than the policy allows.

```
dev ──"who am I" (OIDC, SSO, gh token)──▶ broker ──source token──▶ STS
                                           │  pail policy token --as <principal> --group …
                                           ◀── {token, expires_at} (≤1 h, downscoped) ──┘
dev ──────── token ───────────▶ GCS: allows/denies each object operation itself
```

1. **IAM.** Users hold **no** bucket roles. Only the broker's service account has
   `roles/storage.objectAdmin` on the bucket.
2. **Broker** = verify identity + one command. It can be a 20-line Cloud Run function:
   ```sh
   principal=$(verify "$REQUEST_TOKEN")           # yours: OIDC, IAP, GitHub OIDC, …
   pail --store gs://acme-vcs/app policy token --as "user:$principal" --group group:eng \
        --source-token-cmd 'curl -s -H Metadata-Flavor:Google \
          http://metadata/computeMetadata/v1/instance/service-accounts/default/token | jq -r .access_token'
   ```
   The output is already the credential-provider format (`{"token","expires_at"}`).
3. **Client** = a credential provider that calls the broker (see `extensions/pail-credential-*`):
   ```sh
   pail auth install ./pail-credential-acme && pail auth use acme --for gs://acme-vcs
   ```
4. **How the boundary encodes the policy** (`pail policy boundary --as …`):
   | Rule | Role | Condition |
   |---|---|---|
   | read | `objectViewer` | name under the repository; list with its prefix |
   | content | `objectCreator` | `commits/ pages/ packs/ chunks/`; create-only, since overwrite needs delete |
   | branches | `objectAdmin` | per writable pattern: `refs/`, `intents/`, `reflog/`, minus more specific patterns the caller may not write |
   | gc | `objectAdmin` | content prefixes |
   | admin | `objectAdmin` | `HEAD` |
5. **Limits** (measured on STS, 2026-10): at most 10 rules per boundary; one condition is
   rejected somewhere between 664 and 856 characters. Pail packs branch grants into conditions
   of at most 600 characters and errors if a policy needs more than 10 rules. In practice that
   is about 20 writable patterns per identity.
6. **Verified on GCS** (`scripts/gcs-branch-protection-test.sh`, 12 cases, all enforced):
   alice's token reads `main`, cannot CAS or overwrite `refs/main`, cannot write
   `intents/main/`, `_policy` or `release/*`, cannot delete a commit object, can create
   `refs/pr/*` and content. The lander's token lands `pr/alice` onto `main`.

S3 has the same mechanism (STS `AssumeRole` with a session policy); adding it means emitting
an IAM policy instead of a boundary.

## 4. The lander: reviews and checks

The lander is the only identity in `main`'s writers. It holds its own credential (a service
account token or a proxy bearer) and does this:

```
on request "land pr/x onto main"  (or: on every _meta event for an approved PR)
  1. req  = pail policy requirements main          → approvals, required_checks
  2. head = pail rev-parse pr/x
  3. reviews: _meta:prs/<n>/reviews/*.json  approve, not by the author, for `head`
  4. checks:  _meta:checks/<head>/<c>.json  state == success, for each c in required_checks
  5. pail land pr/x --stack --onto main            → atomic, or fails if main moved
```

1. **Key results by commit.** A check or review for an older commit of `pr/x` does not count;
   pushing new commits re-requires them.
2. **Re-check after a race.** If `land` fails because `main` moved and the PR needs a rebase,
   the rebased head is a new commit: steps 3–4 apply to it.
3. **Who writes `_meta`?** Reviews must not be forgeable: give `_meta` writers `*` only if
   reviews are signed or the review tool runs as a trusted identity; otherwise make the review
   tool (or the proxy) the only `_meta` writer and have it stamp the reviewer's principal.
4. `extensions/pail-pr merge` is a working prototype of steps 1–5 (it reads
   `pail.pr.json`; point it at `pail policy requirements` for policy-driven repos).
5. Run it as a Cloud Run service fed by bucket notifications on `refs/` and `_meta` (see
   [extending.md](extending.md)), or as a CLI an approver runs with the lander's credential.

## 5. Putting it together

```
1. pail policy set policy.json                 main: writers [lander], approvals 1, checks [test]
2. deploy the broker (or pail serve)           users: no bucket IAM
3. deploy the lander + pail-ci                 lander SA: objectAdmin via its own boundary
4. dev:    pail switch -c pr/x; pail commit    allowed (pr/* is "*")
           pail commit on main                 403: main is protected (rule "main")
5. ci:     records _meta:checks/<head>/test.json
6. review: records _meta:prs/<n>/reviews/bob.json
7. lander: requirements met → pail land pr/x --stack --onto main
```

Tests: `tests/cli.rs` `proxy_enforces_branch_policy` (proxy, local store);
`src/policy.rs` unit tests (decisions and boundaries agree);
`scripts/gcs-branch-protection-test.sh` (GCS itself).
