# Actions compatibility corpus

Workflow files that look like real ones, the features pail Actions promises, and what
`pail-actions plan` makes of them. Plan D0: workflow syntax is GitHub's, read verbatim from
`.github/workflows`.

```
compat/
  workflows/   22 workflows written for this corpus (feature files, plus five modelled on popular OSS CI)
  pail/        snapshot of pail's own .github/workflows (refresh: cp .github/workflows/*.yml actions/compat/pail/)
  fixtures/    one JSON per workflow: scenarios (event, ref, changed paths) and the expected plan
```

A fixture holds, per scenario: whether the event triggers the workflow (and why), jobs, the
needs graph (`stages`), matrix expansion (one entry per instance), and per-job `if`,
`environment`, `concurrency`, `timeout_minutes`, `continue_on_error`, services, outputs.
`go test ./...` (in `actions/`) plans every scenario and compares. After an intended change:
`go test ./internal/plan -update`, then review the diff: the fixtures are the specification.
Corpus files are original (modelled on, not copied from, public repositories).

## Status key

| Status | Meaning |
|---|---|
| supported | promised for v1; the plan side works today (T0.4) and the runner side is expected to work |
| planned | promised, needs product work named in the plan (phase in brackets) |
| unsupported | not promised; reason given |

"Plan" says what `pail-actions plan` does today. "act" says what act's model gave us for free.

## Triggers

| Feature | Status | Plan | act | Corpus |
|---|---|---|---|---|
| `push` branches / branches-ignore / tags / tags-ignore | supported | evaluated, incl. `!` negation and the branch-vs-tag rule | event name only | triggers-push-pr, triggers-branch-ignore, release-tag |
| `push` / `pull_request` paths / paths-ignore | supported | evaluated when changed files are given; not for tag pushes | no | triggers-push-pr, oss-node-monorepo, pail/site |
| `pull_request` branches (base) and `types` | supported | evaluated, default types applied | no | triggers-push-pr |
| `workflow_dispatch` with inputs | supported | triggers; input schema not validated yet | `WorkflowDispatchConfig()` parses inputs | triggers-dispatch |
| `schedule` (cron) | supported | matches the fired cron entry; no clock | names only | triggers-schedule |
| `workflow_call` | supported | triggers; local callee expanded under the caller job | yes | reusable-callee |
| `pull_request_target` | planned (D7 trust tiers) | evaluated like `pull_request` | no | triggers-branch-ignore, oss-triage-bot |
| `release` | planned (tag-based, after P1) | name and `types` matched | no | oss-python-lib |
| `issues`, `issue_comment`, `labeled` PR flows | unsupported (pail has no issue tracker; labels not modelled) | name and `types` matched | no | oss-triage-bot |
| `workflow_run`, `repository_dispatch`, `merge_group`, `deployment*` | planned (P4) | not exercised | no | none |

## Jobs and graph

| Feature | Status | Plan | act | Corpus |
|---|---|---|---|---|
| `needs` (list or string), stages, diamonds | supported | stages, deterministic order, unknown and cyclic needs rejected | stages built, order random, error unnamed | needs-graph |
| `if:` on jobs, static parts | supported | decided at plan time for `github.event_name`/`ref`/`ref_name` and `matrix.*`; the rest `deferred` | expression engine (`exprparser`) | contexts-env, needs-graph |
| `if:` with `needs.*`, `steps.*`, status functions, `github.event.*`, secrets | supported | `deferred` to run time (needs results) | engine works at run time | needs-graph, oss-rust-ci |
| matrix: product, `include`, `exclude` | supported | expanded, named like GitHub (`job (a, b)`) | `GetMatrixes()` | matrix-basic, matrix-include-only |
| matrix `fail-fast`, `max-parallel` | supported | reported per job | parsed | matrix-basic |
| matrix from `fromJSON(needs.*)` | planned (P1: expand when `needs` finish) | one placeholder instance, `matrix_dynamic` | **ignores it (one job), or exits the process** | matrix-dynamic |
| `outputs` and `needs.<job>.outputs` | supported | output names listed | yes | needs-graph, environments-approval |
| `concurrency` (workflow and job), `cancel-in-progress` | planned (P1 leases) | group and flag reported, expressions raw | dropped by the model | concurrency-cancel |
| `environment` with required reviewers | planned (P4 approval gate) | name reported; approval rules live outside the file | dropped by the model | environments-approval, oss-python-lib |
| `continue-on-error` (job, step, matrix expression) | supported | reported | step only | continue-on-error-timeouts, matrix-basic |
| `timeout-minutes` | supported | reported | parsed, enforced by runner | continue-on-error-timeouts |
| `permissions` | planned (token scopes need the REST shim, D4) | reported | dropped | environments-approval, oss-docker-publish |
| `defaults.run`, `env` | supported | not planned | yes | oss-rust-ci, contexts-env |
| `runs-on` labels | planned (labels to providers, D8) | listed | listed | all |

## Reuse

| Feature | Status | Plan | act | Corpus |
|---|---|---|---|---|
| reusable workflow, local (`./.github/workflows/x.yml`) | supported | callee planned as `children`; with matrix on the caller | job type detected; runner runs it | reusable-caller |
| reusable workflow, other repo (`owner/repo/...@ref`) | planned (P4, needs cross-repo pail clone) | not resolved | detected as remote | none |
| `secrets: inherit`, `with:`, `outputs` of `workflow_call` | supported | not evaluated | parsed | reusable-caller, reusable-callee |
| composite and JS actions from github.com | supported where the runner has egress; planned: mirror/pin | listed under `actions` | cloned by act at run time | oss-*, cache-artifacts |
| Docker actions, `docker://` steps | supported on Docker providers | listed | yes | oss-docker-publish |

## Runtime services

| Feature | Status | Plan | act | Corpus |
|---|---|---|---|---|
| `services:` containers, `container:` jobs | supported on Docker providers; unsupported on Cloud Run Jobs (no Docker) | listed | yes | services-postgres |
| `actions/checkout` | supported, replaced by `pail clone --at <commit>` (inputs other than the commit ignored) | n/a | skipped by default; swap works as a model rewrite (run-local) | all |
| `actions/cache` (+ `save`/`restore`) | planned (cache service backed by Tier 2) | listed | has a cache server (`artifactcache`) | cache-artifacts, oss-node-monorepo |
| `upload-artifact` / `download-artifact` v4 | planned (artifact service backed by Tier 2) | listed | has an artifact server (`artifacts`, v4) | cache-artifacts, release-tag |
| `GITHUB_*` env, `github.*`, `runner.*`, `job.*` contexts | supported; `github.event` payload planned (built from pail events) | contexts used in `if` only | yes | contexts-env |
| `GITHUB_ENV`, `GITHUB_PATH`, `GITHUB_OUTPUT`, step summary | supported | n/a | yes | contexts-env |
| `secrets.*`, `vars.*` | planned (D5: secret-manager refs) | n/a | passed in config | environments-approval |
| `hashFiles()` | supported | n/a | yes | cache-artifacts |

## Known gaps (the compatibility ceiling)

| Feature | Status | Why |
|---|---|---|
| Actions that call the GitHub API (`github-script`, `labeler`, `stale`, `gh`) | unsupported until the REST shim (D4, phase 4, read-only first) | pail has no API server; `GITHUB_TOKEN` has nothing to talk to |
| `GITHUB_TOKEN` / OIDC `id-token` (PyPI trusted publishing) | unsupported | no token issuer |
| `macos-*` and `windows-*` runners | unsupported on Docker providers | containers are Linux; needs bring-your-own hosts |
| `ubuntu-latest` image parity | planned | act images are smaller than GitHub's; pick the image per label |
| Marketplace actions needing node on the image | supported if the image has node | choose images accordingly |
| Required status checks, check runs, annotations, job summaries UI | planned (P4) | results land in Tier 2 first |
