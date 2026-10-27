# verify: a model of pail, and the code checked against it

A [Quint](https://quint-lang.org) model of how `Repo::commit` publishes over a bucket and
how `gc` collects it, the properties they must keep, and scenarios the real code is checked
against request by request with
[quint-refinements](https://github.com/ArchAstro/quint-refinements). Beside it: the store
contract every backend follows, and the branch policy every enforcement point decides.

Layers, each importing those below. A feature is a set of pure steps (`xCan(s, ...)`:
may it happen; `xNext(s, ...)`: the state after) over one shared state, so a new protocol
(combine, land, rewrite) is a new file of steps plus actions in `pail.qnt`:

```text
store.qnt     any object store: versioned objects, create / CAS / put / delete, lost requests
   ▲
bucket.qnt    pail's layout in it: pages/, commits/ (create-once), refs/ (CAS), locks/
   ▲
world.qnt     the shared state: bucket, writers, the collector, ghosts
   ▲
publish.qnt   Repo::commit steps (lease, read, upload, swap, recheck; crash, expiry)
gc.qnt        gc steps (exclusive lease, drain, mark, list, delete, release)
   ▲
pail.qnt      the steps as actions, any interleaving (`step`), the properties
```

```text
pail.qnt ──quint run──▶ properties hold in every simulated interleaving
     │
scenarios.qnt ──compile──▶ quint-refinements.json + tests/refinement/generated.rs
                                   │
tests/refinement.rs ── real Repo::commit and gc, gated store ──▶ same state after every step

policy_scenarios.qnt ──compile──▶ policy/policy-refinements.json
                                    ├─▶ tests/policy_refinement/generated.rs  (pail)
                                    └─▶ policy/pailpolicy_refinement_gen_test.go (gateway)
```

## Files

| File | What it is |
|---|---|
| `store.qnt` | The object store, independent of pail: `Space[k, b]` of versioned objects, writes, `Delivery` |
| `store_spec.qnt` | One action per `Store` method, each recording its reply in `state.reply` |
| `store_scenarios.qnt` | `@conformance` runs: the store contract every backend must follow, reply for reply |
| `bucket.qnt` | Pail's spaces in the store and the operators on them (`uploaded`, `refSwapped`, `leased`) |
| `world.qnt` | The state every feature acts on (`State`, `Client`, `Collector`) |
| `publish.qnt` | Writers running `commit_opts`, one store request per step |
| `gc.qnt` | The collector running `gc::gc_with`, one store request per step |
| `pail.qnt` | Actions, `step`, and the properties (`safety`) |
| `scenarios.qnt` | `@conformance` runs: interleavings the real writers and gc must follow |
| `policy.qnt` | Branch policy: parsing `policy.json`, `decide`, `request_op` |
| `policy_scenarios.qnt` | `@conformance` runs: documents to parse and requests to decide |
| `quint-refinements.json`, `store-refinements.json`, `policy/policy-refinements.json` | Generated scenario artifacts (do not edit) |
| `../tests/*/generated.rs`, `policy/pailpolicy_refinement_gen_test.go` | Generated Rust traits and Go interface (do not edit) |
| `../tests/refinement.rs` | `Repo::commit` and `gc` against `pail.qnt`: real actors over a shared in-memory bucket |
| `../tests/store_refinement.rs` | Each `Store` backend against `store_spec.qnt` |
| `../tests/policy_refinement.rs` | `Policy::parse`, `Policy::decide`, `policy::request_op` against `policy.qnt` |

## Run

```sh
cd verify && npm ci
npm test            # scenarios pass in Quint
npm run simulate    # random interleavings against `safety`
npm run compile     # after editing any .qnt file
cd .. && cargo test --release --test refinement --test store_refinement --test policy_refinement
# GCS runs in `cargo test` against the in-process emulator (src/gcs_emulator.rs); as a process:
#   cargo run --release --bin pail-gcs-emulator 127.0.0.1:4443
#   PAIL_TEST_STORES=gs://bucket/prefix PAIL_GCS_BASE_URL=http://127.0.0.1:4443 cargo test ...
# real S3/GCS (or other emulators) too:
PAIL_TEST_STORES="s3://bucket/prefix" AWS_ENDPOINT=... cargo test --release --test store_refinement
```

CI runs all four (`npm run check` in place of `compile`, failing on stale generated files).

## Properties (`pail.qnt`)

1. `branchIsComplete`: every commit the branch reaches, and its page, is stored.
2. `refOnlyMovesForward`: every move of the ref keeps what it named before.
3. `publishedIsOnBranch`: a client told "published" finds its commit on the branch.
4. `conflictedHasNothingOnBranch`: a client told "conflict" landed nothing.
5. `landsAtMostOnce`: one `commit` call lands at most one commit.
6. `gcRunsAlone`: while gc marks and deletes, no writer is between its first read and its
   return.

`npm run simulate` checks them over random interleavings of three writers and gc,
including crashes, expired leases and the grace period passing at any moment (simulation,
not proof: it samples behaviours rather than exhausting them). Two mutations confirm it is
not vacuous: gc ignoring writers' leases, or writers ignoring gc's, each break
`branchIsComplete` within seconds.

## The store contract (`store_scenarios.qnt`)

Each action is one `Store` call (`getOpt`, `putNew`, `putAny`, `casNew`, `cas`, `remove`,
`listKeys`). After each, the objects in the store (bodies, versions, tombstones) and the
call's reply must equal the model's.

| Backend | Checked by | Result |
|---|---|---|
| in-memory (`object_store` `InMemory`) | `cargo test` | 6/6 |
| `file://` (pail's local CAS) | `cargo test` | 6/6 |
| S3 API (moto) | CI `s3` job, `PAIL_TEST_STORES` | 6/6 |
| GCS API (`src/gcs_emulator.rs`) | `cargo test` (in-process), CI `gcs` job, `PAIL_TEST_STORES` + `PAIL_GCS_BASE_URL` | 6/6 |

The GCS emulator is ours because fake-gcs-server lacks the XML API `object_store` calls
(list 404s, PUT is `400 invalid uploadType`). It gives every write a fresh generation and
answers a failed `x-goog-if-generation-match` with 412; what it omits is in its doc comment.
It checks pail against the GCS semantics we encoded, not against Google's servers.

Model versions count writes. Real stores differ when an object gets back bytes it had
before: an S3 ETag and pail's local version hash the bytes, so the version comes back;
a GCS generation and an in-memory ETag do not. A first version of the scenarios asserted
"never reused" and failed on `file://` and S3. Pail does not need it (a ref's body is its
whole state), so `store.qnt` says so and no scenario rewrites old bytes.

## Branch policy (`policy.qnt`)

Every enforcement point (`pail serve`; the ArchDev gateway's Go port) runs the same
scenarios through a generated adapter, so none keeps a hand-written copy of the rules.

| Action | Real call | Compared after |
|---|---|---|
| `load(doc)` | the doc as JSON → `Policy::parse` | the parsed policy (defaults filled), or `Invalid` |
| `decide(caller, op, key)` | `Policy::decide` | `Allowed` / `Denied` |
| `requestOp(method, key, inm)` | `policy::request_op` (`""` = no `If-None-Match`) | the op |

- Quint strings cannot be sliced, so keys and globs are `Text` (lists of characters) and a
  branch is a span of its key.
- A `Doc` is JSON as far as parsing cares: each field null, an int, a string, strings, strings
  then a null, or branch rules. Unknown fields are ignored.
- The obligations inline every helper, so `policy.qnt` binds values used twice with
  `Set(v).exists(x => ...)`, and the artifacts are written `--compact`.
- Mutating `src/policy.rs` (prefix order, branch of an intent, the exclusive lease, `PUT`
  of content, version, globs, `HEAD`, default readers): every mutant fails a scenario.
- The gateway copies `policy/` (artifact and Go interface) into
  `internal/gateway/`; refresh that copy when the model changes.

## How the harness maps to the model

| Model | Real |
|---|---|
| action `lease(c)` / `gcLease` | the actor's next `PUT locks/<kind>-<id>` |
| action `checkLeases(c)` / `gcCheckLeases` | its next listing of `locks/` |
| action `readRef(c)` / `gcMark` | its next `GET refs/main` (gc then walks what it names) |
| action `upload(c)` | `c`'s next `PUT commits/<hash>`, with its pages (held until then) |
| action `swap(c, Answered)` | `c`'s next conditional `PUT refs/main`, answered |
| `swap(c, Lost)` | that PUT performed, its reply replaced by an error |
| `swap(c, Dropped)` | that PUT failed without reaching the bucket |
| action `recheck(c)` | the ref read after an unanswered swap |
| `gcListCommits` / `gcListPages` | gc's listing of `commits/` / `pages/` |
| `gcDeleteCommit(id)` / `gcDeletePage(t)` | gc's next DELETE, which must be of that object |
| `gcDone` | gc's DELETE of its lease, its last request |
| `graceElapses` | every object's LastModified, as gc lists it, moves back past the grace period |
| `CommitId` (writers from the root) | the commit hash, named by its parent's name and who uploaded it |
| `Tree` (page key) | the page's hash, named by the tree it holds |
| `locks` (by holder) | the holder's lease objects: one version per lease written or deleted |
| `Versioned.version` | writes and deletes the gates saw per key (ETag order for the ref) |
| `young` | objects created since the last `graceElapses` |
| `phase`, collector `phase` | the request the actor is held at (or that it returned) |
| `outcome` | what `commit` returned: Ok, `ConflictError`, another error |

Every actor is a real `Repo` behind a gate (`Gate` in `tests/refinement.rs`) that holds the
requests above until the scenario releases them; everything else (commit reads, intent
listings, writers' lease releases) passes straight through. After each action the harness
waits until the released actor is held at its next request or has returned, then compares
the whole state: pages, commits, ref, leases and young objects; each writer's phase,
attempt, last read and outcome; the collector's phase, marks and doomed objects. Deleted
objects map to `store.qnt` tombstones.

## What it found

1. `timeoutLandedUnderRun`: bob's swap lands but the reply is lost; alice lands on top
   before bob rechecks. Bob returned a conflict for a commit that was published. Fixed in
   `commit_opts` (it looks for its commit under the head).
2. gc could delete a page a commit in flight reused. A commit whose tree an abandoned
   commit already stored writes nothing for that page (`put_new` of an existing object), so
   the page keeps its old LastModified; gc listing between that upload and the swap deleted
   it, and the ref then named a commit whose tree was gone. `quint run` found it once gc was
   modelled; `tests/repo.rs` reproduced it on the real code. No grace period can fix it, so
   writers now hold a shared lease and gc an exclusive one (`src/lease.rs`):
   `gcWaitsForAWriterReusingAnOldPageRun` is the race, refining with gc waiting.

## Not modelled yet

1. Combining under contention (intents): a client that loses every direct attempt stops as
   `Combining` (holding its lease).
2. `land --stack`, rewrites and epochs (`rewrite_ref`), successor maps, reflog roots.
3. Packs and chunks (files are inline in the model's trees).
4. Known heads (the fast path that skips the first ref read).
5. Lease timing: a live holder is assumed to refresh within the lease's lifetime
   (`Lease::check` refuses a ref swap otherwise); only dead holders' leases expire.

## Notes

- `DIRECT_ATTEMPTS` mirrors the constant in `src/repo.rs` by hand.
