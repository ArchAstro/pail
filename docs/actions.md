# pail Actions: the storage contract

**Status: designed, not built.** This is the contract the orchestrator, runner, scaler and
CLI are written against. The rules here are the ones `verify/actions/` checks
(`actions.qnt`, `actions_sim.qnt`, `actions_scenarios.qnt`; see the Actions section of
`verify/README.md`). Where this file and the model disagree, the model wins. Parts the model
does not cover are marked **(unmodelled)**.

pail Actions runs GitHub Actions workflows (`.github/workflows/*.yml`, unchanged) over a
bucket. Like `pail pr` and `pail ci` (see [extending.md](extending.md)), it is programs that
agree on names: no pail server, all state in the bucket.

```
 events (push, PR, cron, CLI request)
        │  pail events --branch '*' --prefix actions/ --cursor …  (+ safety poll)
        ▼
 ┌──────────────────┐  commits, CAS / --if-absent   ┌──────────────────────────────┐
 │ orchestrator     │ ────────────────────────────▶ │ TIER 1  branches             │
 │ (stateless,      │ ◀──────────────────────────── │   _actions   _checks         │
 │  N replicas)     │                               └──────────────────────────────┘
 │                  │  create / delete              ┌──────────────────────────────┐
 │                  │ ────────────────────────────▶ │ TIER 2  <repo>.actions/      │
 └──────────────────┘ ◀──────────────────────────── │   queue leases results …     │
        ▲  reads results, checks the fence          └──────────────────────────────┘
        │                                              ▲ list/claim/refresh/report
   scaler (docker | static | k8s | …)  ──starts──▶   runner (pulls work; never writes Tier 1)
   CLI: pail run/workflow/secret …  ──writes──▶  Tier 2 requests/, Tier 1 approvals/secrets
```

## 1. Two tiers, chosen by write rate

| | Tier 1: branches | Tier 2: `<repo>.actions/` prefix |
|---|---|---|
| What | `_actions`, `_checks` | plain objects, no commits |
| Use for | transitions: rare, transactional, audited (`pail log`) | heartbeats, progress, logs, queue, results |
| Atomicity | one commit, many files | one object |
| Concurrency | `pail put --expect` (CAS), `--if-absent` | create-only, generation match (the lease primitive) |
| Rate | about 1 commit/s per branch (one ref object) | about 1 write/s per object; each runner writes its own objects |
| Expiry | retention sweep (90 d), history is permanent | bucket lifecycle rules |
| Writers | orchestrator; admins; approvers | orchestrator, runners, CLI |

Rules:

1. **Runners never touch Tier 1.** The orchestrator is the only bridge: it reads what a
   runner reported in Tier 2, checks it against the job file, and commits the transition.
2. **`_checks` is writable only by `role:actions`** (`_policy`), so a PR author cannot write
   their own success (see [protecting-branches.md](protecting-branches.md)).
3. **Branch names in paths** escape `/` as `~` (`pr/hello` is `pr~hello`).
4. **Every key is idempotent to retry.** Creating what exists, or deleting what is gone, is
   not an error to the caller.

Write modes used below: **CAS** = `pail put --expect` (the record is as read); **create** =
`--if-absent` / generation 0; **gen-match** = overwrite only the generation read; **put** =
plain overwrite, safe only because one writer owns the key.

## 2. Enums (GitHub's, verbatim)

| Entity | `status` | `conclusion` (set once, with `completed`) |
|---|---|---|
| run | `requested`, `queued`, `in_progress`, `waiting`, `completed` | `success`, `failure`, `cancelled`, `skipped`, `timed_out`, `action_required`, `startup_failure` |
| job | `queued`, `waiting`, `in_progress`, `completed`; internal `pending` | `success`, `failure`, `cancelled`, `skipped`, `timed_out` |
| step | `pending`, `in_progress`, `completed` | `success`, `failure`, `cancelled`, `skipped` |
| check (`_checks`) | `queued`, `in_progress`, `completed` | `success`, `failure`, `cancelled`, `skipped`, `timed_out` |

`pending` (a job whose `needs` are not met) is stored but reported as `queued` by any
GitHub-shaped API. Other GitHub conclusions (`neutral`, `stale`) are never written.

```
job:  pending ─needs ok─▶ queued ─lease held─▶ in_progress ─accepted result─▶ completed
        │ need failed          │ ▲ environment gate                 │ lease lost, no step started
        ▼                      ▼ │ approved                         └──────▶ queued (lost +1)
   completed/skipped        waiting                       lease lost, step started ─▶ completed/failure
        any non-completed ── run cancelled ─▶ completed/cancelled
```

## 3. Tier 1: `_actions`

Writer is always the orchestrator unless stated.

| Path | Mode | Meaning |
|---|---|---|
| `workflows/<file>.json` | admin, CAS | `{"enabled", "by", "at"}` |
| `runs/<id>.json` | create, then CAS | the run (§3.1) |
| `runs/<id>/<att>/jobs/<job>.json` | create, then CAS | one job of one attempt (§3.2) |
| `runs/<id>/<att>/approvals/<env>/<who>.json` | approver, create | `{"who","at"}`; one file per person, so approvers never contend |
| `claims/<key>` | create | `{"run": id}`: the run created for an event key or cron slot (§3.3) |
| `concurrency/<group>.json` | CAS | `{"run": id}`: newest run of a group |
| `numbers/<workflow>/<n>` | create | run-number claim, in the run's commit **(unmodelled)** |
| `idx/{branch,workflow,sha}/…/<id>` | create | empty markers for listing runs, in the run's commit **(unmodelled)** |
| `secrets/<NAME>.json` | admin, CAS | `{"ref": "gcpsm://…", "env": null}`: a reference, never a value **(unmodelled)** |
| `vars/<NAME>.json` | admin, CAS | `{"value"}`, plaintext **(unmodelled)** |

### 3.1 `runs/<id>.json`

```json
{ "id": 4182, "run_number": 311, "workflow": ".github/workflows/ci.yml", "name": "ci",
  "event": "push", "head_sha": "9f3ab21c5d0e47a1b8c63f20d9e4a7b15c2e8f30", "head_branch": "main",
  "actor": "user:alice", "key": "ci.yml/push/main@9f3ab21c5d0e",
  "run_attempt": 1, "status": "in_progress", "conclusion": null, "cancel": false,
  "concurrency": { "group": "ci-main", "cancel_in_progress": true },
  "created_at": "2026-10-30T17:02:11Z", "run_started_at": "2026-10-30T17:02:14Z",
  "updated_at": "2026-10-30T17:02:14Z" }
```

1. `run_attempt` is the *current* attempt; older attempts' jobs are frozen.
2. `cancel: true` means the run was superseded or cancelled: nothing of it may start.
3. `conclusion`: `cancelled` if `cancel`, else `failure` if any job failed, else `success`.
4. Lifecycle: kept 90 days, then removed in batches of 500 paths per commit.

### 3.2 `runs/<id>/<att>/jobs/<job>.json`

```json
{ "name": "test", "status": "in_progress", "conclusion": null, "runs_on": ["ubuntu-latest"],
  "needs": ["build"], "environment": null,
  "holder": "ns-7f2c", "seq": 2, "lost": 1,
  "accepted": null, "copied_from": null,
  "started_at": "2026-10-30T17:02:14Z", "completed_at": null,
  "steps": [], "log": null }
```

1. `holder` + `seq`: the lease this job started under. Set by `IStart` from the lease.
2. `accepted`: `{"runner","seq"}` of the result that concluded it. Null for skipped, lost
   (`"runner lost"` failure), cancelled and copied jobs.
3. `copied_from`: the earlier attempt a successful job was copied from by a rerun.
4. `lost` counts sweeps. `steps` and `log` are filled at completion from the accepted result.

### 3.3 `claims/<key>`

The key names one trigger, and creating it is the exactly-once claim: the same commit
creates `runs/<id>`, `claims/<key>`, the jobs, and CASes `concurrency/<group>`. A second
replica creating the same key fails, so one trigger yields one run across replicas and
restarts.

| Trigger | Key (a path, `/` between parts) |
|---|---|
| push | `<wf>/push/<branch~>@<sha>` |
| pull_request | `<wf>/pull_request/<n>@<sha>` |
| schedule | `<wf>/schedule/<slot>` (slot = the due cron minute, UTC) |
| workflow_dispatch | `<wf>/dispatch/<request-uuid>` |

## 4. Tier 1: `_checks`

| Path | Mode | Meaning |
|---|---|---|
| `<sha>/ci.<job>.json` | CAS | one check per job of the run's current attempt at `head_sha` |

```json
{ "status": "completed", "conclusion": "success", "state": "success",
  "workflow": "ci", "job": "test", "run": 4182, "attempt": 1,
  "url": "/actions/runs/4182", "log_tail": "test result: ok. 214 passed" }
```

1. **Projected from the accepted job in `_actions`, never from `results/`.** A late success
   from a lost runner cannot become a check.
2. It is a second branch written after `_actions`, so it can lag or be missed by a crash.
   **Reconcile repairs it**: every tick compares each completed job (current attempt) with
   its check and rewrites the differing ones.
3. `state` (`pending|success|failure`) is the legacy field `pail pr merge` reads from
   `_meta:checks/`; derived from `status`/`conclusion`. `pail pr merge` reads `_checks`
   first and falls back to `_meta:checks` during migration.
4. Check names are flat (`ci.test-linux-stable`); a rerun overwrites the check in place.

## 5. Tier 2: `<repo>.actions/`

All objects expire by bucket lifecycle rules except the ones noted.

| Path | Writer, mode | Meaning | Expiry |
|---|---|---|---|
| `queue/<label>/<run>.<att>.<job>.json` | orchestrator, create/delete | job message a runner gets (§5.1) | removed when the job leaves `queued` |
| `leases/<run>/<att>/<job>` | runner, create / gen-match | the lease (§5.2) | 30 d |
| `cancel/<run>/<att>/<job>` | orchestrator, create | runner sees it on refresh and stops | 7 d |
| `progress/<run>/<att>/<job>/<seq>.json` | lease holder, put | `{"holder","seq","steps":[…]}`; its existence with a step `in_progress` or later means "a step started under `seq`" | 7 d |
| `logs/<run>/<att>/<job>/<seq>/<n>.log` | lease holder, create | chunks (~64 KB or 2 s), then `full.log` | 30 d |
| `results/<run>/<att>/<job>/<seq>` | lease holder, create | the hand-off (§5.3) | 7 d |
| `artifacts/<run>/<name>/…` | lease holder, create | `upload-artifact`; same run only | 14 d |
| `cache/<scope>/<key>.tar.zst` | lease holder, create | scope = branch; a PR reads its scope then base, never writes base | 7 d since use |
| `runners/<id>.json` | runner, put | `{"labels","version","last_seen"}` for the Runners page | 1 d |
| `requests/<kind>/<uuid>.json` | CLI, create | `dispatch`, `rerun`, `cancel` requests (§6) | deleted by the orchestrator once acted on |
| `cursors/<replica>` | `pail events`, CAS | event cursor of one orchestrator replica | never |

`progress/` and `logs/` carry the lease seq in the path **(unmodelled; a decision, see open
questions)**: a lost runner that wakes up late then writes under its own seq and cannot
overwrite its successor's progress, and "a step started under seq N" stays well defined for
the sweep.

### 5.1 `queue/…`

```json
{ "run": 4182, "attempt": 1, "job": "test", "store": "gs://B/repo",
  "sha": "9f3ab21c5d0e47a1b8c63f20d9e4a7b15c2e8f30",
  "github": { "event_name": "push", "ref": "refs/heads/main", "repository": "repo", "run_id": 4182 },
  "steps": [ {"uses":"actions/checkout@v4"}, {"run":"cargo test"} ], "env": {"CI":"true"},
  "timeout_minutes": 30, "secrets": [ {"name":"NPM_TOKEN","ref":"gcpsm://proj/npm-token"} ] }
```

The orchestrator keeps the queue equal to the set of `queued` jobs: it creates an entry when
a job becomes `queued`, and deletes it when the job starts, is cancelled or skipped. Both are
idempotent and may lag. An entry can outlive the job's start, so a lease can be taken over
after the job file named the first holder (see §7.3).

### 5.2 `leases/<run>/<att>/<job>`

```json
{ "holder": "ns-7f2c", "expires_at": "2026-10-30T17:03:44Z", "seq": 2 }
```

1. **Claim** is create (generation 0) or, if the lease has expired, **takeover by generation
   match** with `seq = old seq + 1`. One writer wins. A lease is **never deleted and
   recreated** (that would reuse seq 1).
2. **`seq` counts claims, not refreshes.** Refresh is a generation match that keeps
   `holder` and `seq` and moves `expires_at`; if it fails, another runner holds the lease and
   this one stops (its late result is ignored anyway).
3. Defaults (unmodelled): refresh every 15 s, TTL 90 s.
4. A runner claims only an entry whose lease is absent or expired.

### 5.3 `results/<run>/<att>/<job>/<seq>`

```json
{ "runner": "ns-7f2c", "seq": 2, "conclusion": "success",
  "outputs": {}, "steps": [ {"number":1,"name":"Checkout","conclusion":"success"} ],
  "log": "logs/4182/1/test/2/full.log", "finished_at": "2026-10-30T17:05:19Z" }
```

1. `conclusion` is `success`, `failure`, or `cancelled` (the runner saw its cancel marker).
2. The **path includes the lease seq**: one create-only path per job would let a lost
   runner's late result block the next holder's, and the sweep (which looks for "a result")
   would leave the job `in_progress` forever.
3. A result is a *claim*, not a fact. It becomes one only through §7.3.

## 6. The CLI

Writes only what its caller's identity allows; none of it needs `_actions` write access
except approvals and admin settings.

| Command | Writes |
|---|---|
| `pail workflow run <wf> --ref R -f k=v` | `requests/dispatch/<uuid>.json` `{workflow, ref, inputs, actor}` |
| `pail run rerun <id> [--failed]` | `requests/rerun/<uuid>.json` `{run, failed_only, actor}` |
| `pail run cancel <id>` | `requests/cancel/<uuid>.json` `{run, actor}` |
| `pail run approve <id> --env E` | `_actions:runs/<id>/<att>/approvals/<E>/<who>.json`, create (Tier 1) |
| `pail secret/variable set` | `_actions:secrets/…`, `vars/…` (Tier 1, admin) |
| `pail workflow enable/disable` | `_actions:workflows/<file>.json` (Tier 1, admin) |
| `pail run list/view`, `pail pr merge` | read `_actions`, `_checks` |

The orchestrator validates a request (inputs against `on.workflow_dispatch.inputs`, actor
against policy), acts, and deletes it. A dispatch request's uuid is its claim key (§3.3), so
a crash between "run created" and "request deleted" cannot fire it twice **(request kinds
are unmodelled)**.

## 7. The orchestrator

`pail actions serve` is **stateless**: everything it knows it re-reads. Any number of
replicas are safe; the leader lease (the existing `locks/` protocol) is an optimisation
that saves wasted retries and is never needed for correctness.

### 7.1 Wake-ups

```
pail events --branch '*' --prefix actions/ --cursor bucket:actions/cursors/<replica> \
            --safety-poll 10  [--gcs-subscription …]
   ref changes (_actions, _checks, pr/*, main)  +  object changes in <repo>.actions/
        │ one JSON line each ─▶ wake flag   (a tick answers all pending wake-ups at once)
        ▼
   tick = read Tier 1 + Tier 2 ─▶ decide every due intent ─▶ batch into commit(s) ─▶ land
          conflict ─▶ tick again from a fresh read
```

1. Notifications are hints (`docs/extending.md`): they may be lost, repeated or reordered.
   A wake is a flag, so repeats and reorders change nothing; **a lost event costs latency,
   never correctness.**
2. **Time announces nothing.** A lease that lapses writes no object, so only the safety poll
   (and the start-up diff against the cursor) finds it. The detection latency of a dead
   runner is `TTL + safety-poll`; the poll must stay on.
3. Each replica has its own cursor: a bucket cursor is CAS'd, one consumer per cursor.

### 7.2 Intents and their guards

Every tick decides these from a read; a real commit batches several. **A commit guards every
record it read, not only those it writes**: written records that existed are `--expect`ed,
written records that did not are `--if-absent`, and records read but not written are
`--expect`ed too. A conflict means: re-read, decide again.

| Intent | Does | Writes | Also guards (read, not written) |
|---|---|---|---|
| trigger | new run from a key | `runs/<id>` create, `claims/<key>` create, jobs create, `concurrency/<g>` CAS; cancels the group's previous run if still going (`cancel: true`, its not-started jobs `completed/cancelled`) | previous holder's run; every job of the run it cancels |
| promote | pending job whose `needs` all succeeded becomes `queued` (or `waiting` if `environment` lacks an approval from someone other than the actor); one whose need did not succeed becomes `completed/skipped` | the job | |
| start | `queued` job whose lease a runner holds becomes `in_progress` | job: `holder`, `seq` from the lease | |
| accept | a result concludes the job (§7.3) | the job | |
| sweep | expired lease, no result under the job's `seq` (§7.4) | the job | the run |
| finish | all jobs of the current attempt `completed` | the run | |
| rerun | new attempt (§7.5) | run, jobs, `concurrency/<g>` | previous holder's run; its jobs if cancelling |
| check | project a completed job to `_checks` | `_checks:<sha>/ci.<job>.json` | the run |
| sync (Tier 2) | keep `queue/` equal to the queued jobs; create `cancel/…` for `in_progress` jobs of runs with `cancel: true` | objects | |

`concurrency` with `cancel-in-progress`: a trigger or a rerun makes its run the newest of the
group and cancels the previous holder. The CAS on `concurrency/<g>` means two replicas cannot
both win the group.

### 7.3 Result intake (fenced)

A result at `results/<run>/<att>/<job>/<seq>` is accepted iff, in the **job file**:
`status == in_progress`, `holder == result.runner`, and `seq == result.seq`. The live lease
is **not** consulted. (A lease can be taken over after the job file named the first holder,
because the queue entry outlives the start; also comparing the live seq would then strand
the job.) The accept commit CASes the job file, so a second accept of a finished job
conflicts. A late result from a lost runner names a holder/seq the job no longer has, and is
ignored.

### 7.4 Sweep

For an `in_progress` job whose lease is not live (expired or taken over) and with no result
at `results/…/<job.seq>`:

1. run has `cancel: true`: job `completed/cancelled`. **Never requeued.**
2. a step started under `job.seq` (its progress file): `completed/failure`, `lost + 1`
   ("runner lost").
3. else: back to `queued`, `holder` cleared, `lost + 1`; the next tick re-creates the queue
   entry.

The commit guards the run, so a cancel that landed since the read is seen.

### 7.5 Rerun

Allowed on a completed run that did not succeed. A rerun is a **new attempt**: `run_attempt`
+ 1, `status` reset, `cancel: false`; successful jobs are copied (`copied_from`), the rest
start fresh; the old attempt is frozen. It is a **new commit into the concurrency group**
(same rules as a trigger), so it cancels a newer active run of the group. Its `_checks`
overwrite the old attempt's.

### 7.6 Retention

A daily sweep removes runs older than 90 days with `put --delete`, 500 paths per commit;
`pail gc` reclaims the packs. Tier 2 expires by lifecycle rules (§5). **(unmodelled)**

## 8. Runners and scalers

**Runner** (act-engine based), per job:

```
list queue/<label>/ ─▶ read entry ─▶ claim lease (create | gen-match takeover) ─▶ run steps
   every 15 s: refresh lease (gen-match), overwrite progress/…/<seq>.json, upload log chunks,
               look for cancel/…/<job>
   done: create results/<run>/<att>/<job>/<seq>; stop
```

1. A failed refresh means another runner holds the job: stop writing.
2. On a cancel marker, stop steps and write a `cancelled` result.
3. Reports only through Tier 2 (`results/`, `progress/`, `logs/`). Masks secrets before
   upload. Never reads or writes Tier 1; clones the repo at `sha`.
4. Trusted runners resolve `secrets[].ref` with their own cloud identity; an untrusted-tier
   job gets none. No secret value is ever written to the bucket.

**Scaler** is a pluggable interface: *given the queue, start one ephemeral runner per queued
job whose labels it serves; tear it down*. It has read-only access to `queue/` and no
knowledge of workflows.

| Provider | Status |
|---|---|
| local docker | first |
| static (fixed runners on your hosts) | first |
| k8s (a Job per queued entry) | planned |
| Namespace | later, and only with explicit OK (billable instances) |

Permissions: orchestrator `role:actions` (all of both tiers); runner a per-job downscoped
token on its own `leases|progress|logs|results|artifacts|cache` objects and `queue/<labels>`;
scaler read on `queue/`; CLI create on `requests/`; viewers read.

## 9. Write budget

1. `_actions` takes **at most about 1 commit/s**, however many jobs change: a tick gathers
   every due intent into one multi-file commit. `_checks` is a separate ref with its own
   budget.
2. Heartbeats, progress and logs are Tier 2, one object per runner, so throughput scales
   with the number of runners.
3. 1,000-job matrix: one commit to create the run, then at most one per second.
4. Unbatched (rejected): about 3 commits per job state change.

| Load | Tier 1 commits/s | Tier 2 writes/s |
|---|---|---|
| quiet, 1 run/min | 0.05 | ~1 |
| 30 concurrent jobs | ≤ 1 | ~30 |
| 1,000-job matrix start | 1, then ≤ 1 | ~1,000 over ~1,000 objects |

## 10. Properties (checked in `verify/actions`)

| Property | Kept by |
|---|---|
| `atMostOneAccepted`, `acceptedWasHolder`: one result accepted per (run, attempt, job), and it is the holder's | §7.3 fence on job `holder`/`seq`; CAS on the job file; §5.2 create-only lease |
| `leaseExclusive`: no two runners hold a job under one seq | §5.2 create / generation-match claim; seq counts claims |
| `noStranded`: no job stays `in_progress` after its lease lapses | §7.4 sweep; §5.3 result path with seq; §7.3 job-file fence; §7.1 safety poll |
| `finality`, `attemptsAreFrozen`: a conclusion never changes; a rerun is a new attempt | CAS on job and run files; §7.5 |
| `ordering`: no job starts before its `needs` succeeded; a failed need skips it | §7.2 promote |
| `checksEarned`: a `_checks` success names an accepted success at that sha | §4 projection from jobs; reconcile |
| `oneActivePerGroup`, `cancelledIsIdle` | §7.2 trigger/rerun guards (every record read); §7.4 rule 1 |
| `gatesHold`: an environment job is queued only with an approval from someone other than the actor | §7.2 promote; per-person approval files |
| `oneRunPerKey`, `oneCreationPerKey`: one run per trigger, across replicas | §3.3 `claims/<key>` create in the run's commit |

## 11. Open questions (the model does not decide these)

1. Progress and log paths carrying the lease seq, and the progress file as the "step
   started" signal for the sweep.
2. `claims/<key>` key format, and dispatch/rerun/cancel via `requests/` with the request
   uuid as the claim key.
3. `_checks` carrying both GitHub fields and the legacy `state`.
4. One cursor per replica, in the area (`cursors/<replica>`), since a bucket cursor admits
   one consumer.
5. Lease TTL 90 s, refresh 15 s, orchestrator `--safety-poll 10`.
6. A runner that claimed a lease on a job cancelled before `start` gets no cancel marker
   (markers are made for `in_progress` jobs); its result is ignored. Acceptable, or mark
   leased cancelled jobs too?
7. Batching several intents in one commit, `run_number`, `idx/`, secrets, retention and
   leader election are unmodelled; their guards follow the same rules as §7.2.
