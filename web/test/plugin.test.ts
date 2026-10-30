// End to end: `pail pr` (the plugin, through pail's plugin dispatch) and pail-web's API read and
// write the same records on `_meta`, in both directions. Needs python3 for the plugin.
import { execFileSync, spawnSync } from "node:child_process";
import { join } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { boot, eventually, freshDemo, json, makeBranch, metaJson, pailEnv, PAIL, pailIn, pushTo, send, type Booted } from "./helpers";

const ext = join(import.meta.dirname, "..", "..", "extensions");
const hasPython = spawnSync("python3", ["--version"]).status === 0;

describe.skipIf(!hasPython)("pail pr and pail-web share one set of records", () => {
  let app: Booted;
  let store: string;
  const api = "/api/repos/tidepool";

  /** `pail pr …` as `who`, against the test store. Returns stdout; throws with stderr on failure. */
  const pr = (who: string, ...args: string[]) =>
    execFileSync(PAIL(), ["--store", store, "pr", ...args], { env: pailEnv(who, { PATH: `${ext}:${process.env.PATH}` }), encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
  const prFails = (who: string, ...args: string[]) => {
    const r = spawnSync(PAIL(), ["--store", store, "pr", ...args], { env: pailEnv(who, { PATH: `${ext}:${process.env.PATH}` }), encoding: "utf8" });
    expect(r.status).not.toBe(0);
    return r.stderr;
  };
  let notes = 0; // the PR on topic/notes, shared by the last tests
  const web = (id: number) => json(app.base, `${api}/prs/${id}`);

  beforeAll(async () => {
    store = freshDemo();
    makeBranch(store, "topic/from-cli", [["Say hello\n\nThe CLI opened this.", { "hello.txt": "hello\n" }]]);
    makeBranch(store, "topic/from-web", [["Say goodbye", { "bye.txt": "bye\n" }]]);
    makeBranch(store, "topic/notes", [["Add notes", { "notes.txt": "one\ntwo\nthree\nfour\n" }]]);
    app = await boot({ actions: true, author: "Wanda Web <wanda@example.com>", pollMs: 200 }, store);
  });
  afterAll(() => app.close());

  it("a PR made with `pail pr create` shows up in the web API, comments and reviews included", async () => {
    const rec = JSON.parse(pr("Carol <c@x>", "create", "--head", "topic/from-cli", "--json"));
    expect(rec).toMatchObject({ id: 6, title: "Say hello", body: "The CLI opened this.", base: "main", author: "Carol", draft: false });
    const d = await eventually(async () => { try { return await web(6); } catch { return false; } });
    expect(d.pr).toMatchObject({ title: "Say hello", head: "topic/from-cli", author: "Carol", state: "open" });
    expect(d.commits.map((c: any) => c.subject)).toEqual(["Say hello"]);

    pr("Carol <c@x>", "comment", "6", "-m", "Please look at this");
    pr("Dan Dev <d@x>", "review", "6", "--request-changes", "-m", "Needs a newline");
    const seen = await eventually(async () => { const x = await web(6); return x.comments.length && x.reviews.length && x; });
    expect(seen.comments[0]).toMatchObject({ who: "Carol", body: "Please look at this" });
    expect(seen.reviews[0]).toMatchObject({ who: "Dan Dev", verdict: "changes", comment: "Needs a newline", byAuthor: false });
    expect((await json(app.base, `${api}/prs`)).find((p: any) => p.id === 6)).toMatchObject({ comments: 1, changesRequested: 1 });
  });

  it("a PR made through the web API shows up in `pail pr list` and `pail pr view`", async () => {
    const r = await send(app.base, "POST", `${api}/prs`, { title: "Say goodbye", body: "The web opened this.", head: "topic/from-web", base: "main", draft: true });
    expect(r.status).toBe(201);
    const id = r.body.pr.id;
    expect(id).toBe(7);
    const list = pr("Carol <c@x>", "list");
    expect(list).toContain("#7");
    expect(list).toContain("draft");
    expect(list).toContain("Say goodbye");
    const view = JSON.parse(pr("Carol <c@x>", "view", String(id), "--json"));
    expect(view.pr).toMatchObject({ title: "Say goodbye", body: "The web opened this.", author: "Wanda Web", draft: true, state: "open" });
    expect(view.ready).toBe(false);
    expect(view.problems).toContain("pull request is a draft");

    await send(app.base, "POST", `${api}/prs/${id}/comments`, { body: "From the browser" });
    await send(app.base, "POST", `${api}/prs/${id}/review`, { verdict: "approve", comment: "web says yes" });
    const text = pr("Carol <c@x>", "view", String(id));
    expect(text).toContain("From the browser");
    expect(text).toContain("Wanda Web");
    expect(text).toContain("approve: web says yes");
    expect(text).toContain("[open, draft]");
  });

  it("drafts, edits, close and reopen cross over in both directions", async () => {
    // web edits, plugin sees
    await send(app.base, "PATCH", `${api}/prs/7`, { title: "Say goodbye, politely" });
    expect(JSON.parse(pr("Carol <c@x>", "view", "7", "--json")).pr.title).toBe("Say goodbye, politely");
    // plugin readies, web sees
    pr("Wanda Web <w@x>", "ready", "7");
    expect((await eventually(async () => { const d = await web(7); return !d.pr.draft && d; })).pr.draft).toBe(false);
    // plugin edits and closes, web sees
    pr("Carol <c@x>", "edit", "7", "--body", "Edited in the terminal")
    pr("Carol <c@x>", "close", "7", "-m", "Not now");
    const closed = await eventually(async () => { const d = await web(7); return d.pr.state === "closed" && d; });
    expect(closed.pr).toMatchObject({ body: "Edited in the terminal", closed_by: "Carol" });
    expect(closed.pr.closed_at).toBeGreaterThan(0);
    expect(closed.comments.map((c: any) => c.body)).toContain("Not now");
    // web reopens, plugin sees
    await send(app.base, "POST", `${api}/prs/7/reopen`, {});
    expect(pr("Carol <c@x>", "list")).toContain("#7");
    // both refuse a draft
    await send(app.base, "POST", `${api}/prs/7/draft`, { draft: true });
    expect(prFails("Carol <c@x>", "merge", "7", "--force")).toContain("draft");
    expect((await send(app.base, "POST", `${api}/prs/7/merge`, { force: true })).status).toBe(409);
    pr("Carol <c@x>", "ready", "7");
  });

  it("merges: one PR with the plugin, one through the web, each seen by the other", async () => {
    // PR 6 (plugin-made): approve from someone else, merge in the web.
    pr("Erin <e@x>", "review", "6", "--approve", "-m", "fixed");
    await eventually(async () => (await web(6)).reviews.some((r: any) => r.who === "Erin"));
    // Dan's earlier "changes" is replaced by his own approval; no checks run, so policy needs force.
    pr("Dan Dev <d@x>", "review", "6", "--approve");
    const blocked = await send(app.base, "POST", `${api}/prs/6/merge`, {});
    expect(blocked.status).toBe(409);
    expect((await send(app.base, "POST", `${api}/prs/6/merge`, { force: true })).status).toBe(200);
    expect(pr("Carol <c@x>", "list", "--state", "merged")).toContain("#6");
    const merged = JSON.parse(pr("Carol <c@x>", "view", "6", "--json")).pr;
    expect(merged).toMatchObject({ state: "merged", merged_by: "Wanda Web" });
    expect(merged.merged_commit).toHaveLength(64);
    expect((await json(app.base, `${api}/blob?ref=main&path=hello.txt`)).text).toBe("hello\n");

    // PR 7 (web-made): merge with the plugin.
    pr("Erin <e@x>", "review", "7", "--approve");
    expect(prFails("Carol <c@x>", "merge", "7")).toContain("required checks");
    pr("Carol <c@x>", "merge", "7", "--force");
    const after = await eventually(async () => { const d = await web(7); return d.pr.state === "merged" && d; });
    expect(after.pr).toMatchObject({ merged_by: "Carol" });
    expect(after.pr.merged_commit).toHaveLength(64);
    expect((await json(app.base, `${api}/blob?ref=main&path=bye.txt`)).text).toBe("bye\n");
  });

  it("line comments, replies and threads: each side reads what the other wrote, and they agree on outdated", async () => {
    const id = (notes = (await send(app.base, "POST", `${api}/prs`, { title: "Notes", head: "topic/notes", base: "main" })).body.pr.id);
    const head = (await web(id)).headCommit;
    const cliView = () => JSON.parse(pr("Carol <c@x>", "view", String(id), "--json"));

    // web: a line comment, then a reply to it from the plugin
    const root = (await send(app.base, "POST", `${api}/prs/${id}/comments`, { body: "from web", path: "notes.txt", line: 2 })).body.comment;
    pr("Carol <c@x>", "comment", String(id), "-m", "from cli", "--reply-to", root.id);
    // plugin: a line comment of its own, and a plain one; web replies to the plain one
    pr("Dan Dev <d@x>", "comment", String(id), "-m", "cli line", "--path", "notes.txt", "--line", "4");
    pr("Dan Dev <d@x>", "comment", String(id), "-m", "plain");
    const view = cliView();
    const byBody = (cs: any[], b: string) => cs.find((c) => c.body === b);
    const plain = byBody(view.comments, "plain");
    const rep = (await send(app.base, "POST", `${api}/prs/${id}/comments`, { body: "web reply", in_reply_to: plain.file })).body.comment;

    const seen = await eventually(async () => { const d = await web(id); return d.comments.length === 5 && d; });
    const anchor = { path: "notes.txt", side: "new", commit: head };
    expect(byBody(seen.comments, "from web")).toMatchObject({ ...anchor, line: 2, thread: root.id, outdated: false });
    expect(byBody(seen.comments, "from cli")).toMatchObject({ ...anchor, line: 2, in_reply_to: root.id, thread: root.id, who: "Carol" });
    expect(byBody(seen.comments, "cli line")).toMatchObject({ ...anchor, line: 4, who: "Dan Dev" });
    expect(byBody(seen.comments, "web reply")).toMatchObject({ in_reply_to: plain.file, thread: plain.file });
    const after = cliView();
    expect(byBody(after.comments, "web reply")).toMatchObject({ in_reply_to: plain.file, thread: plain.file, file: rep.id });
    expect(byBody(after.comments, "from web")).toMatchObject({ ...anchor, line: 2, who: "Wanda Web", thread: root.id });
    expect(pr("Carol <c@x>", "view", String(id))).toContain(`notes.txt:2 (new)`);

    // both refuse the same bad anchors, and a reply with an anchor
    expect(prFails("Carol <c@x>", "comment", String(id), "-m", "x", "--path", "README.md", "--line", "1")).toContain("not changed");
    expect(prFails("Carol <c@x>", "comment", String(id), "-m", "x", "--path", "notes.txt", "--line", "9")).toContain("no line 9");
    expect(prFails("Carol <c@x>", "comment", String(id), "-m", "x", "--reply-to", "1700000000000-nobody")).toContain("no comment");
    expect(prFails("Carol <c@x>", "comment", String(id), "-m", "x", "--reply-to", root.id, "--line", "1")).toContain("takes its parent");
    expect((await send(app.base, "POST", `${api}/prs/${id}/comments`, { body: "x", path: "README.md", line: 1 })).status).toBe(400);
    expect((await send(app.base, "POST", `${api}/prs/${id}/comments`, { body: "x", path: "notes.txt", line: 9 })).status).toBe(400);

    // a push that changes line 4 only: both call the same comments outdated
    pushTo(store, "topic/notes", { "notes.txt": "one\ntwo\nthree\nFOUR\n" });
    const moved = await eventually(async () => { const d = await web(id); return d.headCommit !== head && d; });
    const outdated = (d: any) => d.comments.filter((c: any) => c.outdated).map((c: any) => c.body);
    expect(outdated(moved)).toEqual(["cli line"]);
    expect(outdated(cliView())).toEqual(["cli line"]);
  });

  it("approvals are of a commit in both: each counts the other's, and a push makes them stale in both", async () => {
    const id = notes;
    const head = (await web(id)).headCommit;
    pr("Erin <e@x>", "review", String(id), "--approve");
    await send(app.base, "POST", `${api}/prs/${id}/review`, { verdict: "approve", comment: "the author's own does not count" });
    const d = await eventually(async () => { const x = await web(id); return x.reviews.some((r: any) => r.who === "Erin") && x; });
    expect(d.reviews.find((r: any) => r.who === "Erin")).toMatchObject({ commit: head, stale: false });
    expect(metaJson(store, `prs/${id}/reviews/Wanda-Web.json`)).toMatchObject({ commit: head, verdict: "approve", who: "Wanda Web" });
    expect(d.readiness.problems.join()).not.toContain("approvals");
    expect(JSON.parse(pr("Carol <c@x>", "view", String(id), "--json")).problems.join()).not.toContain("approvals");

    pushTo(store, "topic/notes", { "notes.txt": "one\ntwo\nthree\nFOUR\nfive\n" });
    const moved = await eventually(async () => { const x = await web(id); return x.headCommit !== head && x; });
    expect(moved.readiness.problems).toContain("0 of 1 approvals (1 on an older commit)");
    const cli = JSON.parse(pr("Carol <c@x>", "view", String(id), "--json"));
    expect(cli.problems).toContain("0 of 1 approvals (1 on an older commit)");
    expect(cli.reviews.map((r: any) => r.stale)).toEqual([true, true]);
    expect(moved.reviews.map((r: any) => r.stale)).toEqual([true, true]);
    // The plugin approves the new head; the web counts it.
    pr("Erin <e@x>", "review", String(id), "--approve");
    const again = await eventually(async () => { const x = await web(id); return x.reviews.some((r: any) => r.who === "Erin" && !r.stale) && x; });
    expect(again.readiness.problems.join()).not.toContain("approvals");
  });

  it("a PR claimed for merging by either is refused by both", async () => {
    const id = notes;
    const rec = metaJson(store, `prs/${id}.json`);
    pailIn(store, ["put", "_meta", "-m", "claim", "--file", `prs/${id}.json=-`], "Wanda Web <w@x>", JSON.stringify({ ...rec, state: "merging", merging: { by: "Wanda-Web", commit: (await web(id)).headCommit } }));
    expect(prFails("Carol <c@x>", "close", String(id))).toContain(`PR ${id} is being merged`);
    expect(prFails("Carol <c@x>", "edit", String(id), "--title", "x")).toContain("being merged");
    const r = await send(app.base, "POST", `${api}/prs/${id}/close`, {});
    expect(r.status).toBe(409);
    expect(r.body.error).toBe(`PR ${id} is being merged by Wanda-Web`);
    expect(JSON.parse(pr("Carol <c@x>", "view", String(id), "--json")).problems[0]).toBe("pull request is being merged by Wanda-Web");
    // The plugin can take it over and finish it; the web then sees the merge.
    pr("Carol <c@x>", "merge", String(id), "--force", "--takeover");
    const merged = await eventually(async () => { const x = await web(id); return x.pr.state === "merged" && x; });
    expect(merged.pr).toMatchObject({ merged_by: "Carol" });
    expect(merged.pr.merging).toBeUndefined();
    expect(merged.pr.merged_commit).toHaveLength(64);
  });
});
