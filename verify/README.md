# verify: a model of pail, and the code checked against it

A [Quint](https://quint-lang.org) model of how `Repo::commit` publishes over a bucket, how
`Repo::rewrite_ref` rewrites it, how `Repo::land_stack` lands a stack on it and how `gc`
collects it, the properties they must keep, and scenarios the real code is checked
against request by request with
[quint-refinements](https://github.com/ArchAstro/quint-refinements). Beside it: the store
contract every backend follows, and the branch policy every enforcement point decides.

Layers, each importing those below. A feature is a set of pure steps (`xCan(s, ...)`:
may it happen; `xNext(s, ...)`: the state after) over one shared state, so a new protocol
(combine, land, rewrite) is a new file of steps plus actions in `pail.qnt`:

```text
store.qnt     any object store: versioned objects, create / CAS / put / delete, lost requests
   ▲
bucket.qnt    pail's layout in it: pages/, commits/ (create-once), refs/ (CAS, lineage), locks/,
              intents/, reflog/
   ▲
world.qnt     the shared state: bucket, writers, the collector, ghosts
   ▲
publish.qnt   Repo::commit steps (lease, read, upload, swap, recheck; crash, expiry)
combine.qnt   commit_combining steps (intent, poll, gather + store the batch, swap, cleanup)
rewrite.qnt   rewrite_ref steps for rebase, undo, reset (read, lease, store, read + carry + record, swap, recheck)
land.qnt      land_stack steps (make the stack; lease, read + fork point + copy, swap, recheck)
gc.qnt        gc steps (exclusive lease, drain, mark, list, delete, release)
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
| `combine.qnt` | Writers that lost their direct attempts, running `commit_combining` |
| `rewrite.qnt` | A client rewording the tip, undoing the last rewrite or resetting the tip, running `rewrite_ref` |
| `land.qnt` | A client making a stack (a branch of its own) and landing it with `land_stack` |
| `gc.qnt` | The collector running `gc::gc_with`, one store request per step |
| `pail.qnt` | Actions, `step`, and the properties (`safety`) |
| `scenarios.qnt` | `@conformance` runs: interleavings the real writers and gc must follow |
| `policy.qnt` | Branch policy: parsing `policy.json`, `decide`, `request_op` |
| `policy_scenarios.qnt` | `@conformance` runs: documents to parse and requests to decide |
| `quint-refinements.json`, `store-refinements.json`, `policy/policy-refinements.json` | Generated scenario artifacts (do not edit) |
| `../tests/*/generated.rs`, `policy/pailpolicy_refinement_gen_test.go` | Generated Rust traits and Go interface (do not edit) |
| `../tests/refinement.rs` | `Repo::commit`, `Repo::rewrite_ref`, `Repo::land_stack` and `gc` against `pail.qnt`: real actors over a shared in-memory bucket |
| `../tests/store_refinement.rs` | Each `Store` backend against `store_spec.qnt` |
| `../tests/policy_refinement.rs` | `Policy::parse`, `Policy::decide`, `policy::request_op` against `policy.qnt` |

## Run

```sh
cd verify && npm ci
npm test            # scenarios pass in Quint
npm run simulate    # random interleavings against `safety` (`step`, then writers only: `contendStep`)
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

1. `branchIsComplete`: every commit a ref reaches (the branch, each stack), and its page,
   is stored.
2. `refOnlyMovesForward`: every move of the ref keeps what it named before, unless a
   rewrite's record names the move.
3. `publishedIsOnBranch`: a client told "published" finds its commit on the branch (unless
   an undo or reset took it off since).
4. `conflictedHasNothingOnBranch`: a client told "conflict" landed nothing.
5. `landsAtMostOnce`: one `commit` call lands at most one commit.
6. `gcRunsAlone`: while gc marks and deletes, no writer is between its first read and its
   return.
7. `recentIsTruthful`, and the rewrite properties `lineageNamesItsRecord`,
   `recordsAreChained`, `reflogIsStored`, `rewriteOutcomeIsTruthful`,
   `rewritesDropOnlyTheirTarget`: see the combining and rewrite sections.
8. `publishedOnce`: each client's change enters the branch at most once by a publish (a
   move no record names); after an undo or reset takes it off, only a rewrite brings it
   back.

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
| action `stack(c, delta)` | `c`'s stack, made whole past every gate: `write_commit` on the branch's tip, `create_branch c` (each under a lease) |
| action `beginLand(c)` | `land_stack(c, main)` spawned; it leases first |
| action `ldRead(c)` | its `GET refs/main` (the stack's ref and records are read past the gate), then the copy's upload |
| action `ldSwap(c, d)` / `ldRecheck(c)` | its conditional `PUT refs/main`; the ref read after one it did not win |
| action `rwStore(c)` | under `writing()`'s lease, the wave of `PUT commits/<hash>` for the plan's commits (`rebase::store`) |
| action `rwRead(c)` | `rewrite_ref`'s `GET refs/main`, then each carried commit's upload and the `PUT reflog/main/<id>` |
| action `rwSwap(c, d)` / `rwRecheck(c)` | its conditional `PUT refs/main`; the ref read after an unanswered one |
| `MARK` (`~carol`) | a commit with `rewritten.from`: its parent's name, its original's writer, the rewriter |
| `reflog` (ids 1, 2, ..) | records in the order they were written; `lineage.rewrite` and `prev` by that number |
| `INTENT_NAME` (`alice+`) | a round's commit for an intent: its parent's name and the intent's writer (`pail.intent`) |
| `Comb` | the poll's ref read and listing, and the commits the round stored |
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
   tip; `beginRebase`: drop its parent, drop the tip, or swap the two). It reads the ref
   and builds the plan in memory as `rebase::build` does (each picked commit kept if it
   still sits on the history so far, else its change checked to apply and written over
   it, dropped if that changes nothing; a commit that does not apply is a conflict before
   any lease), then leases and stores the commits it made in one wave.
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
  leased), `rebaseEmptyRun` (commits whose changes are already there dropped as empty).
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
- Simplifications: plans are over the tip's two commits, without squash or fixup (the
  model names a commit by one author); a reset only drops the tip; each client rewrites
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
- Simplifications: a stack is one commit, made whole (its own lease and requests are not
  steps), and is not moved after; a client lands at most its own stack; `land --stack`'s
  CLI (pre-publish hook, reporting) is not modelled.

## Not modelled yet

1. Squash and fixup plans; successor walks
   (`Repo::successor`, `fork_point`) beyond the record's map.
2. Packs and chunks (files are inline in the model's trees).
3. Known heads (the fast path that skips the first ref read).
4. Lease timing: a live holder is assumed to refresh within the lease's lifetime
   (`Lease::check` refuses a ref swap otherwise); only dead holders' leases expire.

## Notes

- `DIRECT_ATTEMPTS` mirrors the constant in `src/repo.rs` by hand.
