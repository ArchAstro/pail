# Extending pail: conventions plus events

Pail has no server to host features, and does not need one. A repository is a bucket
prefix whose only mutable objects are refs, and every change to a ref is announced by the
object store itself. Features are programs that agree on names:

```
writes   pail put <branch> --file PATH=SRC     atomic commit to any branch, no working copy
         pail land <branch> --stack            every commit of a branch, one atomic swap
         pail rebase --plan FILE --json        rewrite a branch from a JSON plan, one atomic swap
reads    pail cat <rev>:<path>, pail ls -r <rev>:<dir>, pail log, pail rev-parse
events   pail events [--branch GLOB] [--prefix AREA/P] [--cursor C] [--gcs-subscription SUB | --sqs-queue URL | --azure-queue URL | --r2-queue ACCT/Q]
         pail events setup --consumer NAME   (prints the provider commands that wire the notifications)
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

Wire the bucket once per consumer. `pail events setup --consumer ci` prints the commands for
this store (the provider comes from the store's URL; `--provider` overrides), then how to
consume the queue it made. It only prints. `--apply` runs them after a y/N question (`--yes`
skips it) and creates billable cloud resources in your account; it refuses while a command
still holds a placeholder (`PROJECT`, `REGION`, ...).

```sh
pail --store gs://BUCKET/REPO events setup --consumer ci --project P
# gcloud pubsub topics create pail-ci
# gcloud storage buckets notifications create gs://BUCKET --topic=pail-ci --object-prefix=REPO/refs/ …
# gcloud storage buckets notifications create gs://BUCKET --topic=pail-ci --object-prefix=REPO.actions/ …
# gcloud pubsub subscriptions create pail-ci --topic=pail-ci
pail --store gs://BUCKET/REPO events --gcs-subscription projects/P/subscriptions/pail-ci
```

Each consumer gets its own queue or subscription (`pail-NAME`): two consumers on one queue
split its messages instead of each seeing all of them.

| Provider | Store | Notification path | `setup` runs | Consume with | Latency |
|---|---|---|---|---|---|
| GCS | `gs://B/REPO` | bucket notification → Pub/Sub topic → subscription | `gcloud` | `--gcs-subscription projects/P/subscriptions/S` | 1.2–1.6 s measured (below) |
| S3 | `s3://B/REPO` | S3 event notification (or EventBridge) → SQS | `aws` (`sqs`, `s3api`) | `--sqs-queue URL` | unmeasured (SQS long poll returns on arrival) |
| Azure Blob | `https://ACCT.blob.core.windows.net/C/REPO` | Event Grid system topic → Storage Queue | `az` (`storage`, `eventgrid`) | `--azure-queue URL` | unmeasured; the Storage Queue API cannot long-poll, so add up to 2 s of polling |
| Cloudflare R2 | `s3://B/REPO` with `AWS_ENDPOINT` at R2 | R2 event notification → Queue (HTTP pull consumer) | `wrangler` | `--r2-queue ACCOUNT/QUEUE` | unmeasured; pull is polled every 2 s when idle |

Instead of a flag, a source is one line of text: `gcs:SUBSCRIPTION`, `sqs:URL`, `azure:URL`,
`r2:ACCOUNT/QUEUE` or `poll`. Extensions then never need to know a queue's name, only to run
`pail events`. The order is the flag, then `PAIL_EVENTS_SOURCE`, then the config (the most
specific store section wins, as for `credential`), then polling:

```sh
export PAIL_EVENTS_SOURCE=sqs:https://sqs.eu-west-1.amazonaws.com/111122223333/pail-ci
pail config 'events.s3://acme/app.source' 'sqs:https://sqs.eu-west-1.amazonaws.com/111122223333/pail-ci'
```

How each queue is read, and with which credentials:

1. **S3 / SQS.** Long-polls `ReceiveMessage` (20 s), deletes with `DeleteMessageBatch`, both
   signed with SigV4. Reads S3 event notifications (`{"Records":[…]}`, keys URL-encoded),
   EventBridge events (`Object Created` / `Object Deleted`), and either wrapped by SNS.
   Credentials: the store's pail credential provider if it has one, else
   `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`, else the `aws` CLI's
   (`aws configure export-credentials`: profiles, SSO, roles). The consumer needs
   `sqs:ReceiveMessage`, `sqs:DeleteMessage` and `sqs:ChangeMessageVisibility`.
2. **Azure / Storage Queue.** Gets messages and deletes them with their pop receipt. Reads
   Event Grid `Microsoft.Storage.BlobCreated` / `BlobDeleted` (Event Grid or CloudEvents
   schema, JSON or base64); a rename scans. Authorized by a SAS token in the queue URL
   (`rp` permissions), else by a bearer token from a credential provider.
3. **R2 / Queues.** Pulls and acknowledges through the Cloudflare API
   (`/accounts/A/queues/ID/messages/pull` and `/ack`; a queue name is looked up to its id).
   Reads `PutObject`, `CopyObject`, `CompleteMultipartUpload`, `DeleteObject` and
   `LifecycleDeletion`. Authorized by `CLOUDFLARE_API_TOKEN` (Queues read and write), else by a
   credential provider's token.

The Azure and R2 sources are tested against fakes of the documented HTTP APIs, not the
services. MinIO is not supported yet (follow-up: a webhook listener, or its NATS target).

A message is deleted only after the events it caused are written and the cursor is saved, so
a crash repeats a message and never loses one. A message that does not parse is logged
(`pail events: sqs: unparseable message (…); scanning instead`) and counts as "something
changed": everything is re-listed. A message for another bucket, or a key outside the
repository, is deleted and ignored.

`pail events` treats each notification as a hint: it names a key, and the key is re-read.
A line is written only if the key differs from the last one written for it, so duplicated and
reordered notifications cost a read, and nothing in a notification's payload is trusted.
Without notifications (`file://`, emulators) it re-lists every `--interval-ms` with the same
output. Because a notification is only a nudge, a source needs to produce nothing more than
"something under this prefix changed"; that is also why all four sources share one model
(`verify/README.md`).

The guarantees, which `verify/events/events.qnt` states and `tests/events_refinement.rs`
checks the code against:

1. **A dropped hint costs latency, never correctness.** With notifications, everything is
   also re-listed every `--safety-poll` seconds (default 60; `0` turns it off).
2. **At-least-once.** A crash may repeat a line, never skip one. Consumers key work by commit
   (refs) or by key and version (objects).
3. **`--cursor` makes a restart lossless.** The cursor (a file, or `bucket:AREA/KEY`) holds
   what was emitted. On start, every key that differs (deletions too) is written with
   `"source":"resume"`, then it streams. Per key the cursor records the intent to emit, the
   line is written, and the cursor takes the new state, so a key that returned to the state
   the cursor holds (a ref reset, an object deleted again) is still reported. A bucket
   cursor is compare-and-swapped: a second consumer on it fails instead of overwriting.
   The first run with a new cursor emits everything that exists. Without `--cursor` a
   restart takes the present state as already seen.
4. **Order.** For one key, versions are emitted in the order the store had them.

Objects beside the repository are events too: the Actions orchestrator's `<repo>.actions/`
area (queue entries, leases, results) is the sibling `gs://BUCKET/REPO.actions/`.

`setup` wires `REPO.actions/` as well as `REPO/refs/`.

```sh
pail --store gs://BUCKET/REPO events --branch '*' --prefix actions/ \
    --cursor bucket:actions/cursors/orchestrator --gcs-subscription projects/P/subscriptions/pail-ci
# {"key":"queue/ubuntu-latest/4182.1.test.json","version":"1792…","old":null,"op":"put","source":"gcs","at":…}
```

`--prefix AREA/PREFIX` is `<repo>.AREA/PREFIX`. The cursor lives in the area too, and is not
reported as an event.

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
