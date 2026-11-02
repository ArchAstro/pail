# T0.4 spike: act as the runner base

Question (plan D1/D2): fork Gitea's act_runner, or use act as a library? Checked against
`github.com/nektos/act` v0.2.94 (2026-10-31) and `gitea.com/gitea/runner` @ `6e42621`
(2026-11-02), cloned and read for this note.

## Recommendation: library use of nektos/act, not a fork of the Gitea runner

1. Use `github.com/nektos/act` (pinned tag) for both the orchestrator's workflow model and the
   runner's job execution. Write the runner daemon ourselves (queue/lease, logs, results).
2. Do not fork `gitea.com/gitea/runner`. D1's premise is out of date: the repository is now
   `gitea/runner` (was `act_runner`), act is **vendored inside it** (`act/`, 20.5k LOC, Gitea
   copyright, diverged from nektos), the model moved to `gitea.dev/actionslib`, and it needs Go 1.27.
   A fork means owning ~29k LOC to reuse mostly the same act code we can import.
3. The checkout swap needs no fork: the spike does it by rewriting the parsed model before
   the run. If we ever need to patch act internals, `replace` nektos/act with a thin fork of
   act alone, not of the runner.
4. Revisit if: we need a hook act lacks (see Risks), or Gitea's vendored act gains something we
   need (it has a `builtin:` step hook, below). Switching is cheap: both expose a `Config`/`NewPlanExecutor` shape.

## What was built (all under `actions/`)

| Piece | Where | State |
|---|---|---|
| `pail-actions plan --event --ref [--paths --cron --type] <file or dir>` | `cmd/pail-actions`, `internal/plan` | JSON plan; 27 corpus fixtures (75 scenarios) pass |
| `pail-actions run-local` | `internal/runlocal` | runs one job via act; `actions/checkout` becomes `pail clone --at <commit> <store> "$GITHUB_WORKSPACE"` |
| Corpus + fixtures + feature matrix | `compat/` | see `compat/README.md` |
| Tests | `go test ./...` | 33 s cold, 3 s warm locally; host-mode run test skips without a pail binary |

Verified locally: host mode (act `-self-hosted`) against a `file://` store, and Docker mode
(`debian:stable-slim`, linux/arm64 pail built in `rust:1-bookworm`, mounted at
`/usr/local/bin/pail`, store dir mounted read-only): the step printed the committed file.
Docker test: `PAIL_LINUX_BIN=<linux pail> go test ./internal/runlocal -run Docker`.

## act as is (used by the spike)

| Area | What it gave us | Size (nektos v0.2.94, non-test) |
|---|---|---|
| `pkg/model` | workflow + schema validation (`pkg/schema`), jobs, steps, `needs`, services, container, outputs, matrix include/exclude/cross product, fail-fast, max-parallel, job type (local/remote reusable), `workflow_dispatch`/`workflow_call` config | 1.6k + 0.4k |
| `pkg/exprparser` | the `${{ }}` engine (actionlint parser); evaluated `if:` at plan time | 1.0k |
| `pkg/workflowpattern` | GitHub glob patterns for branches/paths with `!` | 0.2k |
| `pkg/runner` | executes a plan: docker or host, services, containers, JS/composite/Docker actions, outputs, env files | 5.9k |
| `pkg/container` | Docker plumbing | 3.9k |
| `pkg/artifacts`, `pkg/artifactcache` | artifact server (v4) and cache server, in-process | 2.2k + 0.8k |
| Seams | `JobLoggerFactory` (log capture), `ActionCache` interface (action source), `Config.Platforms` (label to image or host), `ContainerOptions` (mounts) | |

## act did not do (we added, or must)

1. **Trigger filters.** `Workflow.On()` returns event names only. branches, tags, paths, `-ignore`, `types`, cron: ours (`internal/plan/trigger.go`, ~170 LOC, on `workflowpattern`).
2. **Dropped keys.** `Job` has no `environment`, `concurrency`, job-level `continue-on-error`, `permissions`. We re-read the YAML (`extras`).
3. **Dynamic matrix.** `matrix: ${{ fromJSON(..) }}` is silently one job; with the expression under a key, act calls `log.Fatal` and kills the process. We detect expressions first. Real expansion needs the needs outputs, so the orchestrator expands it at run time.
4. **Planner.** `PlanEvent` ignores filters, orders stages by map iteration (random), reports unknown `needs` as "unable to build dependency graph" without a name. We wrote `stages()` (~35 LOC).
5. **Defaults filled in.** name = job id, `if` = `success()`; we read the raw YAML to tell "absent". `runs-on` is not required (a job without it passes the schema).
6. **Matrix semantics.** Uses GitHub's include/exclude rules closely; an `exclude` key outside the matrix is an error (GitHub's behaviour is not fully checked here). Instance naming is ours.
7. **Git assumptions.** act reads `github.sha/ref` from a local git repo and logs errors in a pail workspace. Set the github context from the queue item instead (not done in the spike; the log filter hides the noise).
8. **Checkout.** By default act *skips* `actions/checkout` and uses the host directory (`NoSkipCheckout`). Gitea's runner added `builtin:checkout` (git-based, 472 LOC) through a `builtin:` step factory.
9. **Remote actions** are cloned from github.com with git at run time: needs egress and git on the runner.
10. **Image parity.** act's default images are slim; `ubuntu-latest` tools differ from GitHub's.

## What must change for pail (runner side)

| Gitea runner part | LOC | In pail |
|---|---|---|
| connect-rpc client: `Register`, `FetchTask`, `UpdateTask`, `UpdateLog` (`internal/pkg/client`, `app/poll`, `app/cmd/register`) | ~1.6k | **replace**: queue/lease protocol over the bucket (claim by CAS, heartbeat to keep the lease, release on exit) |
| `report/reporter.go`, `joblog.go` (state + log rows to Gitea) | ~1.1k | **replace**: job result and step outcomes to Tier 2; logs as Tier 2 chunks, fed from act's `JobLoggerFactory` |
| `app/run/runner.go` (task to act `Config`, env, secrets, labels) | ~1.0k | **rewrite**: queue item gives workflow, job, commit, event; secrets from refs (D5) |
| config, labels, telemetry/metrics, proxy, health, process tree-kill | ~2.7k | keep ideas: labels, tree-kill; drop the rest |
| checkout | 472 | **swap** to `pail clone --at <commit>`; with library use it is a model rewrite (`SwapCheckout`, 20 LOC); inputs (`path`, `ref`, `fetch-depth`) are ignored today |
| act itself | 20.5k vendored / ~17k nektos (model, schema, exprparser, runner, container, artifacts, cache, common) | library, unmodified |

New code we write either way: runner daemon around act (estimate 1.5k to 2.5k Go), orchestrator
(plan, trigger evaluation, expansion, leader lease; P1), artifact/cache services over Tier 2
(act's servers take a storage path; Tier 2 backing is ours).

## Licence

Confirmed from the files: nektos/act `LICENSE` is MIT (2019); gitea/runner, its vendored `act/`
and gitea/actionslib are MIT ("The Gitea Authors", 2022). Keep their notices in anything copied.
Not covered by MIT: Docker images pulled at run time and the actions we execute (each has its own licence).

## Risks

1. **log.Fatal in act's model** on some invalid input: the orchestrator must not parse untrusted workflows in a process it cannot lose; run the parse behind recover/subprocess, or pre-validate with our own checks.
2. **Upstream drift**: Gitea moved off nektos; nektos/act releases are frequent (94 patch releases) and the model changes between them. Pin, and keep `act_gaps_test.go` which fails when behaviour we work around changes.
3. **Go toolchain**: nektos v0.2.94 needs Go 1.26.10 (go.mod says so); Gitea's runner needs 1.27.
4. **Trust**: untrusted PR workflows (D7) run in act's containers; act is not a security boundary. Isolation must come from the provider (VM/Cloud Run), not act.
5. **Compat ceiling**: API-calling actions, OIDC, macOS/Windows, services without Docker: see `compat/README.md`.
6. **Remote action fetch** from github.com: availability, rate limits, supply chain. Plan an `ActionCache` backed by a pinned mirror.
7. **Expression coverage**: plan-time `if` evaluation is deliberately narrow (event name, ref, matrix); anything else is `deferred`.
8. **Docker access in tests**: Docker-mode run needs a linux pail; CI runs neither.

## CI

`ci.yml` gains an `actions` job: gofmt, `go vet`, `go test ./...` (cold ~30 s of compile on a laptop; CI expected 1 to 2 min). No docker, no pail binary needed.
