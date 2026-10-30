// The PR protocol's guarantees, over real pail repositories: records change only by
// compare-and-swap, a merge claims the PR and lands exactly the commit it judged, approvals are of
// a commit, and line comments are anchored, threaded and go outdated. The plugin counterpart is
// extensions/pail-pr (cross-checked in plugin.test.ts).
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { Cache } from "../server/cache";
import { PailRunner } from "../server/pail";
import { HttpError, Repo } from "../server/repo";
import { boot, eventually, freshDemo, json, makeBranch, metaJson, pailIn, pushTo, send, type Booted } from "./helpers";

const api = "/api/repos/tidepool";
const NOTES = "one\ntwo\nthree\nfour\nfive\n";

/** A runner whose caller can act just before a pail command runs (to race it). */
class HookRunner extends PailRunner {
  seen: string[][] = [];
  before?: (args: string[]) => Promise<void> | void;
  async run(repo: any, args: string[], opts: any = {}) {
    this.seen.push(args);
    await this.before?.(args);
    return super.run(repo, args, opts);
  }
}

describe("protocol", () => {
  let store: string;
  let app: Booted;
  let sam: Booted; // a second person
  let runner: HookRunner;
  let repo: Repo; // acts as Rae, through the hook runner
  const open = async (head: string, extra: object = {}) => (await send(app.base, "POST", `${api}/prs`, { title: head, head, base: "main", ...extra })).body.pr.id as number;
  const detail = (n: number, b = app) => json(b.base, `${api}/prs/${n}`);
  const raw = (n: number) => metaJson(store, `prs/${n}.json`);
  const mainHas = (path: string) => { try { pailIn(store, ["cat", `main:${path}`]); return true; } catch { return false; } };
  const tipOf = (branch: string) => pailIn(store, ["rev-parse", branch]).trim();
  const headMoved = async (n: number, from: string) => eventually(async () => { const d = await detail(n); return d.headCommit !== from && d; });
  /** Stands in for a merge that died after claiming the PR. */
  const claim = (n: number, by = "ghost") => pailIn(store, ["put", "_meta", "-m", "claim", "--file", `prs/${n}.json=-`], "Ghost <g@x>", JSON.stringify({ ...raw(n), state: "merging", merging: { by, commit: "0".repeat(64) } }));

  beforeAll(async () => {
    store = freshDemo();
    for (const b of ["cas", "casrace", "closed", "race", "claimed", "stale", "merged"]) makeBranch(store, `topic/${b}`, [[`Add ${b}`, { [`${b}.txt`]: `${b}\n` }]]);
    makeBranch(store, "topic/lines", [
      ["Notes and design", { "notes.txt": NOTES, "docs/design.md": "# Design\n\nShort now.\n" }],
      ["Tweak notes", { "notes.txt": "one\ntwo\nthree\nfour\nfive\nsix\n" }],
    ]);
    app = await boot({ actions: true, author: "Rae Reviewer <rae@example.com>", pollMs: 200 }, store);
    sam = await boot({ actions: true, author: "Sam Second <sam@example.com>", pollMs: 200 }, store);
    runner = new HookRunner(process.env.PAIL_BIN, 8, "Rae Reviewer <rae@example.com>");
    repo = new Repo({ name: "tidepool", store }, runner, new Cache(), true, "Rae Reviewer <rae@example.com>");
  });
  afterAll(async () => { await app.close(); await sam.close(); });

  describe("records change by compare-and-swap", () => {
    it("two concurrent edits both survive", async () => {
      const n = await open("topic/cas");
      for (let round = 0; round < 3; round++) {
        const rs = await Promise.all([
          send(app.base, "PATCH", `${api}/prs/${n}`, { title: `Title ${round}` }),
          send(sam.base, "PATCH", `${api}/prs/${n}`, { body: `Body ${round}` }),
        ]);
        expect(rs.map((r) => r.status)).toEqual([200, 200]);
        expect(raw(n)).toMatchObject({ title: `Title ${round}`, body: `Body ${round}` });
      }
    });

    it("a write against a record that changed after it was read is retried on the new one", async () => {
      const n = await open("topic/casrace");
      const puts = () => runner.seen.filter((a) => a[0] === "put" && a.includes("--expect")).length;
      const before = puts();
      let raced = false;
      // Someone else changes the record after we read it and before we write.
      runner.before = (args) => {
        if (raced || args[0] !== "put" || !args.some((a) => a.startsWith(`prs/${n}.json=`))) return;
        raced = true;
        runner.before = undefined;
        pailIn(store, ["put", "_meta", "-m", "other", "--file", `prs/${n}.json=-`], "Sam <s@x>", JSON.stringify({ ...raw(n), body: "someone else's words" }));
      };
      await repo.edit(n, { title: "Mine" });
      expect(raced).toBe(true);
      expect(puts() - before).toBe(2); // refused once (exit 4), then written against what was seen
      expect(raw(n)).toMatchObject({ title: "Mine", body: "someone else's words" });
    });

    it("an edit after a merge is refused and leaves the record alone", async () => {
      const n = await open("topic/merged");
      expect((await send(app.base, "POST", `${api}/prs/${n}/merge`, { force: true })).status).toBe(200);
      const before = pailIn(store, ["cat", `_meta:prs/${n}.json`]);
      for (const [m, p, b] of [["PATCH", `${api}/prs/${n}`, { title: "late" }], ["POST", `${api}/prs/${n}/close`, {}], ["POST", `${api}/prs/${n}/draft`, { draft: true }], ["POST", `${api}/prs/${n}/reopen`, {}]] as const) {
        const r = await send(app.base, m, p, b);
        expect(r.status, `${m} ${p}`).toBe(409);
        expect(r.body.error).toContain("merged");
      }
      expect(pailIn(store, ["cat", `_meta:prs/${n}.json`])).toBe(before);
      expect(raw(n)).toMatchObject({ state: "merged", merged_by: "Rae Reviewer" });
      expect(raw(n).merging).toBeUndefined();
      // No line comments on a merged PR: it has no commits of its own.
      expect((await send(app.base, "POST", `${api}/prs/${n}/comments`, { body: "x", path: "merged.txt", line: 1 })).status).toBe(400);
    });

    it("a closed PR refuses to merge, and nothing lands", async () => {
      const n = await open("topic/closed");
      expect((await send(app.base, "POST", `${api}/prs/${n}/close`, {})).status).toBe(200);
      const r = await send(app.base, "POST", `${api}/prs/${n}/merge`, { force: true });
      expect(r.status).toBe(409);
      expect(r.body.error).toContain("closed");
      expect(mainHas("closed.txt")).toBe(false);
      expect(raw(n).state).toBe("closed");
    });

    it("a PR being merged refuses close, edit, draft and reopen, and shows as merging", async () => {
      const n = await open("topic/claimed");
      claim(n, "ghost");
      for (const [m, p, b] of [["POST", `${api}/prs/${n}/close`, {}], ["PATCH", `${api}/prs/${n}`, { title: "x" }], ["POST", `${api}/prs/${n}/draft`, { draft: true }], ["POST", `${api}/prs/${n}/reopen`, {}]] as const) {
        const r = await send(app.base, m, p, b);
        expect(r.status, `${m} ${p}`).toBe(409);
        expect(r.body.error).toBe(`PR ${n} is being merged by ghost`);
      }
      const d = await eventually(async () => { const x = await detail(n); return x.pr.state === "merging" && x; });
      expect(d.pr.merging).toEqual({ by: "ghost", commit: "0".repeat(64) });
      expect(d.readiness.ready).toBe(false);
      expect(d.readiness.problems[0]).toBe("pull request is being merged by ghost");
      // Not even force merges it, but a takeover does.
      const r = await send(app.base, "POST", `${api}/prs/${n}/merge`, { force: true });
      expect(r.status).toBe(409);
      expect(r.body.error).toContain("being merged by ghost");
      expect((await send(app.base, "POST", `${api}/prs/${n}/merge`, { force: true, takeover: true })).status).toBe(200);
      expect(raw(n)).toMatchObject({ state: "merged", merged_by: "Rae Reviewer" });
      expect(raw(n).merging).toBeUndefined();
    });

    it("close and edit are refused while a merge is landing", async () => {
      const n = await open("topic/race");
      const seen: Record<string, string> = {};
      runner.before = async (args) => {
        if (args[0] !== "land") return;
        runner.before = undefined;
        // The claim is on the record by now.
        seen.state = raw(n).state;
        for (const [k, f] of [["close", () => repo.close(n)], ["edit", () => repo.edit(n, { title: "x" })]] as const) {
          seen[k] = await f().then(() => "allowed", (e) => (e instanceof HttpError ? `${e.status} ${e.message}` : String(e)));
        }
      };
      await repo.merge(n, true);
      expect(seen).toEqual({ state: "merging", close: `409 PR ${n} is being merged by Rae-Reviewer`, edit: `409 PR ${n} is being merged by Rae-Reviewer` });
      expect(raw(n)).toMatchObject({ state: "merged", merged_by: "Rae Reviewer", title: "topic/race" });
      expect(raw(n).merged_commit).toMatch(/^[0-9a-f]{64}$/);
    });
  });

  describe("merge lands exactly the commit it judged", () => {
    it("a push between judging and landing is refused, the claim released, and nothing lands", async () => {
      makeBranch(store, "topic/pushed", [["Add pushed", { "pushed.txt": "pushed\n" }]]);
      const n = await open("topic/pushed");
      const judged = (await detail(n)).headCommit;
      let claimed: any;
      runner.before = (args) => {
        if (args[0] !== "land") return;
        runner.before = undefined;
        claimed = raw(n);
        pushTo(store, "topic/pushed", { "sneaky.txt": "not reviewed\n" }, "Unreviewed push");
      };
      const landArgs = () => runner.seen.filter((a) => a[0] === "land").pop()!;
      await expect(repo.merge(n, true)).rejects.toMatchObject({ status: 409, message: expect.stringMatching(/^cannot merge: .*\(nothing landed\)$/) });
      expect(claimed).toMatchObject({ state: "merging", merging: { by: "Rae-Reviewer", commit: judged } });
      expect(landArgs()).toEqual(["land", "topic/pushed", "--stack", "--onto", "main", "--expect-tip", judged]);
      expect(raw(n).state).toBe("open");
      expect(raw(n).merging).toBeUndefined();
      expect(mainHas("pushed.txt")).toBe(false);
      expect(mainHas("sneaky.txt")).toBe(false);
      // Judged again on the new head, it merges, with the new commit.
      await repo.merge(n, true);
      expect(mainHas("sneaky.txt")).toBe(true);
      expect(raw(n).state).toBe("merged");
    });

    it("records the landed tip of the base, not the branch's", async () => {
      const n = await open("topic/stale", { draft: false });
      const base = tipOf("main");
      await repo.merge(n, true);
      const tip = tipOf("main");
      expect(tip).not.toBe(base);
      expect(raw(n).merged_commit).toBe(tip);
    });
  });

  describe("approvals are of a commit", () => {
    let n: number;
    beforeAll(async () => {
      makeBranch(store, "topic/approve", [["Add approve", { "approve.txt": "a\n" }]]);
      n = await open("topic/approve");
    });

    it("counts on the head it was given on, goes stale on a push, and is given again on the new head", async () => {
      const c1 = (await detail(n)).headCommit;
      expect((await send(sam.base, "POST", `${api}/prs/${n}/review`, { verdict: "approve", comment: "ok" })).status).toBe(200);
      let d = await eventually(async () => { const x = await detail(n); return x.reviews.length && x; });
      expect(d.reviews[0]).toMatchObject({ who: "Sam Second", commit: c1, stale: false });
      expect(d.readiness.problems.join()).not.toContain("approvals");
      expect((await json(app.base, `${api}/prs`)).find((p: any) => p.id === n).approvals).toBe(1);

      pushTo(store, "topic/approve", { "approve.txt": "a\nb\n" });
      d = await headMoved(n, c1);
      expect(d.reviews[0]).toMatchObject({ commit: c1, stale: true });
      expect(d.readiness.problems).toContain("0 of 1 approvals (1 on an older commit)");
      expect((await json(app.base, `${api}/prs`)).find((p: any) => p.id === n).approvals).toBe(0);

      await send(sam.base, "POST", `${api}/prs/${n}/review`, { verdict: "approve" });
      d = await eventually(async () => { const x = await detail(n); return !x.reviews[0].stale && x; });
      expect(d.reviews[0].commit).toBe(d.headCommit);
      expect(d.readiness.problems.join()).not.toContain("approvals");
    });

    it("takes the commit the reviewer read, if it is one of the PR's", async () => {
      const d = await detail(n);
      const first = d.commits[d.commits.length - 1].id;
      expect((await send(sam.base, "POST", `${api}/prs/${n}/review`, { verdict: "approve", commit: first.slice(0, 16) })).status).toBe(200);
      const after = await eventually(async () => { const x = await detail(n); return x.reviews[0].commit === first && x; });
      expect(after.reviews[0].stale).toBe(true);
      expect(after.readiness.problems).toContain("0 of 1 approvals (1 on an older commit)");
      const mainTip = tipOf("main");
      for (const bad of [mainTip, "nothex", 5]) expect((await send(sam.base, "POST", `${api}/prs/${n}/review`, { verdict: "approve", commit: bad })).status, String(bad)).toBe(400);
      expect((await detail(n)).reviews[0].commit).toBe(first);
    });

    it("never counts a review that names no commit", async () => {
      pailIn(store, ["put", "_meta", "-m", "old review", "--file", `prs/${n}/reviews/tom.json=-`], "Tom <t@x>", JSON.stringify({ verdict: "approve", comment: "", at: 5, who: "Tom" }));
      const d = await eventually(async () => { const x = await detail(n); return x.reviews.some((r: any) => r.who === "Tom") && x; });
      expect(d.reviews.find((r: any) => r.who === "Tom")).toMatchObject({ verdict: "approve", stale: true });
      expect(d.reviews.find((r: any) => r.who === "Tom").commit ?? null).toBeNull();
      expect(d.readiness.problems.some((p: string) => p.startsWith("0 of 1 approvals (2 on an older commit)"))).toBe(true);
    });
  });

  describe("line comments, replies and threads", () => {
    let n: number;
    let c1: string;
    let c2: string;
    const post = (body: object, b = app) => send(b.base, "POST", `${api}/prs/${n}/comments`, body);
    const comments = async () => (await detail(n)).comments as any[];
    const ids: Record<string, string> = {};
    beforeAll(async () => {
      n = await open("topic/lines");
      const d = await detail(n);
      c2 = d.headCommit;
      c1 = d.commits[1].id;
    });

    it("accepts a line comment on the new side, at the head by default", async () => {
      const r = await post({ body: "why two?", path: "notes.txt", line: 2 });
      expect(r.status, r.body.error).toBe(201);
      expect(r.body.comment).toMatchObject({ path: "notes.txt", line: 2, side: "new", commit: c2, thread: r.body.comment.id, outdated: false });
      ids.line2 = r.body.comment.id;
      expect(metaJson(store, `prs/${n}/comments/${ids.line2}.json`)).toEqual({ who: "Rae Reviewer", body: "why two?", at: expect.any(Number), path: "notes.txt", line: 2, side: "new", commit: c2 });
    });

    it("accepts the line of an older PR commit (by prefix), counting lines in that commit's file", async () => {
      expect((await post({ body: "no line 6 then", path: "notes.txt", line: 6, commit: c1 })).status).toBe(400);
      const ok = await post({ body: "line 5 then", path: "notes.txt", line: 5, commit: c1.slice(0, 12) });
      expect(ok.status).toBe(201);
      expect(ok.body.comment.commit).toBe(c1);
      expect((await post({ body: "line 6 now", path: "notes.txt", line: 6 })).status).toBe(201);
    });

    it("refuses bad anchors with 400", async () => {
      const mainTip = tipOf("main");
      const bad: [string, object][] = [
        ["commit not in the PR", { path: "notes.txt", line: 1, commit: mainTip }],
        ["not hex", { path: "notes.txt", line: 1, commit: "zz" }],
        ["path the PR does not change", { path: "README.md", line: 1 }],
        ["path that does not exist", { path: "nope.txt", line: 1 }],
        ["line 0", { path: "notes.txt", line: 0 }],
        ["past the end", { path: "notes.txt", line: 7 }],
        ["fractional", { path: "notes.txt", line: 1.5 }],
        ["textual line", { path: "notes.txt", line: "2" }],
        ["no line", { path: "notes.txt" }],
        ["no path", { line: 1 }],
        ["bad side", { path: "notes.txt", line: 1, side: "left" }],
        ["old side of an added file", { path: "notes.txt", line: 1, side: "old" }],
        ["new side past a shrunken file", { path: "docs/design.md", line: 8 }],
      ];
      for (const [label, extra] of bad) { const r = await post({ body: "x", ...extra }); expect(r.status, `${label}: ${r.body.error}`).toBe(400); }
      expect((await comments()).length).toBe(3);
    });

    it("anchors the old side to the file at the fork point", async () => {
      const r = await post({ body: "this went away", path: "docs/design.md", line: 8, side: "old" });
      expect(r.status).toBe(201);
      expect(r.body.comment).toMatchObject({ path: "docs/design.md", line: 8, side: "old", commit: c2 });
      ids.old = r.body.comment.id;
      expect((await post({ body: "x", path: "docs/design.md", line: 99, side: "old" })).status).toBe(400);
    });

    it("a reply takes its parent's anchor and thread, and an explicit anchor on a reply is refused", async () => {
      const r = await post({ body: "because two is next", in_reply_to: ids.line2 });
      expect(r.status).toBe(201);
      expect(r.body.comment).toMatchObject({ in_reply_to: ids.line2, path: "notes.txt", line: 2, side: "new", commit: c2, thread: ids.line2 });
      ids.reply = r.body.comment.id;
      const again = await post({ body: "ok", in_reply_to: ids.reply });
      expect(again.body.comment).toMatchObject({ thread: ids.line2, path: "notes.txt", line: 2 });
      for (const extra of [{ path: "notes.txt" }, { line: 1 }, { side: "new" }, { commit: c1 }]) expect((await post({ body: "x", in_reply_to: ids.line2, ...extra })).status, JSON.stringify(extra)).toBe(400);
    });

    it("refuses a reply to a comment that is not on the PR", async () => {
      expect((await post({ body: "x", in_reply_to: "1700000000000-nobody" })).status).toBe(404);
      expect((await post({ body: "x", in_reply_to: "../../prs/1" })).status).toBe(400);
      expect((await post({ body: "x", in_reply_to: 7 })).status).toBe(400);
      // A comment on another PR is not a parent here.
      expect((await send(app.base, "POST", `${api}/prs/2/comments`, { body: "x", in_reply_to: ids.line2 })).status).toBe(404);
    });

    it("replies to a plain top-level comment too", async () => {
      const root = (await post({ body: "general point" })).body.comment;
      expect(root.thread).toBe(root.id);
      const rep = (await post({ body: "agreed", in_reply_to: root.id })).body.comment;
      expect(rep).toMatchObject({ in_reply_to: root.id, thread: root.id });
      expect(rep.path).toBeUndefined();
      const all = await comments();
      expect(all.find((c) => c.id === rep.id)).toMatchObject({ thread: root.id, outdated: false });
      expect(all.map((c) => c.id)).toEqual([...all.map((c) => c.id)].sort());
    });

    it("gives concurrent comments unique ids, and loses none", async () => {
      const before = (await comments()).length;
      const rs = await Promise.all([...Array(12)].map((_, i) => post({ body: `parallel ${i}` }, i % 2 ? sam : app)));
      expect(rs.every((r) => r.status === 201)).toBe(true);
      expect(new Set(rs.map((r) => r.body.comment.id)).size).toBe(12);
      const all = await eventually(async () => { const x = await comments(); return x.length === before + 12 && x; });
      expect(all.filter((c) => c.body.startsWith("parallel")).map((c) => c.body).sort()).toEqual([...Array(12)].map((_, i) => `parallel ${i}`).sort());
    });

    it("marks a line comment outdated only when its line changed after a push", async () => {
      const find = async (id: string) => (await comments()).find((c) => c.id === id);
      const six = (await comments()).find((c) => c.body === "line 6 now")!.id;
      const five = (await comments()).find((c) => c.body === "line 5 then")!.id;
      expect((await find(ids.line2)).outdated).toBe(false);

      // Line 6 changes; lines 2 and 5 do not.
      const d0 = await detail(n);
      pushTo(store, "topic/lines", { "notes.txt": "one\ntwo\nthree\nfour\nfive\nSIX\n" }, "Shout six");
      await headMoved(n, d0.headCommit);
      expect((await find(six)).outdated).toBe(true);
      expect((await find(ids.line2)).outdated).toBe(false);
      expect((await find(five)).outdated).toBe(false); // read at c1, unchanged since
      expect((await find(ids.reply)).outdated).toBe(false);
      expect((await find(ids.old)).outdated).toBe(false); // the old side is the fork point's file

      // Now line 2 and the replies on it.
      const d1 = await detail(n);
      pushTo(store, "topic/lines", { "notes.txt": "one\nTWO\nthree\nfour\nfive\nSIX\n" }, "Shout two");
      await headMoved(n, d1.headCommit);
      expect((await find(ids.line2)).outdated).toBe(true);
      expect((await find(ids.reply)).outdated).toBe(true);
      expect((await find(five)).outdated).toBe(false);
      expect((await find(ids.old)).outdated).toBe(false);
      expect((await comments()).filter((c) => !c.path).every((c) => c.outdated === false)).toBe(true);
    });

    it("an unchanged line stays current across a push that only adds elsewhere", async () => {
      const d = await detail(n);
      pushTo(store, "topic/lines", { "extra.txt": "x\n" }, "Elsewhere");
      await headMoved(n, d.headCommit);
      const cs = await comments();
      expect(cs.find((c) => c.body === "line 5 then").outdated).toBe(false);
      // A new comment is anchored on the new head, and the old ones keep their commits.
      const r = await post({ body: "on the new head", path: "extra.txt", line: 1 });
      expect(r.status).toBe(201);
      expect(r.body.comment.commit).not.toBe(c2);
      expect((await comments()).find((c) => c.id === ids.line2)!.commit).toBe(c2);
    });

    it("is read-only with actions off", async () => {
      const ro = await boot({}, store);
      try {
        expect((await send(ro.base, "POST", `${api}/prs/${n}/comments`, { body: "x", path: "notes.txt", line: 1 })).status).toBe(403);
        const d = await detail(n, ro);
        expect(d.comments.find((c: any) => c.id === ids.line2)).toMatchObject({ path: "notes.txt", line: 2, thread: ids.line2 });
      } finally { await ro.close(); }
    });
  });
});
