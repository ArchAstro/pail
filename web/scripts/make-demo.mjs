#!/usr/bin/env node
// Builds a local file:// pail repository with the real `pail` binary: branches, a squash, a
// repack, a README, pull requests with reviews, and CI check results.
//   node scripts/make-demo.mjs [dir]      (default: web/.demo)
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { pailBin } from "./pail-bin.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const out = resolve(process.argv[2] ?? join(here, "..", ".demo"));
const bin = pailBin();
const store = `file://${join(out, "store")}`;
const work = mkdtempSync(join(tmpdir(), "pail-demo-"));
const wc = join(work, "wc");
const NOW = Math.floor(Date.now() / 1000);
let clock = NOW - 12 * 86400;

const people = {
  ada: "Ada Okafor <ada@example.com>",
  bo: "Bo Lindqvist <bo@example.com>",
  chen: "Chen Wei <chen@example.com>",
};

function pail(args, { cwd = wc, who = "ada", input, tick = 0 } = {}) {
  clock += tick * 4;
  return execFileSync(bin, args, {
    cwd, input, encoding: "utf8", stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, PAIL_AUTHOR: people[who], PAIL_DATE: `${clock} +0000`, PAIL_CONFIG_GLOBAL: join(work, "config"), NO_COLOR: "1" },
  });
}
const sh = (...a) => pail(a).trim();

function write(path, text) {
  const p = join(wc, path);
  mkdirSync(dirname(p), { recursive: true });
  writeFileSync(p, text);
}

function commit(msg, files, who = "ada", hours = 7) {
  for (const [p, t] of Object.entries(files)) write(p, t);
  pail(["add", "-A"]);
  pail(["commit", "-m", msg], { who, tick: hours * 3600 });
}

function meta(path, value, message, { ifAbsent = false } = {}) {
  const body = typeof value === "string" ? value : JSON.stringify(value, null, 1);
  pail(["put", "_meta", "-m", message, "--file", `${path}=-`, ...(ifAbsent ? ["--if-absent"] : [])], { input: body, tick: 60 });
}

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
pail(["init", store], { cwd: work });
pail(["clone", store, wc], { cwd: work });

// ---- main: a small project, built up over three weeks ---------------------------------------
commit("Start tidepool: a bucket-backed job queue", {
  "README.md": `# tidepool

A job queue that lives in a bucket. Workers claim jobs with a single atomic write; there is
no broker to run and nothing to keep warm.

## Why

- **No server.** Producers and workers share a bucket prefix and nothing else.
- **Exactly-once claims.** A claim is a create-only write; the loser retries the next job.
- **Inspectable.** Every job and every attempt is a plain JSON file you can \`cat\`.

## Quick start

\`\`\`ts
import { Queue } from "./web/queue";

const q = new Queue("gs://acme-jobs/emails");
await q.push({ kind: "welcome", to: "ada@example.com" });

for await (const job of q.claim("worker-1")) {
  await send(job.payload);
  await job.done();
}
\`\`\`

## Layout

| Path | What |
| --- | --- |
| \`src/\` | the Rust claim loop |
| \`web/\` | the TypeScript client |
| \`docs/\` | design notes |

> Jobs are retried with exponential backoff; see [docs/design.md](docs/design.md).
`,
  "Cargo.toml": `[package]\nname = "tidepool"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nserde = { version = "1", features = ["derive"] }\nserde_json = "1"\n`,
  "src/lib.rs": `pub mod claim;\npub mod job;\n`,
  "src/job.rs": `use serde::{Deserialize, Serialize};\n\n#[derive(Debug, Clone, Serialize, Deserialize)]\npub struct Job {\n    pub id: String,\n    pub kind: String,\n    pub payload: serde_json::Value,\n    pub attempts: u32,\n}\n\nimpl Job {\n    pub fn new(kind: &str, payload: serde_json::Value) -> Job {\n        Job { id: uuid(), kind: kind.into(), payload, attempts: 0 }\n    }\n}\n\nfn uuid() -> String {\n    format!("{:032x}", rand_u128())\n}\n\nfn rand_u128() -> u128 {\n    std::time::SystemTime::now()\n        .duration_since(std::time::UNIX_EPOCH)\n        .unwrap()\n        .as_nanos()\n}\n`,
  "src/claim.rs": `use crate::job::Job;\n\n/// Claim the oldest unclaimed job with a create-only write.\npub fn claim_next(jobs: &[Job], worker: &str) -> Option<Job> {\n    for job in jobs {\n        if try_create(&format!("claims/{}", job.id), worker) {\n            return Some(job.clone());\n        }\n    }\n    None\n}\n\nfn try_create(_key: &str, _owner: &str) -> bool {\n    true\n}\n`,
  "docs/design.md": `# Design\n\nA job is \`jobs/<id>.json\`. A claim is \`claims/<id>\`, written with if-absent.\n\n## Retries\n\nAttempts back off as \`min(2^n, 300)\` seconds.\n`,
  "logo.svg": `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><circle cx="32" cy="32" r="28" fill="#2b59c3"/><path d="M12 38c6-6 10 6 20 0s14 6 20 0" stroke="#fff" stroke-width="4" fill="none"/></svg>\n`,
}, "ada", 0);

commit("Add the TypeScript client", {
  "web/queue.ts": `export interface Job<T = unknown> {\n  id: string;\n  kind: string;\n  payload: T;\n  attempts: number;\n}\n\nexport class Queue {\n  constructor(private readonly prefix: string) {}\n\n  async push<T>(payload: T & { kind: string }): Promise<Job<T>> {\n    const job: Job<T> = { id: crypto.randomUUID(), kind: payload.kind, payload, attempts: 0 };\n    await this.put(\`jobs/\${job.id}.json\`, JSON.stringify(job));\n    return job;\n  }\n\n  async *claim(worker: string): AsyncGenerator<Job & { done(): Promise<void> }> {\n    for (const key of await this.list("jobs/")) {\n      if (await this.createOnly(\`claims/\${key}\`, worker)) {\n        yield { ...(JSON.parse(await this.get(key)) as Job), done: async () => {} };\n      }\n    }\n  }\n\n  private async put(_k: string, _v: string) {}\n  private async get(_k: string) { return "{}"; }\n  private async list(_p: string): Promise<string[]> { return []; }\n  private async createOnly(_k: string, _o: string) { return true; }\n}\n`,
  "web/package.json": `{\n  "name": "tidepool-client",\n  "version": "0.1.0",\n  "type": "module"\n}\n`,
}, "bo");

commit("Handle empty queues without spinning", {
  "src/claim.rs": `use crate::job::Job;\nuse std::time::Duration;\n\n/// Claim the oldest unclaimed job with a create-only write.\npub fn claim_next(jobs: &[Job], worker: &str) -> Option<Job> {\n    for job in jobs {\n        if try_create(&format!("claims/{}", job.id), worker) {\n            return Some(job.clone());\n        }\n    }\n    None\n}\n\n/// How long a worker sleeps when there is nothing to claim.\npub fn idle_delay(empty_polls: u32) -> Duration {\n    Duration::from_millis(250 * 2u64.pow(empty_polls.min(5)))\n}\n\nfn try_create(_key: &str, _owner: &str) -> bool {\n    true\n}\n`,
}, "ada");

commit("Document the retry schedule", {
  "docs/design.md": `# Design\n\nA job is \`jobs/<id>.json\`. A claim is \`claims/<id>\`, written with if-absent.\n\n## Retries\n\nAttempts back off as \`min(2^n, 300)\` seconds. After 8 attempts a job moves to \`dead/\`.\n\n## Leases\n\nA claim carries an expiry; a worker that stops heartbeating loses it.\n`,
}, "chen");

// A PR with fixup commits: it is melded with --autosquash and lands as one commit that names
// everything it was squashed from (meta rewritten.from).
pail(["switch", "-c", "feature/retry"]);
const retry = (body) => `pub fn backoff(attempt: u32) -> u64 {\n    2u64.pow(attempt)${body}\n}\n`;
commit("Add capped, jittered backoff", { "src/retry.rs": retry(""), "src/lib.rs": `pub mod claim;\npub mod job;\npub mod retry;\n` }, "chen", 2);
commit("fixup! Add capped, jittered backoff", { "src/retry.rs": retry(".min(300)") }, "chen", 1);
commit("fixup! Add capped, jittered backoff", { "src/retry.rs": retry(".min(300)") + `\n/// Spread retries so a burst of failures does not return as a burst.\npub fn with_jitter(base: u64, seed: u64) -> u64 {\n    base / 2 + seed % (base / 2 + 1)\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn caps() {\n        assert_eq!(backoff(20), 300);\n    }\n}\n` }, "chen", 1);
pail(["switch", "main"]);

// A burst of generated fixtures on main, then a repack: the repack is maintenance, hidden by default.
for (let i = 0; i < 4; i++) {
  commit(`Add fixture set ${i + 1}`, { [`tests/fixtures/set${i + 1}.json`]: JSON.stringify({ set: i + 1, jobs: Array.from({ length: 12 }, (_, j) => ({ id: `j${j}`, kind: "email", note: "x".repeat(40) })) }, null, 1) + "\n" }, "bo", 5);
}
pail(["maintenance", "repack"], { tick: 3600 });
pail(["sync"]);

pail(["switch", "feature/retry"]);
pail(["rebase", "--autosquash"], { who: "chen", tick: 600 });
pail(["switch", "main"]);
pail(["land", "feature/retry", "--stack"], { who: "ada", tick: 3600 });
pail(["sync"]);
const retryLanded = sh("rev-parse", "main");

// A PR whose base moved on: landing rewrites each commit and records where it came from.
pail(["switch", "-c", "pr/dead-letter"]);
commit("Move exhausted jobs to dead/", { "src/dead.rs": `use crate::job::Job;\n\npub const MAX_ATTEMPTS: u32 = 8;\n\npub fn is_dead(job: &Job) -> bool {\n    job.attempts >= MAX_ATTEMPTS\n}\n` }, "bo", 9);
commit("List dead jobs in the client", { "web/dead.ts": `import type { Job } from "./queue";\n\nexport const isDead = (job: Job) => job.attempts >= 8;\n` }, "bo", 2);
pail(["switch", "main"]);
commit("Add CI and merge policy", {
  "pail.ci.json": JSON.stringify({ checks: { build: "cargo build --locked", test: "cargo test", lint: "cargo clippy -- -D warnings", "web-types": "npm --prefix web run typecheck" }, required: ["build", "test"] }, null, 2) + "\n",
  "pail.pr.json": JSON.stringify({ approvals: 1, required_checks: ["build", "test"] }, null, 2) + "\n",
}, "ada", 6);
pail(["land", "pr/dead-letter", "--stack"], { who: "ada", tick: 1800 });
pail(["sync"]);
const deadLanded = sh("rev-parse", "main");

// ---- open branches ---------------------------------------------------------------------
pail(["switch", "-c", "pr/lease-heartbeat"]);
commit("Heartbeat claims so slow jobs keep their lease", { "src/lease.rs": `use std::time::{Duration, Instant};\n\npub struct Lease {\n    expires: Instant,\n    ttl: Duration,\n}\n\nimpl Lease {\n    pub fn new(ttl: Duration) -> Lease {\n        Lease { expires: Instant::now() + ttl, ttl }\n    }\n\n    /// Push the expiry out by one ttl. Call every ttl / 3.\n    pub fn heartbeat(&mut self) {\n        self.expires = Instant::now() + self.ttl;\n    }\n\n    pub fn expired(&self) -> bool {\n        Instant::now() >= self.expires\n    }\n}\n`, "src/lib.rs": `pub mod claim;\npub mod job;\npub mod lease;\npub mod retry;\n` }, "chen", 4);
commit("Lease: test expiry", { "src/lease.rs": `use std::time::{Duration, Instant};\n\npub struct Lease {\n    expires: Instant,\n    ttl: Duration,\n}\n\nimpl Lease {\n    pub fn new(ttl: Duration) -> Lease {\n        Lease { expires: Instant::now() + ttl, ttl }\n    }\n\n    /// Push the expiry out by one ttl. Call every ttl / 3.\n    pub fn heartbeat(&mut self) {\n        self.expires = Instant::now() + self.ttl;\n    }\n\n    pub fn expired(&self) -> bool {\n        Instant::now() >= self.expires\n    }\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn fresh_lease_is_live() {\n        assert!(!Lease::new(Duration::from_secs(30)).expired());\n    }\n}\n` }, "chen", 1);
pail(["switch", "main"]);

pail(["switch", "-c", "pr/claims-docs"]);
commit("Explain claims in the README", { "README.md": (pail(["cat", "main:README.md"])).replace("## Layout", "## How a claim works\n\nA worker lists \`jobs/\`, then writes \`claims/<id>\` with *if-absent*. Exactly one worker\nsucceeds; everyone else moves on to the next job.\n\n## Layout") }, "bo", 3);
pail(["switch", "main"]);

pail(["switch", "-c", "fix/typo-in-design"]);
commit("Fix typo in design notes", { "docs/design.md": (pail(["cat", "main:docs/design.md"])).replace("heartbeating", "sending heartbeats") }, "chen", 1);
pail(["switch", "main"]);
pail(["sync"]);

// ---- _meta: pull requests, reviews, check results --------------------------------------
const rev = (r) => sh("rev-parse", r);
const tip = rev("main");
const heads = { lease: rev("pr/lease-heartbeat"), docs: rev("pr/claims-docs"), typo: rev("fix/typo-in-design") };
const at = (offsetH) => NOW - offsetH * 3600;

const prs = [
  { id: 1, title: "Retry failed jobs with capped, jittered backoff", body: "Jobs that fail now back off exponentially, capped at five minutes, with jitter so a burst of failures does not come back as a burst.\n\n- \`retry::backoff\` caps at 300s\n- \`retry::with_jitter\` spreads the retries\n\nSquashed on landing.", head: "feature/retry", base: "main", author: "chen", state: "merged", created: at(400), merged_commit: retryLanded, merged_by: "ada", merged_at: at(380) },
  { id: 2, title: "Heartbeat claims so slow jobs keep their lease", body: "A worker running a long job loses its claim when the lease expires, and a second worker picks the job up.\n\nThis adds \`Lease::heartbeat\`, to be called every ttl / 3.\n\n### Not in this PR\n\n- Wiring the heartbeat into the claim loop (next PR)\n- A cancellation signal when the lease is lost", head: "pr/lease-heartbeat", base: "main", author: "chen", state: "open", created: at(30) },
  { id: 3, title: "Explain claims in the README", body: "Adds a short section on how a claim works, since it is the first thing people ask.", head: "pr/claims-docs", base: "main", author: "bo", state: "open", created: at(20) },
  { id: 4, title: "Fix typo in design notes", body: "", head: "fix/typo-in-design", base: "main", author: "chen", state: "closed", created: at(60), updated: at(50), closed_at: at(50), closed_by: "chen" },
  { id: 5, title: "Move exhausted jobs to dead/", body: "After eight attempts a job moves to \`dead/\` and is listed by the client.", head: "pr/dead-letter", base: "main", author: "bo", state: "merged", created: at(300), merged_commit: deadLanded, merged_by: "ada", merged_at: at(250) },
];
for (const pr of prs) meta(`prs/${pr.id}.json`, pr, `Open PR ${pr.id}: ${pr.title}`, { ifAbsent: true });

const comment = (n, who, body, h) => meta(`prs/${n}/comments/${String((at(h)) * 1000).padStart(13, "0")}-${who}.json`, { who, body, at: at(h) }, `Comment on PR ${n}`, { ifAbsent: true });
comment(2, "bo", "Nice and small. Is `ttl / 3` a hard rule, or just what we call it in the docs?", 14);
comment(2, "chen", "Just the docs for now. I will enforce it in the claim loop next, as a separate PR so this one stays reviewable.", 13);
meta("prs/2/reviews/ada.json", { verdict: "approve", comment: "Looks right. Please wire it into the claim loop next.", at: at(12) }, "Review PR 2: approve");
meta("prs/2/reviews/bo.json", { verdict: "changes", comment: "The heartbeat interval is documented but not enforced anywhere.", at: at(18) }, "Review PR 2: changes");
meta("prs/3/reviews/ada.json", { verdict: "approve", comment: "", at: at(5) }, "Review PR 3: approve");
meta("prs/1/reviews/ada.json", { verdict: "approve", comment: "Squash it.", at: at(390) }, "Review PR 1: approve");

const check = (commit, name, state, extra) => meta(`checks/${commit}/${name}.json`, { state, branch: "ci", runner: "ci-1", at: at(1), ...extra }, `ci: ${name} ${state} on ${commit.slice(0, 12)}`);
const buildLog = "   Compiling tidepool v0.1.0\n    Finished dev [unoptimized + debuginfo] target(s) in 3.41s\n";
const pass = (command, d, log) => ({ command, exit: 0, duration_s: d, log_tail: log });
check(tip, "build", "success", pass("cargo build --locked", 3.4, buildLog));
check(tip, "test", "success", pass("cargo test", 8.9, "running 3 tests\ntest retry::tests::caps ... ok\ntest lease::tests::fresh_lease_is_live ... ok\ntest claim::tests::claims_oldest ... ok\n\ntest result: ok. 3 passed; 0 failed\n"));
check(tip, "lint", "success", pass("cargo clippy -- -D warnings", 5.1, "    Checking tidepool v0.1.0\n    Finished dev target(s) in 2.02s\n"));
check(tip, "web-types", "success", pass("npm --prefix web run typecheck", 6.7, "> tsc --noEmit\n"));

check(heads.lease, "build", "success", pass("cargo build --locked", 3.9, buildLog));
check(heads.lease, "test", "failure", { command: "cargo test", exit: 101, duration_s: 9.2, log_tail: "running 4 tests\ntest lease::tests::fresh_lease_is_live ... ok\ntest lease::tests::heartbeat_extends ... FAILED\n\nfailures:\n\n---- lease::tests::heartbeat_extends stdout ----\nthread 'lease::tests::heartbeat_extends' panicked at src/lease.rs:31:9:\nassertion failed: lease.remaining() > Duration::from_secs(29)\n\ntest result: FAILED. 3 passed; 1 failed\n" });
check(heads.lease, "lint", "pending", {});
check(heads.docs, "build", "success", pass("cargo build --locked", 3.1, buildLog));
check(heads.docs, "test", "success", pass("cargo test", 7.8, "test result: ok. 3 passed; 0 failed\n"));
check(heads.typo, "build", "success", pass("cargo build --locked", 3.0, buildLog));
check(heads.typo, "test", "pending", {});

rmSync(wc, { recursive: true, force: true });
rmSync(work, { recursive: true, force: true });
writeFileSync(
  join(out, "pail-web.config.json"),
  JSON.stringify({ repos: [{ name: "tidepool", store, description: "A job queue that lives in a bucket (demo data)." }] }, null, 2) + "\n",
);
console.log(`demo repository: ${store}\nconfig: ${join(out, "pail-web.config.json")}`);
