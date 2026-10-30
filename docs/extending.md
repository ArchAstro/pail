# Extending pail: conventions plus events

Pail has no server to host features, and does not need one. A repository is a bucket
prefix whose only mutable objects are refs, and every change to a ref is announced by the
object store itself. Features are programs that agree on names:

```
writes   pail put <branch> --file PATH=SRC     atomic commit to any branch, no working copy
         pail land <branch> --stack            every commit of a branch, one atomic swap
         pail rebase --plan FILE --json        rewrite a branch from a JSON plan, one atomic swap
reads    pail cat <rev>:<path>, pail ls -r <rev>:<dir>, pail log, pail rev-parse
events   pail events [--branch GLOB] [--gcs-subscription SUB]
         one JSON line per ref change: {"branch","commit","old","source","at"}
```

## Conventions

| Name | Meaning |
|---|---|
| `pr/<anything>` | a branch proposed for review (convention only; any branch can be a PR head) |
| `_meta` | a branch holding records, not code: PRs, reviews, check results. Writes are commits, so they are atomic, versioned and conflict-checked |
| `_meta:prs/<n>.json` | a pull request: `id, title, body, head, base, author, state` (`open`, `closed`, `merged`), `draft`, `created`, `updated`, `closed_at`, `closed_by`, `merged_at`, `merged_by`, `merged_commit`. Times are epoch seconds; everything after `state` is optional, so older records stay valid |
| `_meta:prs/<n>/reviews/<who>.json` | a review: `{"verdict": "approve"\|"changes", "comment", "at", "who"}` (`who` is the display name, optional); one per person, a new review replaces theirs |
| `_meta:prs/<n>/comments/<ms>-<who>.json` | a comment: `{"who", "body", "at"}`. `<ms>` is epoch milliseconds, zero-padded to 13 digits, so names sort in time order; claimed with `--if-absent` (on a clash, try `ms+1`) |
| `_meta:checks/<commit>/<check>.json` | a check result for a commit: pending, success or failure, with a log tail |
| `<branch>:pail.ci.json` | the checks a commit declares: `{"checks": {"test": "cargo test"}, "required": ["test"]}` |
| `<base>:pail.pr.json` | merge policy for a base branch: `{"approvals": 1, "required_checks": [...]}` |
| `<who>` | a person as a file name: `[A-Za-z0-9._-]`, every other run of characters becomes `-` (`Alice Smith` is `Alice-Smith`). The record keeps the display name (`author`, `who`) |
| `_*` branches | written by tools; never built by CI (so recording a result cannot trigger a run) |

Because records are commits:

1. **Ids are claimed atomically.** `pail put _meta --file prs/12.json=… --if-absent` fails
   with exit 4 if another writer claimed 12 first; the PR tool tries 13.
2. **History is free.** `pail log _meta -- prs/12.json` is the audit log of PR 12.
3. **Results are keyed by commit.** Events are at-least-once and unordered; a CI runner that
   sees a commit twice finds its results already recorded and skips it.

## Events from the bucket

Wire a GCS bucket once (per repository prefix):

```sh
gcloud pubsub topics create pail-refs
gcloud storage buckets notifications create gs://BUCKET --topic=pail-refs \
    --object-prefix=REPO/refs/ --event-types=OBJECT_FINALIZE,OBJECT_DELETE
gcloud pubsub subscriptions create pail-ci --topic=pail-refs
pail --store gs://BUCKET/REPO events --gcs-subscription projects/P/subscriptions/pail-ci
```

`pail events` treats each notification as a nudge: it re-reads the ref and emits only real
changes, once. Without notifications (`file://`, emulators) it polls with the same output.
S3 (EventBridge/SQS), Azure (Event Grid) and R2 (Queues) offer the same prefix-filtered
create events; adding them is a new source in `src/events.rs`.

Measured on GCS (2026-10-20, residential client): a ref event arrived 1.2–1.6 s after the
commit started (the commit itself ~0.5 s).

## Plugins

`pail <name> args…` runs `pail-<name>` from `PATH` when `<name>` is not a built-in (as `git`
and `cargo` do; built-ins always win). The plugin's environment has the store pail resolved
(`PAIL_STORE`: from `--store`, `PAIL_STORE`, or the working copy you are in) and the path of
the pail binary (`PAIL_BIN`); its exit code is pail's. `pail help` and `pail --list` show
the plugins found. A plugin is any executable that only calls the `pail` CLI:

```sh
install -m755 extensions/pail-pr ~/.local/bin/pail-pr     # any directory on PATH
install -m755 extensions/pail-ci ~/.local/bin/pail-ci
pail pr --help
```

## Two plugins

Both are short, dependency-free Python programs that know nothing of pail's internals: they
only call the `pail` CLI and follow the conventions above.

- **`extensions/pail-pr`** (`pail pr`): `create` (alias `open`), `list`, `view` (`show`),
  `diff`, `checkout`, `review`, `comment`, `merge`, `close`, `reopen`, `edit`, `ready`,
  `watch`; `--json` on `create`, `list`, `view`, `merge`, `watch`. Merge checks the base
  branch's policy (approvals of the head commit from someone other than the author, required
  checks passing on it), refuses drafts and closed PRs (even
  with `--force`), then claims the PR (state `merging`, which `edit`, `close`, `ready` and
  `reopen` refuse) and lands exactly the commit it judged (`pail land --stack --expect-tip`).
  `comment` takes `--path/--line/--side/--commit` for a line of the diff and `--reply-to` for a
  thread; `view` groups threads and marks outdated ones. `pail-web` (with `actions: true`)
  writes the same records, so a PR made in either shows up live in the other. Every change to
  `prs/<n>.json` is a compare-and-swap (`pail put --expect`): it re-reads and decides again
  when the record changed, so no edit overwrites one it did not see. The protocol is modelled
  in `verify/pr.qnt` and the plugin is checked against it (`verify/README.md`).
- **`extensions/pail-ci`**: `run` follows `pail events`, checks out the exact commit of each
  event (`pail clone --at`), runs the checks `pail.ci.json` declares and records each result
  on `_meta`; `status <rev>` shows them.

```
dev:  pail switch -c pr/hello; …; pail commit -m "say hello"     ─┐
                                                                  │ refs/pr/hello written
bucket notification ──▶ Pub/Sub ──▶ pail events ──▶ pail-ci ◀─────┘
pail-ci: clone --at <commit>, run "test", pail put _meta checks/<commit>/test.json
dev:  pail pr create --title "Say hello"             (_meta:prs/1.json, --if-absent)
bob:  pail pr review 1 --approve                     (_meta:prs/1/reviews/bob.json)
dev:  pail pr merge 1    → policy ok → pail land pr/hello --stack --onto main
```

`tests/cli.rs` (`pr_and_ci_extensions`, `pr_plugin_commands`) runs this flow end to end on a local store; the same
flow ran on GCS driven by bucket notifications.
