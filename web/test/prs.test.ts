// Pull requests through the API: what the plugin writes is read back, and what the API writes
// is the same record (the end-to-end test with the real plugin is in plugin.test.ts).
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { boot, eventually, freshDemo, json, makeBranch, send, type Booted } from "./helpers";

describe("reading (actions off)", () => {
  let app: Booted;
  beforeAll(async () => { app = await boot(); });
  afterAll(() => app.close());

  it("shows comments in order, with the count on the list", async () => {
    const d = await json(app.base, "/api/repos/tidepool/prs/2");
    expect(d.comments.map((c: any) => c.who)).toEqual(["bo", "chen"]);
    expect(d.comments[0].body).toContain("ttl / 3");
    const list = await json(app.base, "/api/repos/tidepool/prs");
    expect(list.find((p: any) => p.id === 2).comments).toBe(2);
    expect(list.find((p: any) => p.id === 3).comments).toBe(0);
  });

  it("carries closed metadata, and marks reviews by the author", async () => {
    const closed = await json(app.base, "/api/repos/tidepool/prs/4");
    expect(closed.pr).toMatchObject({ state: "closed", closed_by: "chen" });
    expect(closed.pr.closed_at).toBeGreaterThan(0);
    expect(closed.readiness.problems).toContain("pull request is closed");
  });

  it("compares two branches: commits, the diff, the defaults for a new PR, the PR already open", async () => {
    const c = await json(app.base, "/api/repos/tidepool/compare?base=main&head=pr/lease-heartbeat");
    expect(c.commits.map((x: any) => x.subject)).toEqual(["Lease: test expiry", "Heartbeat claims so slow jobs keep their lease"]);
    expect(c.title).toBe("Heartbeat claims so slow jobs keep their lease");
    expect(c.patch).toContain("diff --git a/src/lease.rs b/src/lease.rs");
    expect(c.existing).toBe(2);
    const none = await json(app.base, "/api/repos/tidepool/compare?base=main&head=main");
    expect(none.commits).toEqual([]);
    expect((await fetch(`${app.base}/api/repos/tidepool/compare?base=main&head=nope`)).status).toBe(404);
    expect((await fetch(`${app.base}/api/repos/tidepool/compare?base=main&head=--evil`)).status).toBe(400);
    expect((await fetch(`${app.base}/api/repos/tidepool/compare?base=main&head=_meta`)).status).toBe(400);
  });

  it("refuses every write", async () => {
    const w = "/api/repos/tidepool/prs";
    for (const [m, p, b] of [
      ["POST", w, { title: "x", head: "pr/claims-docs", base: "main" }],
      ["PATCH", `${w}/3`, { title: "x" }],
      ["POST", `${w}/3/comments`, { body: "x" }],
      ["POST", `${w}/3/close`, {}],
      ["POST", `${w}/4/reopen`, {}],
      ["POST", `${w}/3/draft`, { draft: true }],
    ] as const) expect((await send(app.base, m, p, b)).status, `${m} ${p}`).toBe(403);
    expect((await json(app.base, "/api/repos/tidepool/prs/3")).pr.title).toBe("Explain claims in the README");
  });
});

describe("writing (actions on)", () => {
  let app: Booted;
  let store: string;
  const api = "/api/repos/tidepool";
  beforeAll(async () => {
    store = freshDemo();
    for (const n of ["a", "b", "c", "d", "same"]) makeBranch(store, `topic/${n}`, [[`Add ${n}\n\nWhy ${n}.`, { [`${n}.txt`]: `${n}\n` }]]);
    app = await boot({ actions: true, author: "Rae Reviewer <rae@example.com>", pollMs: 200 }, store);
  });
  afterAll(() => app.close());

  it("opens a PR as the plugin does: a plain record on _meta, claimed with --if-absent", async () => {
    const r = await send(app.base, "POST", `${api}/prs`, { title: "Add a", body: "Because a.", head: "topic/a", base: "main" });
    expect(r.status).toBe(201);
    expect(r.body.pr).toMatchObject({ id: 6, title: "Add a", head: "topic/a", base: "main", author: "Rae Reviewer", state: "open", draft: false });
    const { execFileSync } = await import("node:child_process");
    const raw = JSON.parse(execFileSync(process.env.PAIL_BIN!, ["--store", store, "cat", "_meta:prs/6.json"]).toString());
    expect(raw).toMatchObject({ id: 6, state: "open", draft: false, author: "Rae Reviewer" });
    expect(raw.created).toBeGreaterThan(1e9);
    expect(raw.updated).toBe(raw.created);
    const d = await json(app.base, `${api}/prs/6`);
    expect(d.commits).toHaveLength(1);
    expect(d.readiness.ready).toBe(false);
  });

  it("claims distinct ids when several are opened at once", async () => {
    const rs = await Promise.all(["b", "c", "d"].map((n) => send(app.base, "POST", `${api}/prs`, { title: `Add ${n}`, head: `topic/${n}`, base: "main", draft: true })));
    expect(rs.map((r) => r.status)).toEqual([201, 201, 201]);
    expect(new Set(rs.map((r) => r.body.pr.id)).size).toBe(3);
    expect(rs.every((r) => r.body.pr.draft === true)).toBe(true);
  });

  it("validates input before it reaches the CLI", async () => {
    const ok = { title: "T", head: "topic/same", base: "main" };
    const bad: [string, any, number][] = [
      ["no title", { ...ok, title: "  " }, 400],
      ["multi-line title", { ...ok, title: "a\nb" }, 400],
      ["long title", { ...ok, title: "x".repeat(257) }, 400],
      ["non-text body", { ...ok, body: 5 }, 400],
      ["long body", { ...ok, body: "x".repeat(60001) }, 400],
      ["flag as head", { ...ok, head: "--delete" }, 400],
      ["meta as head", { ...ok, head: "_meta" }, 400],
      ["space in base", { ...ok, base: "ma in" }, 400],
      ["missing head", { ...ok, head: "topic/zzz" }, 404],
      ["same branch", { ...ok, head: "main" }, 400],
      ["no commits", { ...ok, head: "feature/retry", base: "main" }, 400],
      ["duplicate", { title: "again", head: "topic/a", base: "main" }, 409],
    ];
    for (const [label, body, status] of bad) expect((await send(app.base, "POST", `${api}/prs`, body)).status, label).toBe(status);
    expect((await send(app.base, "POST", `${api}/prs`, { ...ok })).status).toBe(201);
    expect((await send(app.base, "PATCH", `${api}/prs/abc`, { title: "x" })).status).toBe(404);
    expect((await send(app.base, "POST", `${api}/prs/6/comments`, { body: " " })).status).toBe(400);
    const res = await fetch(`${app.base}${api}/prs`, { method: "POST", headers: { "Content-Type": "application/json" }, body: "{not json" });
    expect(res.status).toBe(400);
  });

  it("comments, edits, drafts, closes and reopens, and keeps updated times", async () => {
    const c = await send(app.base, "POST", `${api}/prs/6/comments`, { body: "First!" });
    expect(c.status).toBe(201);
    await send(app.base, "POST", `${api}/prs/6/comments`, { body: "Second **point**" });
    const d = await json(app.base, `${api}/prs/6`);
    expect(d.comments.map((x: any) => x.body)).toEqual(["First!", "Second **point**"]);
    expect(d.comments[0]).toMatchObject({ who: "Rae Reviewer" });
    expect(d.comments[0].id).toMatch(/^\d{13}-Rae-Reviewer$/);

    expect((await send(app.base, "PATCH", `${api}/prs/6`, { title: "Add a, better", body: "New words" })).status).toBe(200);
    expect((await send(app.base, "PATCH", `${api}/prs/6`, {})).status).toBe(400);
    let pr = (await json(app.base, `${api}/prs/6`)).pr;
    expect(pr).toMatchObject({ title: "Add a, better", body: "New words", head: "topic/a" });
    expect(pr.updated).toBeGreaterThanOrEqual(pr.created);

    await send(app.base, "POST", `${api}/prs/6/draft`, { draft: true });
    let det = await json(app.base, `${api}/prs/6`);
    expect(det.pr.draft).toBe(true);
    expect(det.readiness.problems).toContain("pull request is a draft");
    expect((await send(app.base, "POST", `${api}/prs/6/merge`, { force: true })).status).toBe(409);
    await send(app.base, "POST", `${api}/prs/6/draft`, { draft: false });

    expect((await send(app.base, "POST", `${api}/prs/6/close`, {})).status).toBe(200);
    pr = (await json(app.base, `${api}/prs/6`)).pr;
    expect(pr).toMatchObject({ state: "closed", closed_by: "Rae Reviewer" });
    expect(pr.closed_at).toBeGreaterThan(0);
    expect((await send(app.base, "POST", `${api}/prs/6/close`, {})).status).toBe(409);
    expect((await send(app.base, "POST", `${api}/prs/6/draft`, { draft: true })).status).toBe(409);
    expect((await send(app.base, "POST", `${api}/prs/6/merge`, {})).status).toBe(409);
    expect((await send(app.base, "POST", `${api}/prs/6/reopen`, {})).status).toBe(200);
    pr = (await json(app.base, `${api}/prs/6`)).pr;
    expect(pr.state).toBe("open");
    expect(pr.closed_at).toBeUndefined();
    expect((await send(app.base, "POST", `${api}/prs/6/reopen`, {})).status).toBe(409);
  });

  it("merges a PR that was opened here, and records where it landed", async () => {
    // Self-approval does not count; Bob's does (a review file per person).
    await send(app.base, "POST", `${api}/prs/6/review`, { verdict: "approve" });
    const { execFileSync } = await import("node:child_process");
    const asBob = JSON.stringify({ verdict: "approve", comment: "lgtm", at: 1 });
    execFileSync(process.env.PAIL_BIN!, ["--store", store, "put", "_meta", "-m", "Review PR 6", "--file", "prs/6/reviews/bob.json=-"], { input: asBob, env: { ...process.env, PAIL_AUTHOR: "Bob <b@x>" } });
    const d = await eventually(async () => { const x = await json(app.base, `${api}/prs/6`); return x.reviews.some((r: any) => r.who === "bob") && x; });
    expect(d.reviews.find((r: any) => r.who === "bob")).toMatchObject({ verdict: "approve", byAuthor: false });
    expect(d.reviews.find((r: any) => r.who === "Rae Reviewer")).toMatchObject({ byAuthor: true });
    expect(d.readiness.problems.join()).not.toContain("approvals");
    // No check has run on the new branch, so policy blocks it until forced.
    expect(d.readiness.problems.join()).toContain("required checks");
    expect((await send(app.base, "POST", `${api}/prs/6/merge`, {})).status).toBe(409);
    expect((await send(app.base, "POST", `${api}/prs/6/merge`, { force: true })).status).toBe(200);
    const after = (await json(app.base, `${api}/prs/6`)).pr;
    expect(after).toMatchObject({ state: "merged", merged_by: "Rae Reviewer" });
    expect(after.merged_commit).toHaveLength(64);
    expect((await json(app.base, `${api}/blob?ref=main&path=a.txt`)).text).toBe("a\n");
  });
});
