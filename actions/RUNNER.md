# pail-actions runner

The runner daemon of docs/actions.md sections 5 and 8 (tasks T2.1, minimal T2.3). It pulls
jobs from Tier 2 of a store, runs them with nektos/act as a library, and reports through
Tier 2 only. It never touches Tier 1 (`_actions`, `_checks`).

## Run it

```
pail-actions runner --store gs://B/repo --labels ubuntu-latest,self-hosted \
    [--name ns-7f2c] [--once] [--pail /path/to/pail] \
    [--host | --image debian:stable-slim --linux-pail /path/to/linux/pail] \
    [--ttl 90s] [--refresh 15s] [--poll 2s]
```

1. `--store` is the repo store; Tier 2 is its `.actions` area, reached with
   `pail --store URL obj --area actions ...` (needs a pail with `obj`; `--pail` or `$PAIL_BIN`).
2. `--once` waits for one claimable job, runs it, exits (the ephemeral shape a scaler starts).
3. `--host` runs steps on this machine (act `-self-hosted`); without it, docker.
4. Docker mode mounts a linux pail at `/usr/local/bin/pail` (checkout is `pail clone --at`)
   and, for `file://` stores, the store directory read-only.

## Per job

```
list queue/<label>/ -> read entry -> claim leases/<run>/<att>/<job> (create | takeover seq+1)
  every --refresh: lease version-match refresh (failure = lost: stop all writes, cancel act),
                   cancel/<run>/<att>/<job> check, progress/<run>/<att>/<job>/<seq>.json
  every 2 s / 64 KB: logs/<run>/<att>/<job>/<seq>/<n>.log
  end: logs/.../<seq>/full.log, results/<run>/<att>/<job>/<seq>
runners/<id>.json heartbeat every --refresh; deleted on exit.
```

A queue entry is turned into a one-job workflow for act (`runner.Workflow`). act's github
context (sha, ref, event name, run id, repository) comes from the entry.

## Tests

1. `go test ./internal/tier2 ./internal/runner`: unit tests on `MemStore` (claim race,
   takeover bumps seq, refresh failure stops writes, cancel, seq in paths, log chunking).
2. `internal/runner/integration_test.go`: real act in host mode over a `file://` store, via
   `PailStore`. Needs a pail binary (`PAIL_BIN`, or `target/{release,debug}/pail`); skipped
   otherwise. If that binary has no `pail obj`, Tier 2 is served by `tier2/fakepail`, a shell
   script that implements the fixed `obj` contract; the checkout still uses the real pail.
3. Docker mode is manual (CI has no docker). Verified on arm64 Docker 29:

```
cargo build --release --bin pail                       # host pail (store setup + obj)
docker run --rm -v "$PWD":/src:ro -v /tmp/linux:/out -e CARGO_TARGET_DIR=/out/target \
    -w /src rust:1-bookworm cargo build --release --bin pail   # linux pail
pail-actions runner --store file:///tmp/s --labels ubuntu-latest --once \
    --pail ./target/release/pail --image debian:stable-slim \
    --linux-pail /tmp/linux/target/release/pail
```

Result of the manual run (2026-11-02, `debian:stable-slim`, linux/arm64): result `success`
with both steps `success`, and full.log shows `file-says: hello from pail`, the container's
`uname`, and `GITHUB_SHA` equal to the entry's `sha`. In that run Tier 2 was served by the
`tier2/fakepail` script passed as `--pail` (the real pail had no `obj` yet); the job's own
checkout used the linux pail mounted in the container.

## Done / not done vs section 8

Done: queue scan, lease claim/takeover/refresh, loss detection, cancel marker, progress,
log chunks and full.log, results with seq, runner heartbeat, checkout by `pail clone --at`,
github context from the entry, step progress from act's logger, job `timeout_minutes`.

Not done:

1. Secrets: `secrets[]` is ignored with a log line; masking has only act's own `::add-mask::`
   handling (`Chunker` has a mask hook, unused).
2. Job `outputs` (result `outputs` is `{}`), `needs` outputs as inputs.
3. Artifacts, cache (`artifacts/`, `cache/` over Tier 2), `upload-artifact` and `cache` actions.
4. Per-job downscoped token, label to image mapping (one `--image` for all labels).
5. A failed `pail obj` call is retried only by the next tick; no backoff.
6. `full.log` is built in memory; very large logs should spool to disk.
7. Scaler (T2.2): none here.
8. act is not a security boundary; run untrusted jobs in an isolated VM.
