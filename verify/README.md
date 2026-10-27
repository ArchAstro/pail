# verify: a model of publishing, and the code checked against it

A [Quint](https://quint-lang.org) model of how `Repo::commit` publishes over a bucket, the
properties it must keep, and scenarios the real client is checked against request by
request with [quint-refinements](https://github.com/ArchAstro/quint-refinements).

Three layers, each importing the one below, so a new protocol (land, combine, rewrite, gc)
adds actions on the same bucket and a new kind of object adds a space to `bucket.qnt`:

```text
store.qnt     any object store: versioned objects, create / CAS / put / delete, lost requests
   ▲
bucket.qnt    pail's layout in it: commits/ (create-once), refs/ (CAS)
   ▲
publish.qnt   Repo::commit, one store request per action; the properties
```

```text
publish.qnt ──quint run──▶ properties hold in every simulated interleaving
     │
scenarios.qnt ──compile──▶ quint-refinements.json + tests/refinement/generated.rs
                                   │
tests/refinement.rs ── real Repo::commit, gated store ──▶ same state after every step
```

## Files

| File | What it is |
|---|---|
| `store.qnt` | The object store, independent of pail: `Space[k, b]` of versioned objects, writes, `Delivery` |
| `store_spec.qnt` | One action per `Store` method, each recording its reply in `state.reply` |
| `store_scenarios.qnt` | `@conformance` runs: the store contract every backend must follow, reply for reply |
| `bucket.qnt` | Pail's spaces in the store and the operators on them (`uploaded`, `refSwapped`, `tip`) |
| `publish.qnt` | Clients running `commit_opts`, one store request per action; the properties |
| `scenarios.qnt` | `@conformance` runs: interleavings the real client must follow |
| `quint-refinements.json`, `store-refinements.json` | Generated scenario artifacts (do not edit) |
| `../tests/refinement/generated.rs`, `../tests/store_refinement/generated.rs` | Generated Rust traits (do not edit) |
| `../tests/refinement.rs` | `Repo::commit` against `publish.qnt`: real clients over a shared in-memory bucket |
| `../tests/store_refinement.rs` | Each `Store` backend against `store_spec.qnt` |

## Run

```sh
cd verify && npm ci
npm test            # scenarios pass in Quint
npm run simulate    # random interleavings against `safety`
npm run compile     # after editing scenarios.qnt or publish.qnt
cd .. && cargo test --release --test refinement --test store_refinement
# real S3/GCS (or emulators) too:
PAIL_TEST_STORES="s3://bucket/prefix" AWS_ENDPOINT=... cargo test --release --test store_refinement
```

CI runs all four (`npm run check` in place of `compile`, failing on stale generated files).

## Properties (`publish.qnt`)

1. `refNamesStoredCommit`: the ref never names a commit that is not stored.
2. `refOnlyMovesForward`: every move of the ref keeps what it named before.
3. `publishedIsOnBranch`: a client told "published" finds its commit on the branch.
4. `conflictedHasNothingOnBranch`: a client told "conflict" landed nothing.
5. `landsAtMostOnce`: one `commit` call lands at most one commit.

`npm run simulate` checks them over random interleavings (simulation, not proof: it
samples behaviours rather than exhausting them).

## The store contract (`store_scenarios.qnt`)

Each action is one `Store` call (`getOpt`, `putNew`, `putAny`, `casNew`, `cas`, `remove`,
`listKeys`). After each, the objects in the store (bodies, versions, tombstones) and the
call's reply must equal the model's.

| Backend | Checked by | Result |
|---|---|---|
| in-memory (`object_store` `InMemory`) | `cargo test` | 6/6 |
| `file://` (pail's local CAS) | `cargo test` | 6/6 |
| S3 API (moto) | CI `s3` job, `PAIL_TEST_STORES` | 6/6 |
| GCS | `PAIL_TEST_STORES=gs://...` | not run: fake-gcs-server lacks the XML API `object_store` uses |

Model versions count writes. Real stores differ when an object gets back bytes it had
before: an S3 ETag and pail's local version hash the bytes, so the version comes back;
a GCS generation and an in-memory ETag do not. A first version of the scenarios asserted
"never reused" and failed on `file://` and S3. Pail does not need it (a ref's body is its
whole state), so `store.qnt` says so and no scenario rewrites old bytes.

## How the publish harness maps to the model

| Model | Real |
|---|---|
| action `readRef(c)` | `c`'s next `GET refs/main` |
| action `upload(c)` | `c`'s next `PUT commits/<hash>` |
| action `swap(c, Answered)` | `c`'s next conditional `PUT refs/main`, answered |
| `swap(c, Lost)` | that PUT performed, its reply replaced by an error |
| `swap(c, Dropped)` | that PUT failed without reaching the bucket |
| action `recheck(c)` | the ref read after an unanswered swap |
| `CommitId {by, gen}` | the commit hash, named by who uploaded it and its generation |
| `Versioned.version` | the object's ETag, numbered per key in the order the bucket issued them |
| `phase` | the request the client is held at (or that it returned) |
| `outcome` | what `commit` returned: Ok, `ConflictError`, another error |

The refinement map (real state to model state, `Harness::state` in `tests/refinement.rs`)
has the same two layers: `space()` reads each object under a prefix with its version
(`store.qnt`), and `bucket_value()` decodes commits and refs into `bucket.qnt`'s types.

Every client is a real `Repo` behind a gate (`Gate` in `tests/refinement.rs`) that holds the
requests above until the scenario releases them; everything else (pages, intent listings,
commit reads) passes straight through. After each action the harness waits until the
released client is held at its next request or has returned, then compares the whole state:
the bucket's commits, trees and ref, and each client's phase, attempt, last read and outcome.

## What it found

`timeoutLandedUnderRun`: bob's swap lands but the reply is lost; alice lands on top before
bob rechecks. Bob saw a head that was not his commit, retried, found his own change in the
way and returned a conflict for a commit that was published. `quint run` found the
interleaving in under a second; the scenario reproduced it against the real code; the fix
(`commit_opts` looks for its commit under the head) made it refine.

## Not modelled yet

1. Combining under contention (intents): a client that loses every direct attempt stops as
   `Combining`.
2. `land --stack`, rewrites and epochs (`rewrite_ref`), successor maps.
3. GC deleting unreferenced objects.
4. Known heads (the fast path that skips the first ref read).

## Notes

- `DIRECT_ATTEMPTS` mirrors the constant in `src/repo.rs` by hand.
