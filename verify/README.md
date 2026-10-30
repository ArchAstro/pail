# verify: a model of pail, and the code checked against it

A [Quint](https://quint-lang.org) model of how `Repo::commit` publishes over a bucket, how
`Repo::rewrite_ref` rewrites it, how `Repo::land_stack` lands a stack on it and how `gc`
collects it, the properties they must keep, and scenarios the real code is checked
against request by request with
[quint-refinements](https://github.com/ArchAstro/quint-refinements). Beside it: the store
contract every backend follows, the branch policy every enforcement point decides, and the
pull request protocol of `pail pr` (`pr.qnt`, below) and, designed but not yet built, the
workflow system pail Actions (`actions.qnt`, below).

Layers, each importing those below. A feature is a set of pure steps (`xCan(s, ...)`:
may it happen; `xNext(s, ...)`: the state after) over one shared state, so a new protocol
(combine, land, rewrite) is a new file of steps plus actions in `pail.qnt`:

```text
store.qnt     any object store: versioned objects, create / CAS / put / delete, lost requests
   ▲
bucket.qnt    pail's layout in it: pages/, commits/ (create-once), packs/, chunks/, refs/ (CAS, lineage),
              locks/, intents/, reflog/
   ▲
world.qnt     the shared state: bucket, writers, the collector, ghosts
   ▲
publish.qnt   Repo::commit steps (lease, read, upload, swap, recheck; crash, expiry)
combine.qnt   commit_combining steps (intent, poll, gather + store the batch, swap, cleanup)
rewrite.qnt   rewrite_ref steps for rebase, undo, reset (read, lease, store, read + carry + record, swap, recheck)
land.qnt      land_stack steps (make the stack: branch, commit; lease, read + fork point + copy, swap, recheck)
gc.qnt        gc steps (exclusive lease, drain, mark, list, check, delete commits, pages, packs, chunks, release)
clock.qnt     time: the tick, the refreshers that keep leases alive
   ▲
pail.qnt      the steps as actions, any interleaving (`step`), the properties
```

```text
pail.qnt ──quint run──▶ properties hold in every simulated interleaving
     │
scenarios.qnt ──compile──▶ quint-refinements.json + tests/refinement/generated.rs
                                   │
tests/refinement.rs ── real commit, rewrite_ref, land_stack and gc, gated store ──▶ same state after every step

policy_scenarios.qnt ──compile──▶ policy/policy-refinements.json
                                    ├─▶ tests/policy_refinement/generated.rs  (pail)
                                    └─▶ policy/pailpolicy_refinement_gen_test.go (gateway)

pr.qnt ─── pr_sim.qnt ──quint run──▶ safety + history properties hold in every simulated interleaving
   │
pr_scenarios.qnt ──compile──▶ pr-refinements.json + tests/pr_refinement/generated.rs
                                   │
tests/pr_refinement.rs ── the real extensions/pail-pr, its put and land held by a shim ──▶ same records after every step

actions/actions.qnt ─── actions_sim.qnt ──quint run──▶ safety + history properties hold in every simulated interleaving
   │
actions_scenarios.qnt ──quint test──▶ the runs a refinement harness will follow (no code to refine yet)
```

## Files

| File | What it is |
|---|---|
| `store.qnt` | The object store, independent of pail: `Space[k, b]` of versioned objects, writes, `Delivery` |
| `store_spec.qnt` | One action per `Store` method, each recording its reply in `state.reply` |
| `store_scenarios.qnt` | `@conformance` runs: the store contract every backend must follow, reply for reply |
| `bucket.qnt` | Pail's spaces in the store and the operators on them (`uploaded`, `refSwapped`, `leased`), and the time constants |
| `world.qnt` | The state every feature acts on (`State`, `Client`, `Collector`, each holder's `Timing`) |
| `publish.qnt` | Writers running `commit_opts`, one store request per step |
| `combine.qnt` | Writers that lost their direct attempts, running `commit_combining` |
| `rewrite.qnt` | A client rewording the tip, undoing the last rewrite or resetting the tip, running `rewrite_ref` |
| `land.qnt` | A client making a stack (a branch of its own) and landing it with `land_stack` |
| `locate.qnt` | A writer finding where the commit it read is after rewrites (`Repo::successor`) |
| `gc.qnt` | The collector running `gc::gc_with`, one store request per step |
| `clock.qnt` | Time: `tick`, and each lease holder's refresher (`refreshLand`, `refreshAck`) |
| `pail.qnt` | Actions, `step`, and the properties (`safety`) |
| `scenarios.qnt` | `@conformance` runs: interleavings the real writers and gc must follow |
| `policy.qnt` | Branch policy: parsing `policy.json`, `decide`, `request_op` |
| `policy_scenarios.qnt` | `@conformance` runs: documents to parse and requests to decide |
| `pr.qnt` | The `pail pr` protocol: records on `_meta`, clients that read, decide and write, the properties (`safety`) and the design switches (`CAS`, `CLAIM`, `GUARD`, `PER_COMMIT`) |
| `pr_scenarios.qnt` | `@conformance` runs: the interleavings the plugin must follow |
| `pr_sim.qnt` | `pr.qnt` under random interleavings, with a ghost for the history properties |
| `mutate_pr.sh` | Flips each design switch in a scratch copy and expects `pr_sim.qnt` to find a violation |
| `actions/actions.qnt` | pail Actions (not built yet): orchestrator replicas, runners, leases, results, the properties (`safety`) and the design switches |
| `actions/actions_scenarios.qnt` | `@conformance` runs: the interleavings the orchestrator and runner must follow (run by `quint test`; not yet compiled into a harness) |
| `actions/actions_sim.qnt` | `actions.qnt` under random interleavings, with a ghost for the history properties |
| `mutate_actions.sh` | Flips each design switch in a scratch copy and expects `actions_sim.qnt` to find a violation |
| `quint-refinements.json`, `store-refinements.json`, `policy/policy-refinements.json`, `pr-refinements.json` | Generated scenario artifacts (do not edit) |
| `../tests/*/generated.rs`, `policy/pailpolicy_refinement_gen_test.go` | Generated Rust traits and Go interface (do not edit) |
| `../tests/refinement.rs` | `Repo::commit`, `Repo::rewrite_ref`, `Repo::land_stack`, `Repo::successor` and `gc` against `pail.qnt`: real actors over a shared in-memory bucket |
| `../tests/store_refinement.rs` | Each `Store` backend against `store_spec.qnt` |
| `../tests/policy_refinement.rs` | `Policy::parse`, `Policy::decide`, `policy::request_op` against `policy.qnt` |
| `../tests/pr_refinement.rs` | The real `extensions/pail-pr` against `pr.qnt`: processes whose `put` and `land` a shim (`tests/pr_refinement/shim.py`) holds |

## Run

```sh
cd verify && npm ci
npm test            # scenarios pass in Quint
npm run simulate    # random interleavings against `safety` (`step`, then writers only: `contendStep`; then `pr_sim.qnt`, `actions_sim.qnt`)
npm run mutate:pr   # each design switch of pr.qnt off: the simulation must find a violation
npm run witness:actions && npm run mutate:actions   # the cases actions.qnt reaches; each of its switches off is caught
npm run compile     # after editing any .qnt file
cd .. && cargo test --release --test refinement --test store_refinement --test policy_refinement --test pr_refinement
# GCS runs in `cargo test` against the in-process emulator (src/gcs_emulator.rs); as a process:
#   cargo run --release --bin pail-gcs-emulator 127.0.0.1:4443
#   PAIL_TEST_STORES=gs://bucket/prefix PAIL_GCS_BASE_URL=http://127.0.0.1:4443 cargo test ...
# real S3/GCS (or other emulators) too:
PAIL_TEST_STORES="s3://bucket/prefix" AWS_ENDPOINT=... cargo test --release --test store_refinement
```

CI runs `npm run check` (in place of `compile`, failing on stale generated files) and `npm test` on every push.
The long runs (`simulate`, `witness*`, `mutate*`) are manual: `gh workflow run model --ref <branch>`.

## Properties (`pail.qnt`)

1. `branchIsComplete`: every commit a ref reaches (the branch, each stack), and its page,
   is stored.
2. `refOnlyMovesForward`: every move of the ref keeps what it named before, unless a
   rewrite's record names the move.
3. `publishedIsOnBranch`: a client told "published" finds its commit on the branch (unless
   an undo or reset took it off since).
4. `conflictedHasNothingOnBranch`: a client told "conflict" landed nothing.
5. `landsAtMostOnce`: one `commit` call lands at most one commit.
6. `gcRunsAlone`: while gc is about to delete (its lease check passed), no writer that can
   still move a ref (its lease fresh, or a swap on its way) is between its first read and its
   return.
7. `recentIsTruthful`, and the rewrite properties `lineageNamesItsRecord`,
   `recordsAreChained`, `reflogIsStored`, `rewriteOutcomeIsTruthful`,
   `rewritesDropOnlyTheirTarget`: see the combining and rewrite sections.
8. `publishedOnce`: each client's change enters the branch at most once by a publish (a
   move no record names); after an undo or reset takes it off, only a rewrite brings it
   back.
9. `locatedOnBranch`, `locateKeepsChanges`: see the successor walks section.
10. `knownHeadsAreTrue`: see the known heads section.
11. `packsAreStored`, `relocateKeepsContent`, `gcSparesNamedPacks`, `packsAreWritersOwn`: see the
    packs section.
12. `chunksAreStored`, `chunkedFilesReadBack`, `gcSparesNamedChunks`: see the chunks section.

`npm run simulate` checks them over random interleavings of four clients and gc,
including crashes, the clock ticking, stalled refreshers and the grace period passing at any
moment (simulation, not proof: it samples behaviours rather than exhausting them). Time
and its stalls are enabled in a fraction of the draws (`SLOW` in `pail.qnt`): a tick as likely
as any step let leases go stale before writers reached combining, rewrites or gc's deletes
(`contendStep` then ran 30x faster than before the clock, a sign of runs that stopped
mattering). Witness invariants (negations of: a combined commit, a rewrite, a gc delete, a
lease expiring under a live holder) are found within seconds of `step` / `contendStep`; `npm run witness`
(CI) checks that `step`, at CI's 100 steps, reaches gc deleting a pack (`gcNeverDeletesAPack`),
gc deleting a chunk (`gcNeverDeletesAChunk`) and a branch with a huge file
(`branchHasNoHugeFile`), each a witness invariant that must be violated. They are rare: about
one run in 10,000 to 20,000 at 100 steps, one in 150,000 at 60 (the old `--max-steps`, with
20,000 runs gc deleted nothing) and none in 200,000 at 40, so the script samples up to
200,000 and stops at the first (seconds). A commit that reused a base's pack
(`nobodyPublishesAReuse`) is rarer still (one in about 200,000): the reuse scenarios cover it. Two mutations confirm it is
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
| action `upload(c)` (also) | the `LIST intents/main/` made with the upload: pending intents send `c` to `Intending` |
| action `intend(c, at)` | `c`'s `PUT intents/main/<id>.json`; `at` orders it (the scenario's times must follow creation order) |
| action `pollRef(c)` / `pollList(c)` | the ref read and intent listing of a poll, made together, released in either order |
| action `wait(c)` | `c` polls again without combining (not the oldest intent) |
| action `gather(c, vanished)` | the GETs of the other pending intents, then the batch's commit uploads |
| action `batchSwap(c, d)` | the batch's conditional `PUT refs/main` |
| action `cleanup(c)` | the DELETEs of the batch's intents |
| action `batchRecheck(c)` | the ref read after an unanswered batch swap |
| action `withdraw(c)` | `c`'s DELETE of its own intent |
| action `beginRewrite(c)` / `beginRebase(c, plan)` | `c`'s `GET refs/main`; it then runs `rebase::build` on the plan unleased (as `pail rebase` builds), reporting what it built |
| action `beginUndo(c)` | `c`'s `GET refs/main` (then the record its lineage names, not held); `RewriteRequest::undo` |
| action `beginReset(c)` | `force_ref`'s `GET refs/main`, to the tip's parent |
| action `beginBranch(c)` | `create_branch(c, tip)` spawned: it leases first, and checks its lease when the lease listing returns |
| action `createBranch(c, d)` | the `PUT refs/c` that check let go (`d` as for a swap); a stale lease never reaches it |
| action `stack(c, delta)` | `c`'s commit on its branch, past every gate: `commit_opts` with `branch = c` (a branch only it writes) |
| action `beginLand(c)` | `land_stack(c, main)` spawned; it leases first |
| action `ldRead(c)` | its `GET refs/main` (the stack's ref and records are read past the gate), then the copy's upload |
| action `ldSwap(c, d)` / `ldRecheck(c)` | its conditional `PUT refs/main`; the ref read after one it did not win |
| action `locate(c)` | `Repo::successor` of `c`'s read commit, from its read's lineage to the branch's, read past every gate |
| action `beginKnown(c, from)` | `commit_opts` by `c` with `from`'s `KnownHead` (as its last `Written` left it: kept when its `head` is its `commit`); its first ref read is skipped |
| action `rwStore(c)` | under `writing()`'s lease, the wave of `PUT commits/<hash>` for the plan's commits (`rebase::store`) |
| action `rwRead(c)` | `rewrite_ref`'s `GET refs/main`, then each carried commit's upload and the `PUT reflog/main/<id>` |
| action `rwSwap(c, d)` / `rwRecheck(c)` | its conditional `PUT refs/main`; the ref read after an unanswered one |
| `MARK` (`~carol`) | a commit with `rewritten.from`: its parent's name, its original's writer, the rewriter |
| `reflog` (ids 1, 2, ..) | records in the order they were written; `lineage.rewrite` and `prev` by that number |
| `INTENT_NAME` (`alice+`) | a round's commit for an intent: its parent's name and the intent's writer (`pail.intent`) |
| `Comb` | the poll's ref read and listing, and the commits the round stored |
| action `recheck(c)` | the ref read after an unanswered swap |
| `gcListCommits` / `gcListPages` | gc's listing of `commits/` / `pages/` |
| action `gcListChunks` / `gcDeleteChunk(k)` | gc's listing of `chunks/`; its next DELETE, which must be of that chunk |
| `chunks` | `chunks/<hash>`, named by the model's `k1`.. (the harness knows each chunk's bytes), holding the name of the bytes it stores (none if they are no chunk's) |
| `Client.reuse` | the paths whose content the base has elsewhere (`prepare_files`' `reuse`), each to the pack the base's page names (`root` for a huge file) |
| action `gcDeleteCommit(id)` / `gcDeletePage(t)` / `gcDeletePack(k)` | gc's next DELETE, which must be of that object |
| action `gcListPacks` | gc's listing of `packs/` |
| `Commit.locs`, `Page.locs` | for each big file of the tree, the pack its page's `Loc::Pack` names |
| `packs` | `packs/<hash>.pack`, named by `packOf` (below), holding the contents of its blobs (the harness reads the pack's footer) |
| `gcDone` | gc's DELETE of its lease, its last request |
| `graceElapses` | every object's LastModified, as gc lists it, moves back past the grace period |
| action `tick` | the test clock (`Store::with_clock`) moves 100 s on; every refresher whose sleep ended makes its `PUT locks/<holder>` and waits at the gate |
| action `refreshLand(h, d)` | that refresher PUT released (`d` as for a swap); answered, its reply then waits at the gate |
| action `refreshAck(h)` | that reply let through, and the refresher sleeps again |
| `now` | the test clock, in ticks |
| `locks` (by holder) `.at` | the tick the holder's lease was last written: the LastModified the gate gives it in listings |
| `timing` (by holder) | `refreshed`: the lease's own record of its last refresh (`Lease::probe`, `Repo::lease_probe`); the rest as the refresher's requests were let through: `inflight`/`sent`/`landed`/`done`, and `checked`, the tick a swap or delete request arrived at the gate (a passed `Lease::check` precedes it) |
| `CommitId` (writers from the root) | the commit hash, named by its parent's name and who uploaded it |
| `Tree` (page key) | the page's hash, named by the tree it holds |
| `locks` (by holder) | the holder's lease objects: one version per lease written or deleted |
| `Versioned.version` | writes and deletes the gates saw per key (ETag order for the ref) |
| `young` | objects created since the last `graceElapses` |
| `phase`, collector `phase` | the request the actor is held at (or that it returned) |
| `outcome` | what `commit` returned: Ok, `ConflictError`, another error; what `rewrite_ref` returned: Ok, `MovedError`, `ConflictError`, another error |

Every actor is a real `Repo` behind a gate (`Gate` in `tests/refinement.rs`) that holds the
requests above until the scenario releases them; everything else (commit and page reads,
writers' lease releases) passes straight through. A writer makes some requests together
(an upload and a listing; a poll's ref read and listing; a round's uploads or deletes), so
it may wait at several at once. After each action the harness waits until every released
request is answered and the actor is held again or has returned, then compares
the whole state: pages, commits, ref (with `recent` and lineage), leases, intents, reflog
records and young objects; each writer's phase, attempt, last read, combining round,
rewrite and outcome; the collector's
phase, marks and doomed objects. Deleted
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
3. `rewordAfterLostSwapRun`: alice's swap lands unanswered; carol rewords alice's commit
   before alice rechecks. Alice's commit is no longer under the head (its reworded copy is),
   so she retried, found her own change in the way and returned a conflict for a published
   commit: `conflictedHasNothingOnBranch`, found by `quint run` within a minute. Fixed in
   `commit_opts` (`landed_since`: also under the tip the first rewrite since replaced).
4. `rewriteLostThenRewrittenRun`: carol's rewrite swap lands unanswered; dave rewrites again
   before carol rechecks. The ref's lineage is dave's, so carol retried, found her tip gone
   and returned `MovedError` for a rewrite that landed: `rewriteOutcomeIsTruthful`. Fixed in
   `rewrite_ref` (`in_chain`: its record is in the ref's chain).
5. `undoTwiceRun`: alice resets carol's commit off the branch; dave and bob both undo the
   reset, dave first. Bob's undo found the reset's new tip (the root) still under the head,
   took carol's restored commit for one landed since and carried it onto itself: carol's
   change twice, `landsAtMostOnce`. Found by CI's `npm run simulate`. Fixed in
   `rewrite_ref`: an undo carries the lineage it read (`RewriteRequest::since`), and any
   other rewrite since fails it with `MovedError`.
6. `force_ref` named no one, in its record or as committer of the commits it carried. It
   now takes the committer.
7. `landAfterResetRun`: alice's land swap lands unanswered; bob resets the branch, dropping
   her copy, before she rechecks. Her recheck compared the branch's tip with her copy
   alone, so she retried and landed her change again, undoing the reset:
   `publishedOnce`, found by `quint run` once landing was modelled. (A fast-forward land
   did not: its fork point then fell on the reset's old tip.) Fixed in `land_stack`: its
   recheck asks `landed_since`, as a writer's does.

Scenarios 3, 4 and 7 diverge from the unfixed code at the recheck, 5 at bob's read.

Lease timing (`clock.qnt`, below) found three more, each in `src/lease.rs` or `src/gc.rs`:

8. `staleGcRefusesToDeleteRun`: gc marked, then stalled: its refresh never landed, so by
   `TTL` the writers no longer saw its lease and alice made a stack. gc resumed, listed her
   commit as unreachable (it marked before) and deleted it under her ref: `branchIsComplete`.
   gc never called `Lease::check`. It now does before each delete, and a gc that finds
   itself stale gives up (`gc_under`).
9. `gcLeaseCountsFromItsWriteRun`: a lease counted its age from when `Lease::held` ran,
   after the put and the listing that followed, but its LastModified is the put's. gc, slow
   to list, passed its check at tick 6 on a lease the others had stopped seeing at tick 6;
   a refresher's reply likewise stamped the time it came back. Leases now count from the
   instant before the put (`Lease::write`, the refresher).
10. `lateRefreshDoesNotReviveRun`: with 9 fixed, a refresh sent at tick 6 for a lease
    written at tick 0 still made gc fresh again when it landed, after alice had made her
    stack and the others had stopped seeing the lease. A refresh answered after its holder
    went stale is now ignored (`Stamp::advance`): the others may have stopped seeing the lease
    before it landed, and a reply cannot take that back.

Each was a counterexample of `quint run` (branchIsComplete / safety within seconds), with
the fix undone, before it was a fix.

## Combining (`combine.qnt`)

A writer that loses both direct attempts (or whose upload finds intents pending) writes an
intent and polls. Whoever combines lands every pending intent in one swap, oldest first,
skipping any that conflicts with what lands before it, and records each in the ref's
`recent`. New property `recentIsTruthful`: each landed intent names a commit on the branch
made of it. `publishedIsOnBranch`, `conflictedHasNothingOnBranch` and `landsAtMostOnce`
now cover combined commits.

- `quint run --step=contendStep` (writers only: no crashes, gc, give-ups) reaches combining;
  `step` rarely gets a writer through two lost races. 100,000 contended runs: no violation.
- Scenarios: `combineAloneRun`, `joinedBatchRun` (a writer joins, waits, finds its intent
  landed), `conflictSkippedRun`, `batchSwapLostRun`.
- Simplifications: any writer may combine at any time (the stall takeover is by time);
  `INTENT_MAX_AGE` and `RECENT_MAX` are never reached; reading the intents and storing the
  batch are one step.

## Rewrites (`rewrite.qnt`)

Three rewrites, each its record's `op`, all published by `rewrite_ref`:

1. `rebase`: a client carries out a plan on the tip's commits (`beginRewrite`: reword the
   tip; `beginRebase`: drop its parent, drop the tip, swap the two, or meld one into the
   other: `squash` (the tip into its parent) or `squashUnder` (the parent into the tip,
   a fixup moved down). It reads the ref and builds the plan in memory as `rebase::build`
   does (each picked commit kept if it still sits on the history so far, else its change
   checked to apply and written over it, dropped if that changes nothing; a squash or
   fixup group is one commit: each member's change, lead first, checked to apply over the
   tree with the group's changes before it, every member's successor, the group dropped
   whole if it changes nothing; a commit that does not apply is a conflict before any
   lease), then leases and stores the commits it made in one wave.
   A melded commit's id keeps its members' tags (`[.., t, "&", lead, mark]`), so
   `authorsOf` names every writer whose change it makes; the properties ask
   `authorsOf(x).contains(c)` where one author used to do.
2. `undo` (`pail rebase --undo`): `beginUndo` reads the ref and the record its lineage
   names; `RewriteRequest::undo` goes from that record's new tip back to its old one, each
   successor mapped back, and fails if the branch was rewritten since (its lineage moved).
3. `reset` (`Repo::force_ref`): `beginReset` moves the branch to the tip's parent, dropping
   the tip; no map.

`rewrite_ref` then reads the ref; moved off the tip it was made from: `MovedError`;
an undo whose lineage moved: `MovedError` too; otherwise carries each commit landed since onto the new tip (its change from its parent,
checked to apply there: `check_applies`, else `ConflictError`), writes the record
(`reflog/<branch>/<id>`: op, old, new, epoch, prev, map) and swaps with the epoch bumped.
Moving the ref forward keeps its lineage. gc marks every record's old and new tips (and
what is under them).

- Properties: `lineageNamesItsRecord` (the ref's lineage names a record of its epoch whose
  new tip the branch has), `recordsAreChained` (each record's epoch is its prev's plus one),
  `reflogIsStored` (every commit a record reaches stays stored, for successor walks and
  undo), `rewriteOutcomeIsTruthful` (Ok: its record is in the ref's chain; `MovedError` or
  `ConflictError`: none is), `rewritesDropOnlyTheirTarget` (a rebase takes off the branch only
  the changes of the commits its record lists as dropped, `None` in pail's map; an undo
  or reset at most one: every change landed meanwhile is carried).
  `refOnlyMovesForward` allows moves a record names; `recentIsTruthful` asks only that the
  branch has each landed intent's change (a rewrite keeps `recent` as it was), and with
  `publishedIsOnBranch` excuses a change an undo or reset took off.
- Scenarios: `rewordCarriesRun`, `rewordAfterLostSwapRun`, `rewriteMovedRun`,
  `rewriteLostThenRewrittenRun`, `rewriteSwapDroppedRun`, `gcKeepsRewrittenHistoryRun`,
  `rewordKeepsRecentRun` (a rewrite keeps `recent`); `undoCarriesRun` (the map reversed,
  a commit landed since carried), `resetAfterLostSwapRun` (a lost-reply publish the reset
  dropped still reports published; the undo brings it back), `resetCarriesRun` (a carried
  commit's change written over the new tip's tree), `resetConflictRun` (a commit landed
  since does not apply: nothing published), `undoTwiceRun` (an undo of a reset already
  undone moves); `rebaseDropParentRun` (a plan dropping a commit, then carrying one
  landed since), `rebaseDropTipRun` (a kept commit reused, nothing stored),
  `rebaseReorderRun`, `rebaseConflictRun` (a plan's commit does not apply: nothing
  leased), `rebaseEmptyRun` (commits whose changes are already there dropped as empty);
  `rebaseSquashRun` (one commit with both changes, both its successor),
  `rebaseSquashUnderRun` (a fixup moved down), `rebaseSquashConflictRun` (led by the
  commit that depends on the other: a conflict), `rebaseSquashOverwritesRun` (members in
  order: the later change wins), `rebaseSquashEmptyRun` (a change and its revert: both
  dropped, the branch back at the root).
- Six mutants of `rebase::build`'s groups each fail a scenario: members checked without
  the group's changes before them, a member's change put under the lead's, an empty group
  kept, only the lead mapped to the group's commit, a melded commit kept as it was, the
  group built from its lead alone.
- `contendStep` interleaves rewriters with writers. 100,000 contended runs and 50,000
  `step` runs: no violation.
- Twenty mutants of the rewrite code each fail a scenario: no carrying, epoch not bumped,
  no `prev`, gc ignoring the reflog, a publish resetting the lineage, the moved check
  skipped, either recheck fix undone, the record's map missing carried commits, `recent`
  dropped; the undo's map not reversed, the undo expecting the old tip, a reset naming no
  one, carried commits unchecked, a carried commit keeping its own tree, an undo ignoring a
  rewrite since; and four of `rebase::build`: never reusing a kept commit, the map
  missing drops, no apply check, empty commits kept. The map mutant
  passed until quint-refinements#25 (the runtime filled a map entry the snapshot lacked in
  from the model).
- Simplifications: plans are over the tip's two commits (a group melds two; messages and
  `--autosquash` are not modelled); a reset only drops the tip; each client rewrites
  at most once, so a chain has at most four records; `rewrites_between`'s
  `REWRITE_CHAIN_MAX` is never reached.

## Landing a stack (`land.qnt`)

A client makes its stack whole (`stack`: one commit on the branch's tip and a branch named
after the client), then runs `land_stack` onto the branch: lease; read the stack and the
branch; find the fork point (the merge base, or the merge base with a rewrite's old tip if
later); land nothing if the branch has the commit or a copy of it above that; fast-forward
if the branch is still at the fork point; otherwise copy the commit onto the branch's tip
unless a path it writes changed since (`ConflictError`); swap, keeping `recent` and
lineage; after a swap it did not win, recheck (`landed_since`) or try again.

- Properties: `publishedIsOnBranch`, `conflictedHasNothingOnBranch`, `landsAtMostOnce` hold
  of landers as of writers; `publishedOnce` is new; gc marks every ref.
- Scenarios: `landAfterResetRun` (finding 7), `landFastForwardRun`, `landAfterRewordRun`
  (the fork point under a reword), `landAfterResetDropRun` (a reset's old tip as the fork
  point: the dropped commit below the stack stays dropped), `landConflictRun`,
  `landTwiceRun` (landing again finds the copy).
- Five mutants each fail a scenario: the recheck comparing tips, the fork point ignoring
  rewrites, no conflict check, no fast-forward, copies ignored.
- Branch creation: `staleBranchCreateRefusesRun` (the lease put at tick 0 is listed at tick 4:
  `Lease::check` refuses, nothing is written, the lease goes), `branchCreateAtTheEdgeRun`,
  `createBranchLostReplyRun`.
- A stack is made as `pail switch -c` and `pail commit` make it: `create_branch` at the branch's
  tip (its own lease, checked before the ref write: `beginBranch`, `lease`, `checkLeases`,
  `createBranch`), then one commit on it (`stack`).
- Simplifications: a stack is one commit, whose commit step is taken whole (its own lease and
  requests are not steps; no one else writes the branch), and is not moved after; a client
  lands at most its own stack; `land --stack`'s
  CLI (pre-publish hook, reporting) is not modelled.

## Successor walks (`locate.qnt`)

A writer that returned asks where the commit it read is now (`Repo::successor`, as `pail
sync` and `pail commit` do after a rewrite): from the lineage it read at to the branch's,
each record between, oldest first (`rewrites_between`: back from the branch's record to
its own, or one no later than its epoch), maps the commit: each commit of its history the
record rewrote goes to its successor, each the new history kept to itself, dropped ones
nowhere; the latest of those places wins. (Taking the first one up the history lost
changes: a reorder or a fixup can move an older commit above a newer one. The simulation
found it; `locateKeepsChanges` failed.)

```text
  seen ──r1──▶ x1 ──r2──▶ x2 ── … ──▶ found      (oldest record first)
  per record: latest of { successor(h) | h kept: h } over h in x, parent(x), … (dropped skipped)
```

- Properties, while the branch is at the lineage it looked at:
  1. `locatedOnBranch`: what it found is on the branch.
  2. `locateKeepsChanges`: what it found has every change the commit it read had, but
     those a record in the chain dropped.
- Scenarios: `locateAfterDropRun` (a dropped commit → its kept ancestor; a picked one →
  its copy), `locateThroughUndoRun` (a reword, then its undo: two records, oldest first,
  and asking again after each), `locateThroughRewordsRun` (two rewords: the older record
  matters), `locateThroughReorderRun` (a swap: the top commit's own successor lacks the
  moved one's change; the place above it has both).
- Five of six mutants (plus the first-place-up-the-history walk, which only
  `locateThroughReorderRun` kills) of `successor`/`map_through` fail a scenario: a dropped commit
  mapped to itself, kept commits not kept, records newest first, the newest record only,
  no walking up the history. The sixth (`rewrites_between` not stopping at `from`'s own
  record) is equivalent: that record's epoch stops the walk anyway.
- Simplifications: only a writer's read commit is located; `None` (nothing survived)
  never happens, since the root survives every rewrite.

## Known heads (`publish.qnt`, `beginKnown`)

`pail commit` keeps the head its last publish left it (`KnownHead`: the ref's version,
`recent` and lineage, kept only when its commit is the ref's tip then) and passes it to the
next commit, whose first attempt swaps from it without reading the ref. A stale one only
loses the swap: the second attempt reads the ref and replays as usual.

```text
  publish ──▶ known = (tip == mine) ? {version, recent, lineage} : none
  beginKnown(c, from) ──▶ Leasing ─lease/check─▶ Uploading (no readRef) ─swap─▶ …
                                       stale version → swap loses → Reading (attempt 2)
```

- Property `knownHeadsAreTrue`: while the ref's version is a known head's, the ref names
  its commit, `recent` and lineage; so a swap from it moves the branch from that commit.
- `beginKnown(c, from)` commits on `from`'s known head (one client per commit in the model,
  so a known head is handed to the next writer).
- Scenarios: `knownHeadRun` (first attempt reads nothing), `knownHeadStaleRun` (stale:
  the swap loses, attempt 2 reads and replays), `knownHeadAfterCombineRun` (the batch's
  tip keeps `recent`), `knownHeadAfterRewriteRun` (the lineage is carried).
- Mutants of `commit_opts` and its outcomes: known head on every attempt, `recent` not
  carried, lineage not carried, and the version paired with the writer's own commit after a
  lost answer, a combined batch or a joined batch each fail a scenario. A joined writer
  is mid-batch only with three intents, which four clients cannot reach (two must land
  first to send the combiner to intents): `contended_writers_all_land_by_combining`
  (`tests/repo.rs`) kills that one, its writers committing on their known heads. Using the
  known head whatever the base is equivalent: its only caller (`Worktree::commit`) builds
  it from the base.
- Simplifications: a client commits once, so the head is handed to another client
  (`from`); `Worktree::commit`'s own keep-or-drop is mirrored in the harness, not run.

## Packs and repacking (`bucket.qnt`, `gc.qnt`)

A file bigger than a manifest entry holds (`INLINE_MAX`) is stored in a pack, and the leaf
page names the pack and the range (`Loc::Pack`). A page hashes its locations, so moving a
file to another pack makes another page, another root, another commit. A repack
(`pail maintenance repack`) is therefore an ordinary commit: its delta writes each moved
path with the content it has, and the new page names a new pack.

```text
  alice  a = p1 ──▶ commit [root alice]        locs {a: alice}   packs/alice  {p1}
  carol  repack ──▶ commit [root alice carol]  locs {a: carol}   packs/carol  {p1}   same tree, no change
                    (the delta is `a = p1`, the content it has; pages differ, trees do not)
```

- Model: contents in `BIG` (`p1`..`p3`) live in packs, the others inline. A writer prepares
  one pack for the big files its delta writes (`packOf`: named by the client), stores it
  with its first commit, and a retry on a newer head reuses it (`commit_opts` takes the
  uploads once). A commit's `locs` say where its page has each big file: its own pack for
  the paths it writes, its parent's loc for the rest (a writer based before a repack
  inherits the repack's locs from the head it replays on). The page is keyed by tree and
  locs.
- gc marks every pack a marked page names, then, after pages, lists `packs/` and deletes
  the unmarked, old ones (`GcSweepingPacks`).
- Properties:
  1. `packsAreStored`: every pack the pages of the commits a ref (or a record) reaches
     name is stored.
  2. `relocateKeepsContent`: each big file reads back its content through its page's pack,
     so a repack that moved a file changed no content.
  3. `gcSparesNamedPacks`: while gc sweeps packs, none it has doomed is named by a commit
     a ref reaches.
  `refOnlyMovesForward` and the rest hold of a repack as of any commit, which is the claim
  that it is ordinary.
- Scenarios: `repackKeepsContentRun` (same tree, empty change list, both packs kept by gc),
  `repackRacesStaleWriterRun` (a writer based before the repack lands over it and keeps
  the repack's locs), `repackRacesChangedPathRun` (a writer changed a moved path: the
  repack conflicts; one recomputed from the new tip lands; gc deletes the abandoned
  repack's commit, page and pack and nothing else), `gcWaitsForARepackReusingAnOldPackRun`
  (the repack's pack is old when the retry reuses it; gc waits for the writer's lease),
  `gcSparesAYoungPackRun`.
- Mutants, each failing a scenario: gc marking no packs (model and `src/gc.rs`), gc
  ignoring the grace period for packs (both), a pack stored without its contents, a retry
  placing every file in its own pack, the real `commit_opts` not storing the writer's
  uploads, and not checking that the paths a replayed delta writes are disjoint
  (`check_disjoint`).
- Copies and combined commits (refined): a rewrite's copy (a reword, a plan's pick or
  squash group, a commit carried past a rewrite) and a landed stack's copy are written
  from the original's change list, whose entries carry their locations. So the copy's page
  puts each path the change writes where the original's page has it, the rest where the new
  parent's does, and the copy stores no pack and no chunk (`storedCopy`, `copyLocs`). A group
  takes each path from the member that wrote it last (`groupLocs`). A path only relocated
  (same content, another pack) is no change, so a copy drops the relocation. A combined
  intent's commit points at the pack its writer uploaded with its first attempt
  (`packOf` maps `alice+` to `alice`), as the real combiner does. `packsAreWritersOwn`: every
  pack in the bucket is a client's own.
- Scenarios: `rebaseReorderKeepsPacksRun` (and gc keeps both packs), `rebaseSquashKeepsPacksRun`,
  `rebaseSquashLaterPackWinsRun`, `carryKeepsPacksRun`, `landCopyKeepsPacksRun`.
- Mutants of the model, each killed in Quint by an assertion (`carryKeepsPacksRun`,
  `landCopyKeepsPacksRun`): the previous model, a copy given a pack of its own (a land copy
  by `uploaded`, a carried commit in a pack named by itself). It was wrong against the real
  code, which the refinement scenarios now check.
- Simplifications: a pack's name is its writer's, not its content hash (the harness reads it off a
  decoy blob it adds, so two writers' packs of one content differ). Content already stored
  (`reuse`) is modelled for direct and combined commits (below). A repack's retry (recompute from
  the new tip) is a new `commit` call, played by another client in the model. A stack's
  files are not reused from its base. A combining round re-creates the writer's pack in the
  model (a no-op where it is stored, as in the real code; the real one never needs it, since
  intents older than `INTENT_MAX_AGE` are ignored and gc's grace is far longer).

## Chunks (`bucket.qnt`, `gc.qnt`)

A file of `CHUNK_THRESHOLD` (1 MiB) or more is not packed. `prepare_files` splits it with
`chunk_spans` (content-defined, FastCDC), stores each piece as `chunks/<hash of the piece>`
and the leaf page names them (`Loc::Chunks`). Equal pieces are one object, whichever file or
commit has them, so an edited large file mostly stores nothing: its upload asks first (`HEAD`,
`check_first`) and a `put_new` of an existing chunk writes nothing, leaving its LastModified
old. gc marks the chunks every marked page names and, after packs, lists `chunks/` and
deletes the unmarked, old ones, in the same loop as every other prefix (`GcSweepingChunks`).

```text
  h1 = [k1 k2]   alice  a = h1 ──▶ chunks/k1, chunks/k2         (young)
  h2 = [k1 k3]   bob    b = h2 ──▶ chunks/k3                    k1 exists: nothing written, k1 stays old
  page of bob's commit names k1 k2 k3 (a: h1, b: h2)             gc: k1 is marked by bob's page, whatever its age
```

- Model: contents in `HUGE` (`h1`, `h2`) are chunked, `CHUNKS` says into what (`k1`..`k3`, `k1`
  shared). A chunk is `chunks: Space[str, str]`, its name to the bytes it holds (a chunk
  stored without its bytes holds none). A commit that writes a huge path stores the chunks of
  its content that are not stored yet (`storedWith`, every protocol: a combined intent or a
  rewrite's copy stores them too, which the real code does not need, as the writer already
  has; `create` is a no-op there). The chunks a page names are fixed by its tree's contents,
  so a page's identity (tree and pack locs) does not change.
  gc's `Young`, `spared` and `liveChunks` cover chunks like packs; the phase is
  `GcSweepingChunks`, after packs, and listing and every delete are guarded by `Lease::check`
  (`gcGuarded`), `deleting()` covers it, so `gcRunsAlone` and the `LATENCY` bound of `tickCan`
  hold for chunk deletes as for the others.
- Properties:
  1. `chunksAreStored`: every chunk the pages of the commits a ref (or a record) reaches name
     is stored.
  2. `chunkedFilesReadBack`: each of them is stored with its bytes, so a chunked file reads
     back its content.
  3. `gcSparesNamedChunks`: while gc sweeps chunks, none it has doomed is named by a commit a
     ref reaches.
- Scenarios (the chunks of a gc's list are deleted in key order, which a scenario's
  `gcDeleteChunk` order must follow): `chunksAreSharedRun` (a chunk shared by two files: the second writer stores
  one chunk, and the shared one stays old and not young), `gcSparesAYoungChunkRun` (an
  abandoned commit's chunks are spared young, deleted once old), `gcKeepsASharedChunkRun`
  (the abandoned commit's own chunk goes, the one a live commit shares stays),
  `gcWaitsForAWriterReusingAnOldChunkRun` (the lease race: a writer reuses an old,
  unreferenced chunk, so writes nothing and its age says it is garbage; gc, started between
  the upload and the swap, waits for the writer's lease and then marks it),
  `staleGcRefusesToDeleteChunkRun` (a stale gc gives up before a chunk delete).
- Refined against the real code in direct commits and gc. The harness gives each model chunk
  its bytes (`chunk_bytes`: constant bytes, which the chunker only cuts at its maximum size
  or the file's end, so `k1` is a 4 MiB chunk and `k2`, `k3` 1 MiB tails; `check_chunking`
  fails if the chunker ever cuts them otherwise) and maps a chunk object to its name by the
  hash of the bytes it decodes to.
- Mutants of `src/`, each failing a scenario (undone after the run): gc marking no chunks,
  gc ignoring the grace period for chunks, a chunk stored without its bytes
  (`prepare_files`; `chunksAreSharedRun`: the chunk's body), `store_commit` skipping the chunk
  uploads (`chunksAreSharedRun`: the chunk is missing), no `Lease::check` before a chunk
  delete (`staleGcRefusesToDeleteChunkRun`), and gc not waiting for the writers' leases
  (`gcWaitsForAWriterReusingAnOldChunkRun`: the page and pack scenarios fail too). A
  `prepare_files` that ignores `reuse` fails the reuse scenarios (`reuseNamesTheBasePackRun`).
- Simplifications: a huge file is two chunks (`k1` shared; the model has no chunk common to
  every version); whether the real chunker cuts two real files the same way is not modelled
  (the harness's bytes are chosen so it does); `INLINE_MAX` and `PACK_MAX` splitting is not
  modelled.

## Reuse (`bucket.qnt`, `publish.qnt`)

`Worktree::commit` passes `prepare_files` the locations of its base's entries by content
hash (`reuse`): a staged file whose content the base has at another path (a move, a copy, a
revert) is not stored again, and its entry gets that path's location, a pack range or a
chunk list. Nothing else reuses: `write_files` (the repack, `pail-mirror`) passes an empty
map, and a rewrite's copy of a commit keeps the original's locations as a matter of course
(see packs). A combining writer's intent carries its delta's entries, locations included,
so a reused file keeps its reused location through combining.

- Model: a client that begins on a base the branch (or a record) reaches fixes `Client.reuse`
  before any lease (`reuseOf`): each big or huge path its delta changes to content another
  path of the base holds, to the pack the base's page names for that path (a huge file's
  chunks are named by content, so the value is only a marker). Its upload then stores no
  pack for that path, no chunks for it, and its page names the base's pack. A relocation (a
  path written to the content it has) never reuses. `Intent.reuse` is the client's: a round's
  commit for the intent names the reused pack for those paths and its writer's pack for the
  rest (`Entry.reuse`). Rewrites and landing copy the original's entries (packs section).
- Why only a reachable base: a working copy's base is what it read from the branch, which
  gc marks (the ref's history, and every record's tips), so its packs and chunks stay. A
  stored but unreachable base (a writer's abandoned commit) could in principle be young while
  the older pack or chunk its page names is gone, and reusing from it would name a missing
  object. No working copy has such a base, and the restriction is not needed for the
  properties as far as simulation shows (20,000 runs of 100 steps without it: no violation).
  The harness applies it by marking, as gc does.
- Checked by `packsAreStored`, `relocateKeepsContent`, `chunksAreStored` and the scenarios:
  `reuseNamesTheBasePackRun` (a copy of a big file names the base's pack and stores none),
  `reuseSurvivesAReplayRun` (the writer's replay on a newer head keeps the base's pack for
  the path it reused and the head's for the rest).
  `reuseThroughCombiningRun` (alice's copy of a big file loses twice and combines: her round's
  commit names carol's pack, and she stored none).
- Refined against the real `commit_opts` with the real `Repo::prepare_files(inputs, reuse)`,
  the harness building `reuse` as the worktree does, in direct and combined commits.
- Mutant of the model, killed in Quint by `reuseThroughCombiningRun`: the intent not carrying the
  client's reuse (the round's commit in the writer's pack, which it never stored).
- Simplifications: reuse is of a path other than the one written; only commits a working copy
  makes reuse (a stack's commit, rewrites and landing do not: the latter copy entries).

## Amortized compaction: soft relocations (`scenarios.qnt`, `publish.qnt`, `bucket.qnt`)

A commit may carry, besides its delta, a bounded list of old files of its base to move into
the pack it uploads anyway (`CommitOptions::relocs`; the client re-encodes clean local copies,
so no request is added). A relocation is *soft*: it is the same content at another place,
so it never decides whether the commit lands.

```text
  user commit  = delta (hard: a changed path conflicts)  +  relocs (soft)
  on a head past the base, a reloc is kept iff the head still has the path as the base did:
      same content AND same pack  (so a path the head changed, or relocated elsewhere, is dropped)
      and the delta does not write it
  kept relocs: the page points at the writer's pack.  dropped: nothing, silently.
  the writer's pack holds everything it planned, kept or not (storage is not reclaimed)
```

- Model: `Client.reloc` and `Intent.reloc` (path to the content it has), `beginSoft`,
  `keptSoft` (the rule above, used by a direct commit's upload and by each commit of a
  combining round, against the batch so far), `softBudget`. A combined commit's `Entry.wrote`
  is the big files its page puts in the writer's pack.
- Properties: `relocationsKeepContent` (a commit a writer or its combined intent makes changes
  only paths its own delta writes: a relocation never changes content, kept or dropped),
  `relocationsNeverConflict` (a client told it conflicted has a path of its own delta that
  the branch changed), `relocationsAreBudgeted` (at most as many big files as the delta's own;
  the real planner also has a floor, which two paths could not show).
- Scenarios (`scenarios.qnt`, refined against `Repo::commit_opts`): `softKeptRun`,
  `softDroppedWhenHeadChangedRun` (the user's commit lands with its own change, the
  relocation of a path another commit changed is dropped), `softDroppedWhenHeadRelocatedRun`
  (a repack of the path landed first), `softDroppedInCombineRun` (the same through an intent).
- Mutants of the model, each killed: relocations hard (`softDroppedWhenHeadChangedRun`,
  `softDroppedInCombineRun`), dropping the user's change when a relocation is dropped (three
  scenarios), keeping a relocation whatever the head did (three scenarios, and `simulate`
  breaks `relocationsKeepContent`), ignoring the budget (`simulate` breaks
  `relocationsAreBudgeted`). Checking only the loc (not the content) is equivalent: a
  changed path is always another pack.
- Refined against the real code: `Harness::begin_soft` runs `Repo::prepare_files_moving` and
  `commit_opts` with `CommitOptions::relocs`, and reports the writer's relocations
  (`reloc`) and, for an intent, the ones it stored. Real code: `src/amortize.rs` (which
  files, how many), `Repo::kept_relocs` (the soft rule), `Worktree::commit` (the call).

## Pull requests (`pr.qnt`)

`pail pr` has no server: a pull request is a branch plus records on the `_meta` branch, and
every client (the plugin, pail-web) reads, decides, then writes, while others do the same.
`pr.qnt` models that: one PR, one head branch (commits 1..tip; the base is commit 0) and its
base, four clients (`alice2` is `alice` again), and the writes:

| Write | Real call |
|---|---|
| `WCreate` | `pail put _meta --file prs/<n>.json=.. --if-absent` (id `n`, then `n+1` on exit 4) |
| `WSave` | `pail put _meta --file prs/1.json=.. --expect prs/1.json=<blob read>` (edit, ready, close, reopen, and a merge's claim, release and record) |
| `WReview` | `put prs/1/reviews/<who>.json` (a reviewer's latest verdict, with the commit) |
| `WComment` | `put prs/1/comments/<ms>-<who>.json --if-absent` (`ms+1` on exit 4) |
| `WLand` | `pail land <head> --stack --onto <base> --expect-tip <commit>` |

A client's `begin` is everything up to its first write (reads, decision); the write is held
(`Wait`) until `release`, which lets it through or finds a conflict, after which it reads and
decides again. `push` adds a commit to the head branch, `ci(k)` a passing check, `view` is
`pail pr view --json` (which comments are outdated, whether mergeable). The model's `fileAt`
says what each commit's files hold, which gives anchors their lines.

Properties (`safety` in `pr.qnt`; `simSafety` adds the history ones of `pr_sim.qnt`):

1. `landedMatchesRecord`: the base has commits only for a PR that claimed them, and a merged
   record names the commit on the base. A closed or draft PR is never merged.
2. `claimHasAHolder`: a `merging` record has a client still landing or recording it.
3. `claimedWasReady`: what a PR claimed has its check and is not a draft.
4. `commentsAreAnchored`: a line comment is on a line of its commit's diff (the commit was
   the PR's, the path changed by it, the line one of that side's file); a reply's parent
   exists and the reply is on the parent's line. Comments are create-only, so none is lost.
5. `reviewsAreOfCommits`: a review is of one of the PR's commits.
6. `mergedIsTerminal`: once merged, always (no stale edit, close or ready written over it).
7. `editsAreKept`: the last acknowledged edit of title, body and draft is what the record holds.
8. `landsWhatWasJudged`: the commit on the base is one a merge judged ready, counting only
   approvals of that commit.

Outdated, in words: a line comment is outdated iff the PR is not merging or merged, the head
has moved since the comment's commit, and the line, on the new side, is not what it was there
(changed or gone), or the comment's commit is no longer the PR's. Old-side lines never are
(the base does not move). Checked by `view` in the scenarios.

Design switches (`pr.qnt`), each the fix for a bug the model found in the first plugin; `npm
run mutate:pr` turns each off in a scratch copy and the simulation finds a violation:

| Switch off | What breaks |
|---|---|
| `CAS` | blind overwrite: an edit or ready read before a merge writes `open` back over `merged`; two edits lose one (`mergedIsTerminal`, `editsAreKept`) |
| `CLAIM` | judge, land, then record: a close or draft between judging and landing still merges (`landedMatchesRecord`) |
| `GUARD` | the land takes whatever the head is by then: a push in between lands unchecked commits (`landsWhatWasJudged`) |
| `PER_COMMIT` | an approval survives later pushes (`landsWhatWasJudged`) |

### What the harness does

`tests/pr_refinement.rs` runs each scenario against the real plugin. Every client is a
process of `extensions/pail-pr`, started through `tests/pr_refinement/run_pr.py` (which fixes
`time.time`, so a comment's millisecond is the scenario's, and runs the plugin unchanged),
with `PAIL_BIN` set to `shim.py`: it passes reads to the real `pail` and holds every `put`
and `land` until released. So the interleavings happen at the plugin's real read/write
boundary, and the plugin has no test hook.

| Model | Real |
|---|---|
| action `begin(c, cmd)` | start the plugin with `cmd`'s arguments; wait until it is held at its first write, or exited |
| action `release(c)` | let the held write through; wait likewise |
| `Wait(w)` | the held call: its argv and the file it would store, read back as the model's write (`WSave.expect` is the record whose blob the call names) |
| `Idle`, `res` | the process exited: status 0 `Acked`, else `Refused` |
| action `push` | a commit in a working copy of the head branch |
| action `ci(k)` | `put _meta checks/<commit>/test.json` |
| action `view` | `pail pr view 1 --json`: the outdated comments, `ready` |
| `prs`, `reviews`, `comments`, `checks` | the files on `_meta`, parsed; commits by hash |
| `tip`, `landed` | the head branch's tip; the base's tip (a fast-forward land puts the head's commit there) |

Scenarios: concurrent create, comments in one millisecond, a line comment with a reply and
an outdating push, invalid anchors, edit vs merge (both orders), concurrent edits, an edit
after a merge, close vs merge (both orders), a push before the land, a stale approval,
reviews of a commit, draft and closed never merging with reopen, one merge at a time, and
line comments once the commits are landed.

### What it found

The model of the protocol as the first plugin had it (blind `save()`, judge-land-record) fails
`pr_sim.qnt` with the switches off, and the scenarios fail the first plugin at these
interleavings:

1. `editAfterMergeRun`: carol reads the open PR to edit it; bob merges it; carol writes her read
   back, `state: open` over `merged`. Fix: `put --expect` on the blob read (new in `pail
   put`); on exit 4 re-read and decide again, where an edit of a merged PR refuses.
2. `concurrentEditsRun`: alice sets the title and bob the body from the same read; the second
   write restores the first's field. Same fix.
3. `closeAfterClaimRun` / `closeBeforeClaimRun`: a close racing the merge leaves a closed PR
   whose commits landed, or a merged one that was closed. Fix: the merge claims the record
   (`merging`, by CAS) before landing; edit, ready and close refuse a merging PR, and a claim
   after a close judges the closed record again.
4. `pushBeforeLandRun`: readiness judged commit 1, a push followed, `pail land` landed the
   branch as it was by then. Fix: `pail land --stack --expect-tip <commit>` (new), on every
   attempt; a refused land releases the claim.
5. `staleApprovalRun`: an approval survived a push. Fix: a review records the commit it was
   given on and counts only for that head.
6. `landedRangeRun`: a line comment's default commit (the head) was not checked to be in the
   PR's range, so a comment could anchor to commits already landed.

The mutation check of the plugin itself is in the PR description: each of twenty reversions
of a fix fails a scenario.

### Not modelled

Force pushes and rewrites of the head (an anchor's commit can leave the PR: such a comment
is shown outdated, untested); a base that moves other than by the merge; several PRs per
branch beyond id claiming; `--force`, `--takeover` (a merge that died after its claim);
crashes between a claim and its land; reviews or checks withdrawn while a merge is between
judging and claiming; policy files; pail-web (its server follows the same rules and is
tested in `web/test`, not refined).

## Actions (designed, not yet built) (`actions/`)

(The files live in `actions/`, not beside the others: `quint-refinements` digests every `.qnt`
in the directory of the scenarios it compiles, so one more here would make all four generated
artifacts drift.)

pail Actions is a workflow system over a bucket: orchestrator replicas own run and job state
on the `_actions` and `_checks` branches (Tier 1: commits, `put --expect`, `--if-absent`), and
runners touch only plain objects under `<repo>.actions/` (Tier 2: queue, leases, progress,
results, cancel markers). The orchestrator is the one bridge from the second to the first.
There is no implementation yet: the model comes first, and the orchestrator and runner are
built and refined against it. `actions.qnt` models one workflow (jobs `build`, `test`,
`deploy`; the last two need `build`, `deploy` waits for an environment approval), runs from
three event keys (two pushes in one concurrency group with cancel-in-progress, one cron
slot), two orchestrator replicas, three runners, two attempts and a small clock (`TTL`).

A replica's tick is `read`, then `write`: it observes both tiers, decides one intent and holds
the commit at its write, which lands or finds a conflict; so every write is from a stale
read. Runners claim a lease (`claimRead`, then `claimWrite`: create, or take over an expired
one by generation match), refresh it, report a step started, write a create-only result, see a
cancel marker, or crash. Approvers create approval files.

| Intent | Commit |
|---|---|
| `ITrigger` | a run from an event key, cron slot: `runs/<id>` (`--if-absent`), `claims/<key>` (`--if-absent`), its jobs, `concurrency/<group>` (CAS), cancelling the group's previous run |
| `IPromote` | a pending job whose `needs` succeeded is queued, or waits for its environment's approval; one whose need failed is skipped |
| `IStart` | a queued job whose lease a runner holds is in_progress under (holder, seq) |
| `IAccept` | a runner's result becomes the job's conclusion, if fenced |
| `ISweep` | a lease expired with no result: the job is requeued if no step started, else fails "runner lost" (a cancelled run's job is cancelled) |
| `IFinish`, `IRerun` | the run concludes; a new attempt copies the successes, restarts the rest and takes its group back |
| `ICheck` | the `_checks` projection of a completed job |

Properties (`safety` in `actions.qnt`; `simSafety` adds the history ones of `actions_sim.qnt`):

1. `atMostOneAccepted` (sim): at most one runner's result is ever accepted per (run, attempt,
   job); `acceptedWasHolder`: the result a job concluded by is the holder's and seq's it
   started under.
2. `noStranded`: a job is never in_progress with a lapsed lease unless the orchestrator can
   accept its result or sweep it (the state form of "no job stays in_progress forever").
3. `finality` and `attemptsAreFrozen` (sim): `completed`, its conclusion and who concluded it
   never change; a rerun is a new attempt and the old one keeps its conclusion.
4. `ordering`: a job is queued, waiting, started or concluded by a runner only when every
   need succeeded, and is skipped only when one did not.
5. `checksEarned`: a `_checks` success names a job that an accepted result (or a copy of one)
   concluded success, of a run at that sha.
6. `oneActivePerGroup`: at most one run of a group is active and not cancelled; with
   `cancelledIsIdle`: a cancelled run has nothing left to start.
7. `gatesHold`: an environment job is queued only with an approval from someone other than
   the run's actor.
8. `oneRunPerKey`, `oneCreationPerKey` (sim): an event key or cron slot has one run, and one
   commit ever created it, across replicas.

and `leaseExclusive`: no two runners hold one job under one seq.

Design switches (`actions.qnt`), each `true` the fix; `npm run mutate:actions` turns each off in
a scratch copy and the simulation finds a violation:

| Switch off | What breaks |
|---|---|
| `CAS` | a stale replica overwrites a record with its read: a finished job restarted or concluded twice (`finality`, `atMostOneAccepted`) |
| `IF_ABSENT` | two replicas claim one run id, or one key, and overwrite each other (`oneRunPerKey`, `oneCreationPerKey`) |
| `GUARD_READS` | a commit expects only the records it writes: a sweep requeues a job of a run cancelled since it read, a push does not see the run it found done restarted by a rerun (`cancelledIsIdle`, `oneActivePerGroup`) |
| `LEASE_CREATE_ONLY` | a blind lease put: two runners hold one job under one seq (`leaseExclusive`) |
| `FENCE` | any result of an in_progress job is accepted: a stalled runner's late result concludes the job its successor holds (`acceptedWasHolder`) |
| `FENCE_ON_JOB` | intake also compares the live lease's seq (the plan's "lease seq matches"): a lease taken over after the job started strands the job (`noStranded`) |
| `RESULT_PER_LEASE` | one create-only result path per job: a late result of a lost runner blocks the next holder's, and the sweep sees a result (`noStranded`) |
| `SWEEP` | a runner that dies leaves its job in_progress for good (`noStranded`) |
| `NEEDS_CHECK` | a job is queued before its need concluded (`ordering`) |
| `APPROVAL_EXCLUDES_ACTOR` | the actor approves their own deploy (`gatesHold`) |
| `CANCEL_IN_PROGRESS` | a push does not cancel the run it supersedes (`oneActivePerGroup`) |
| `RERUN_NEW_ATTEMPT` | a rerun resets the attempt's jobs in place (`finality`, `attemptsAreFrozen`) |
| `CHECK_FROM_JOB` | `_checks` is written from the runners' raw results: a late success becomes a check on a failed job (`checksEarned`) |

No single switch breaks `atMostOneAccepted`: a result is accepted once by the fence (a job takes
only the result under its holder and seq) and once by the guard on the job file (a second
accept of a finished job conflicts). `mutate:actions` turns both off to show it can fail.

What modelling found in the design (before any code), each now a rule of the model:

1. A result path needs the lease seq (`results/<run>/<att>/<job>/<seq>`): with one create-only
   path per job, a late result of a lost runner blocks the next holder's, and the sweep (which
   sees "a result") leaves the job in_progress for good.
2. Intake compares the result with the job file's holder and seq, not also with the live lease:
   a lease can be taken over (the queue entry outlives the start) after the job file named the
   first holder, and "the lease seq matches" would then strand the job.
3. A lease's `seq` counts claims, not refreshes, and an expired lease is taken over by
   generation match (not deleted and created again, which would reuse seq 1).
4. A commit expects every record it read, not only those it writes. Simulation found two
   races with only the written records guarded: a push that cancels run 1 did not expect the
   job a sweep requeued meanwhile (a cancelled run kept a queued job: `cancelRacesSweepRun`),
   and a push that found the previous run done did not expect the run a rerun then restarted
   (two active runs in one group: `rerunRacesPushRun`). So a trigger or rerun expects the
   group file, the group's previous holder and, when it cancels it, every job of it; a sweep
   the run.
5. A sweep of a cancelled run's job concludes it cancelled, not requeued.
6. A rerun is a new commit into the concurrency group: it cancels a newer active run.
7. `_checks` is projected from the accepted job in `_actions`, never from `results/`, and is
   repaired by reconcile (it is a second branch, written after the first).

Witnesses (`npm run witness:actions`): a lost runner's job is requeued and runs again, a run is
cancelled, a run is rerun, an environment job is queued on an approval, a job is skipped.
`actions_scenarios.qnt` has 18 runs, for the plan's UC1, UC2, UC3, UC4, UC6, UC7 and UC8
(and UC9's projection), written as single actions so that they compile with
`quint-refinements` (checked by hand: the artifact is large, so none is committed). The
refinement harness comes with the orchestrator (T1.7) and the runner (T2.5); until then the
scenarios run in Quint only (`npm test`).

Not modelled: matrices and expressions, logs, progress beyond "a step started", artifacts and
caches, secrets, retention, `workflow_dispatch` requests (a second kind of create-only
trigger), several workflows, the orchestrator's leader lease (every write is guarded, so
correctness does not depend on it), batching of several intents in one commit (a commit here
is one intent), clock skew between replicas and runners, and runner restarts (a crash is
for good).

## Not modelled yet

Nothing on the list. What is still abstracted is named in each section's simplifications:
one branch for writers, four clients, two paths, one rewrite per client, a stack of one
commit, the pack-file layout (ranges, `INLINE_MAX` and `PACK_MAX` splitting), real chunk
boundaries, time as ticks with the stated bounds, and the relocation budget (two paths cannot
show the 256 KiB floor, the 4 MiB cap or the manifest-leaf charge; `src/amortize.rs`'s unit
tests do).

## Lease timing (`clock.qnt`)

A lease is rewritten every `LEASE_TTL / 3` by its holder's refresher. The others ignore it
once its LastModified is older than `TTL` by their clock. The holder trusts it, for a ref
swap or a gc delete, while `Lease::check` finds it refreshed within `TTL / 2`. So a holder
that stalls looks dead to the others (at `TTL`) only after it knows it (at `TTL / 2`), and the
`TTL / 2` between is the margin for everything the model cannot see.

Time is a count of ticks, a sixth of `TTL` each (`TTL` 6, a refresh every 2, a check at 3),
so simulation stays fast; the harness maps a tick to 100 s.

```text
  t=0     the refresh lands (LastModified)
  t=3     a holder's last chance to pass Lease::check          ┐ LATENCY (D): the swap or
  t=3+D   ... and the last tick its request can land           ┘ delete it checked for lands
  t=6-S+1 the first tick a lister, its clock S ahead, may ignore the lease

  safe  ⇔  D + S ≤ MAX_AGE (= TTL / 2, in ticks)
```

1. State: `now`, and per holder (clients and gc) `timing`: `running` (its lease is held:
   its refresher runs), `refreshed` (what `check` measures from), `done`, and the refresh
   in flight (`inflight`, `sent`, `landed`; before the lease is held, `sent` is when its first
   put was sent). A lease object in `locks` carries `at`, its
   LastModified, and `leasedByOthers` filters by it.
2. `tick`: time passes; a holder whose refresh is due (`REFRESH` after its last) sends it.
   `refreshLand(h, d)` is the put reaching the store, whenever (a stall is a refresh that
   never lands, or lands late); `refreshAck(h)` is its reply.
3. Every request guarded by `Lease::check` is guarded in the step that sends it
   (`guarded`): the commit's, a combining round's and a landing's swap (`advance_ref`), a
   rewrite's swap (`rewrite_ref`) and each of gc's deletes. A refused swap through
   `advance_ref` is an error, which `commit_opts`, the combiner and `land_stack` take as an
   unknown outcome and recheck: the model's `Dropped`. A refused `rewrite_ref` returns the
   error (`Unknown`); a refused gc stops and releases its lease.
4. The lease counts from the instant before its put (`holding`: the lease's `at`;
   `refreshAckNext`: `sent`), and a refresh answered after the holder went stale does not
   count.

Assumptions, named so the model can say when it is safe:

| | | Model | Checked |
|---|---|---|---|
| D | `LATENCY`: ticks a request may take to land after the check that allowed it (a stall between check and request) | 1 | `tickCan` blocks time while a checked swap or delete is on its way for D ticks |
| S | `SKEW`: how far the lister's clock may run ahead of the store's stamping LastModified | 1 | `leasedByOthers` adds it to a lease's age; the harness's test clock runs it ahead |
| | clocks otherwise run at the same rate (no drift) | | |
| | the lister reads its clock once per listing (the real one reads it when it sends the request, which only makes leases look younger: more cautious); the listing is one step | | the gate shifts LastModified by the ticks a listing waited |

Safety holds iff `D + S ≤ TTL / 2`. `quint run pail.qnt --invariant=safety` with each pair
(`LATENCY`, `SKEW` edited), 60,000 runs of 60 steps: no violation for (D, S) = (0, 0), (1, 1),
(2, 1), (1, 2), (3, 0), (0, 3); 400,000 runs break `safety` for (2, 2), (3, 1), (4, 0) and
fewer for (1, 3), (0, 4), (5, 0). In seconds of the real
constants: a process may be paused up to 5 minutes between check and request, less any skew
between machines. A pause longer than `TTL / 2` in `Lease::check` itself, a swap that waits
longer in a retry, or a clock that jumps are what no margin protects against. Put latency
is not in the bound: a lease counts from before its put.

- Scenarios: `expiredLeaseIsIgnoredRun` (a dead holder ignored at `TTL` on the lister's
  clock, not before), `staleWriterRefusesSwapRun`, `staleRewriterRefusesRun`,
  `staleLanderRefusesRun`, `staleCombinerRefusesRun` (each swap site refuses a stale
  holder), `swapAtTheEdgeOfTheMarginRun` (age `MAX_AGE` passes), `refreshCountsFromItsSendRun`,
  `leaseCountsFromItsPutRun`, `gcLeaseCountsFromItsWriteRun`, `lateRefreshDoesNotReviveRun`,
  `staleGcRefusesToDeleteRun`, `staleGcRefusesToDeletePackRun` and `staleGcRefusesToDeleteChunkRun` (pack and chunk deletes are guarded like commit and page deletes).
- Mutants of `src/lease.rs`, `src/gc.rs` and `src/repo.rs`: fourteen, each undone after the run. All fail a scenario: gc without its check
  (`staleGcRefusesToDeleteRun`), a lease counting from after its put (`leaseCountsFromItsPutRun`)
  or from after its listing (`expiredLeaseIsIgnoredRun`), a refresh counting from its reply
  (`refreshCountsFromItsSendRun`), a late refresh reviving (`lateRefreshDoesNotReviveRun`),
  `Lease::check` against `TTL` instead of `TTL / 2` (both copies: `staleGcRefusesToDeleteRun`,
  `staleWriterRefusesSwapRun`) or refusing at `MAX_AGE` (`swapAtTheEdgeOfTheMarginRun`), a
  lease expiring at `TTL` rather than after it, or never (`expiredLeaseIsIgnoredRun`, both), a
  refresher every `TTL / 2` (`expiredLeaseIsIgnoredRun`), and no check before `advance_ref`'s
  swap or `rewrite_ref`'s (`staleWriterRefusesSwapRun`, `staleRewriterRefusesRun`). The
  fourteenth, no check before `create_branch`'s ref write, fails
  `staleBranchCreateRefusesRun` (the check is when the lease listing returns, and the ref
  write is a request of its own)
- Simplifications: the stack's commit step takes its lease and releases it inside one step, so
  it takes no time (`create_branch`'s lease is stepped); a gc that waits longer than `MAX_AGE` for writers' leases to go has
  an unrefreshed lease from the start, so it finds itself stale at its first delete (it
  gives up and is run again); `Lease::shared`/`exclusive`'s `wait` and backoff use real
  time; a holder that dies stops its refresher with it (`crash`), and a stalled one
  (refreshes not landing) is the other way to go stale.

## Notes

- `DIRECT_ATTEMPTS` mirrors the constant in `src/repo.rs` by hand.
