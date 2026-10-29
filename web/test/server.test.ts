import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { Cache } from "../server/cache";
import { loadConfig, parseStoreList, redactStore } from "../server/config";
import { formatFilePatch, parseBatch, parseDate, parsePerson, rollupOf, toSummary } from "../server/repo";
import { boot, freshDemo, json, type Booted } from "./helpers";

describe("config", () => {
  it("reads repos from the environment, with names from the URL's last segment", () => {
    const cfg = loadConfig([], { PAIL_WEB_REPOS: "app=gs://b/app, s3://bucket/team/api" } as any, "/nonexistent");
    expect(cfg.repos.map((r) => [r.name, r.store])).toEqual([["app", "gs://b/app"], ["api", "s3://bucket/team/api"]]);
    expect(cfg.actions).toBe(false);
    expect(cfg.pail).toBe("pail");
  });
  it("lets the environment override the file's port and actions", () => {
    const cfg = loadConfig([], { PAIL_WEB_REPOS: "a=file:///x", PAIL_WEB_PORT: "9000", PAIL_WEB_ACTIONS: "1" } as any, "/nonexistent");
    expect([cfg.port, cfg.actions]).toEqual([9000, true]);
  });
  it("falls back to PAIL_STORE and rejects bad or duplicate names", () => {
    expect(loadConfig([], { PAIL_STORE: "file:///tmp/x/demo" } as any, "/nonexistent").repos[0].name).toBe("demo");
    expect(() => loadConfig([], { PAIL_WEB_REPOS: "a b=file:///x" } as any, "/nonexistent")).toThrow(/must match/);
    expect(() => loadConfig([], { PAIL_WEB_REPOS: "a=file:///x,a=file:///y" } as any, "/nonexistent")).toThrow(/twice/);
  });
  it("never shows credentials in a store URL", () => {
    expect(redactStore("https://user:secret@host/repo?token=x")).toBe("https://host/repo");
    expect(redactStore("gs://bucket/prefix")).toBe("gs://bucket/prefix");
    expect(parseStoreList("x=https://h/r")[0]).toEqual({ name: "x", store: "https://h/r" });
  });
});

describe("pure helpers", () => {
  it("parses people, dates and commit meta", () => {
    expect(parsePerson("Ada Okafor <ada@example.com>")).toEqual({ name: "Ada Okafor", email: "ada@example.com" });
    expect(parseDate("1793304037 +0000")).toBe("2026-10-29T20:00:37.000Z");
    const c = toSummary({ id: "a".repeat(64), parent: null, author: "A <a@x>", author_date: "0 +0000", committer: "A <a@x>", committer_date: "0 +0000", message: "Subject\n\nBody line", meta: { "pail.maintenance": "repack", "rewritten.from": "b c" }, files: 1, bytes: 1 });
    expect(c).toMatchObject({ subject: "Subject", body: "Body line", maintenance: true, rewrittenFrom: ["b", "c"] });
  });
  it("parses cat-file --batch output, including missing objects and binary bodies", () => {
    const body = Buffer.from([0, 1, 2, 10, 3]);
    const out = Buffer.concat([Buffer.from("abc blob 5\nhello\n"), Buffer.from("x:nope missing\n"), Buffer.from("def blob 5\n"), body, Buffer.from("\n")]);
    const m = parseBatch(out, ["x:a", "x:nope", "x:b"]);
    expect(m.get("x:a")!.toString()).toBe("hello");
    expect(m.get("x:nope")).toBeNull();
    expect(m.get("x:b")!.equals(body)).toBe(true);
  });
  it("formats git-style patches for added, deleted, changed and binary files", () => {
    const add = formatFilePatch("a.txt", null, Buffer.from("one\ntwo\n"));
    expect(add).toContain("new file mode");
    expect(add).toContain("@@ -0,0 +1,2 @@");
    const mod = formatFilePatch("a.txt", Buffer.from("one\ntwo\nthree\n"), Buffer.from("one\n2\nthree\n"));
    expect(mod).toContain("-two\n+2\n");
    expect(formatFilePatch("a.txt", Buffer.from("x\n"), null)).toContain("deleted file mode");
    expect(formatFilePatch("b.bin", Buffer.from([0, 1]), Buffer.from([0, 2]))).toContain("Binary files");
  });
  it("rolls check states up: failure beats pending beats success", () => {
    expect(rollupOf([]).state).toBe("none");
    expect(rollupOf(["success", "success"]).state).toBe("success");
    expect(rollupOf(["success", "pending"]).state).toBe("pending");
    expect(rollupOf(["pending", "failure", "success"])).toMatchObject({ state: "failure", passed: 1, failed: 1, pending: 1, total: 3 });
  });
});

describe("cache", () => {
  it("coalesces concurrent loads of one key and serves hits", async () => {
    const c = new Cache(1 << 20);
    let loads = 0;
    const load = async () => { loads++; await new Promise((r) => setTimeout(r, 10)); return "v"; };
    expect(await Promise.all([c.get("k", load), c.get("k", load), c.get("k", load)])).toEqual(["v", "v", "v"]);
    await c.get("k", load);
    expect(loads).toBe(1);
  });
  it("evicts least recently used entries past its budget, and expires ttl entries", async () => {
    const c = new Cache(2000);
    for (let i = 0; i < 20; i++) await c.get("k" + i, async () => "x".repeat(200));
    expect(c.stats().bytes).toBeLessThanOrEqual(2000);
    let n = 0;
    await c.ttl("t", 5, async () => ++n);
    await c.ttl("t", 5, async () => ++n);
    expect(n).toBe(1);
    await new Promise((r) => setTimeout(r, 10));
    await c.ttl("t", 5, async () => ++n);
    expect(n).toBe(2);
    c.invalidate("t");
    await c.ttl("t", 5000, async () => ++n);
    expect(n).toBe(3);
  });
});

describe("API over the demo repository", () => {
  let app: Booted;
  beforeAll(async () => { app = await boot(); });
  afterAll(() => app.close());

  it("lists configured repos without leaking credentials", async () => {
    const cfg = await json(app.base, "/api/config");
    expect(cfg.actions).toBe(false);
    expect(cfg.repos).toHaveLength(1);
    expect(cfg.repos[0].name).toBe("tidepool");
  });

  it("describes branches, hiding _meta, with the default first", async () => {
    const o = await json(app.base, "/api/repos/tidepool");
    expect(o.default).toBe("main");
    const names = o.branches.map((b: any) => b.name);
    expect(names[0]).toBe("main");
    expect(names).toEqual(expect.arrayContaining(["feature/retry", "pr/lease-heartbeat", "pr/dead-letter"]));
    expect(names).not.toContain("_meta");
    expect(o.branches.find((b: any) => b.name === "main").checks).toMatchObject({ state: "success", total: 4 });
    expect(o.branches.find((b: any) => b.name === "pr/lease-heartbeat").checks.state).toBe("failure");
  });

  it("hides maintenance commits by default and shows them on request", async () => {
    const plain = await json(app.base, "/api/repos/tidepool/log?ref=main&limit=100");
    const all = await json(app.base, "/api/repos/tidepool/log?ref=main&limit=100&maintenance=1");
    expect(plain.commits.some((c: any) => c.maintenance)).toBe(false);
    const repack = all.commits.filter((c: any) => c.maintenance);
    expect(repack).toHaveLength(1);
    expect(repack[0].subject).toMatch(/repack/);
    expect(all.commits.length).toBe(plain.commits.length + 1);
  });

  it("paginates the log", async () => {
    const p1 = await json(app.base, "/api/repos/tidepool/log?ref=main&limit=4");
    const p2 = await json(app.base, "/api/repos/tidepool/log?ref=main&limit=4&skip=4");
    expect(p1.commits).toHaveLength(4);
    expect(p1.more).toBe(true);
    expect(p2.commits[0].id).not.toBe(p1.commits[3].id);
    expect(p2.commits[0].id).toBe((await json(app.base, "/api/repos/tidepool/log?ref=main&limit=5")).commits[4].id);
    const last = await json(app.base, "/api/repos/tidepool/log?ref=main&limit=100&skip=8");
    expect(last.more).toBe(false);
  });

  it("links squashed and rewritten commits to their sources", async () => {
    const { commits } = await json(app.base, "/api/repos/tidepool/log?ref=main&limit=100");
    const squash = commits.find((c: any) => c.rewrittenFrom.length > 1);
    expect(squash.subject).toBe("Add capped, jittered backoff");
    expect(squash.rewrittenFrom).toHaveLength(3);
    const one = commits.find((c: any) => c.subject === "Move exhausted jobs to dead/");
    expect(one.rewrittenFrom).toHaveLength(1);
    expect(one.landedFrom).toBe("pr/dead-letter");
    const detail = await json(app.base, `/api/repos/tidepool/commit/${squash.id}`);
    expect(detail.sources.map((s: any) => s.subject)).toEqual(expect.arrayContaining(["Add capped, jittered backoff", "fixup! Add capped, jittered backoff"]));
    expect(detail.patch).toContain("diff --git a/src/retry.rs b/src/retry.rs");
  });

  it("serves trees, files, the README and the file list", async () => {
    const root = await json(app.base, "/api/repos/tidepool/tree?ref=main");
    expect(root.entries.map((e: any) => e.name)).toEqual(expect.arrayContaining(["README.md", "src", "web"]));
    expect(root.entries[0].type).toBe("tree");
    const src = await json(app.base, "/api/repos/tidepool/tree?ref=main&path=src");
    expect(src.entries.map((e: any) => e.name)).toContain("claim.rs");
    const blob = await json(app.base, "/api/repos/tidepool/blob?ref=main&path=src/claim.rs");
    expect(blob.text).toContain("claim_next");
    expect(blob.binary).toBe(false);
    const readme = await json(app.base, "/api/repos/tidepool/readme?ref=main");
    expect(readme.text).toContain("# tidepool");
    const files = await json(app.base, "/api/repos/tidepool/files?ref=main");
    expect(files).toContain("src/claim.rs");
    const feature = await json(app.base, "/api/repos/tidepool/tree?ref=feature/retry&path=src");
    expect(feature.entries.map((e: any) => e.name)).toContain("retry.rs");
  });

  it("serves raw bytes safely and refuses bad refs and paths", async () => {
    const raw = await fetch(`${app.base}/api/repos/tidepool/raw?ref=main&path=logo.svg`);
    expect(raw.headers.get("content-type")).toMatch(/^text\/plain/);
    expect(raw.headers.get("content-security-policy")).toContain("sandbox");
    const dl = await fetch(`${app.base}/api/repos/tidepool/raw?ref=main&path=logo.svg&download=1`);
    expect(dl.headers.get("content-disposition")).toContain('filename="logo.svg"');
    expect((await fetch(`${app.base}/api/repos/tidepool/blob?ref=main&path=nope.txt`)).status).toBe(404);
    expect((await fetch(`${app.base}/api/repos/tidepool/tree?ref=nope`)).status).toBe(404);
    expect((await fetch(`${app.base}/api/repos/tidepool/log?ref=--output=/tmp/x`)).status).toBe(400);
    expect((await fetch(`${app.base}/api/repos/nope`)).status).toBe(404);
  });

  it("lists pull requests with state, reviews and check roll-ups", async () => {
    const prs = await json(app.base, "/api/repos/tidepool/prs");
    expect(prs.map((p: any) => [p.id, p.state])).toEqual([[5, "merged"], [4, "closed"], [3, "open"], [2, "open"], [1, "merged"]]);
    const two = prs.find((p: any) => p.id === 2);
    expect(two).toMatchObject({ approvals: 1, changesRequested: 1 });
    expect(two.checks.state).toBe("failure");
    expect(prs.find((p: any) => p.id === 3).checks.state).toBe("success");
  });

  it("builds a PR page: policy, readiness, required checks, commits, and a diff against the base", async () => {
    const d = await json(app.base, "/api/repos/tidepool/prs/2");
    expect(d.policy).toEqual({ approvals: 1, required_checks: ["build", "test"] });
    expect(d.readiness.ready).toBe(false);
    expect(d.readiness.problems.join()).toContain("test");
    const byName = Object.fromEntries(d.checks.map((c: any) => [c.name, c]));
    expect(byName.test).toMatchObject({ state: "failure", required: true });
    expect(byName.test.log_tail).toContain("FAILED");
    expect(byName.lint).toMatchObject({ state: "pending", required: false });
    expect(d.reviews.map((r: any) => r.who).sort()).toEqual(["ada", "bo"]);
    expect(d.commits).toHaveLength(2);
    const diff = await json(app.base, "/api/repos/tidepool/prs/2/diff");
    expect(diff.patch).toContain("diff --git a/src/lease.rs b/src/lease.rs");
    expect(diff.patch).toContain("+    pub fn heartbeat");
    const ready = await json(app.base, "/api/repos/tidepool/prs/3");
    expect(ready.readiness).toEqual({ ready: true, problems: [] });
    expect((await fetch(`${app.base}/api/repos/tidepool/prs/99`)).status).toBe(404);
  });

  it("shows checks for a commit, declared ones included", async () => {
    const { commit, checks } = await json(app.base, "/api/repos/tidepool/checks?ref=main");
    expect(commit).toHaveLength(64);
    expect(checks.map((c: any) => [c.name, c.state, c.required])).toEqual([["build", "success", true], ["test", "success", true], ["lint", "success", false], ["web-types", "success", false]]);
  });

  it("is read-only by default: actions are refused", async () => {
    for (const [path, body] of [["review", { verdict: "approve" }], ["merge", {}]] as const) {
      const res = await fetch(`${app.base}/api/repos/tidepool/prs/3/${path}`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
      expect(res.status).toBe(403);
    }
  });

  it("caches by commit: repeated reads spawn no more processes", async () => {
    await json(app.base, "/api/repos/tidepool/tree?ref=main&path=src");
    const before = (await json(app.base, "/api/stats")).spawned;
    for (let i = 0; i < 3; i++) await json(app.base, "/api/repos/tidepool/blob?ref=main&path=src/claim.rs");
    await json(app.base, "/api/repos/tidepool/blob?ref=main&path=src/claim.rs");
    const mid = (await json(app.base, "/api/stats")).spawned;
    await json(app.base, "/api/repos/tidepool/blob?ref=main&path=src/claim.rs");
    expect((await json(app.base, "/api/stats")).spawned).toBe(mid);
    expect(mid - before).toBeLessThanOrEqual(4); // refs (ttl) + the first blob's size and bytes
  });
});

describe("actions (enabled)", () => {
  let app: Booted;
  beforeAll(async () => { app = await boot({ actions: true, author: "Rae Reviewer <rae@example.com>" }, freshDemo()); });
  afterAll(() => app.close());

  it("records a review through pail put, then merges a ready PR with pail land", async () => {
    const post = (path: string, body: unknown) => fetch(`${app.base}/api/repos/tidepool/prs/3/${path}`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
    expect((await post("review", { verdict: "approve", comment: "Ship it" })).status).toBe(200);
    const d = await json(app.base, "/api/repos/tidepool/prs/3");
    expect(d.actions).toBe(true);
    expect(d.reviews.find((r: any) => r.who === "Rae Reviewer")).toMatchObject({ verdict: "approve", comment: "Ship it" });
    expect((await post("merge", {})).status).toBe(200);
    const after = await json(app.base, "/api/repos/tidepool/prs/3");
    expect(after.pr.state).toBe("merged");
    const readme = await json(app.base, "/api/repos/tidepool/blob?ref=main&path=README.md");
    expect(readme.text).toContain("How a claim works");
    // PR 2 is not ready (a required check fails), so merging it is refused.
    const blocked = await fetch(`${app.base}/api/repos/tidepool/prs/2/merge`, { method: "POST", headers: { "Content-Type": "application/json" }, body: "{}" });
    expect(blocked.status).toBe(409);
  });
});

describe("live updates", () => {
  it("streams ref changes over SSE and re-reads refs when a branch moves", async () => {
    const store = freshDemo();
    const app = await boot({ pollMs: 200 }, store);
    try {
      const ctl = new AbortController();
      const res = await fetch(`${app.base}/api/repos/tidepool/events`, { signal: ctl.signal });
      expect(res.headers.get("content-type")).toContain("text/event-stream");
      const reader = res.body!.getReader();
      const dec = new TextDecoder();
      let text = "";
      const until = async (re: RegExp, ms = 20000) => {
        const end = Date.now() + ms;
        while (!re.test(text) && Date.now() < end) {
          const r = await Promise.race([reader.read(), new Promise<null>((r) => setTimeout(() => r(null), 500))]);
          if (r && !r.done) text += dec.decode(r.value);
        }
        return re.test(text);
      };
      expect(await until(/: connected/)).toBe(true);
      const before = (await json(app.base, "/api/repos/tidepool")).branches.length;
      const { execFileSync } = await import("node:child_process");
      execFileSync(process.env.PAIL_BIN!, ["--store", store, "put", "live/branch", "-m", "Live", "--file", "hello.txt=-"], { input: "hi", env: { ...process.env, PAIL_AUTHOR: "T <t@x>" } });
      expect(await until(/event: ref\ndata: .*"branch":"live\/branch"/)).toBe(true);
      const after = await json(app.base, "/api/repos/tidepool");
      expect(after.branches.length).toBe(before + 1);
      expect(after.branches.map((b: any) => b.name)).toContain("live/branch");
      ctl.abort();
    } finally {
      await app.close();
    }
  });
});
