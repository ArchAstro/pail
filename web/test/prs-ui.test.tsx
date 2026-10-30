// The new pages in a browser-like environment, over a real server and the real pail binary:
// compare and open, then comment, edit, draft, close and reopen, and the same pages read-only.
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { App } from "../src/App";
import { clearApiMemo } from "../src/api";
import { boot, eventually, freshDemo, json, makeBranch, pushTo, type Booted } from "./helpers";

let rw: Booted;
let ro: Booted;
let current = "";
let store = "";
const realFetch = globalThis.fetch;

beforeAll(async () => {
  store = freshDemo();
  makeBranch(store, "topic/ui", [["Add the ui file\n\nFirst paragraph of the body.", { "ui.txt": "ui\n" }], ["Tweak the ui file", { "ui.txt": "ui2\n" }]]);
  makeBranch(store, "topic/other", [["Other change", { "other.txt": "o\n" }]]);
  rw = await boot({ actions: true, author: "Una UI <una@example.com>", pollMs: 200 }, store);
  ro = await boot({}, store);
  current = rw.base;
  Element.prototype.scrollIntoView = () => {};
  vi.stubGlobal("fetch", (input: any, init: any = {}) => realFetch(typeof input === "string" && input.startsWith("/") ? current + input : input, { ...init, signal: undefined }));
});
afterAll(async () => {
  vi.unstubAllGlobals();
  await rw.close();
  await ro.close();
});

const open = (path: string, server = rw) => {
  current = server.base;
  clearApiMemo();
  return render(<MemoryRouter initialEntries={[path]}><App /></MemoryRouter>);
};
const T = { timeout: 15000 };

describe("compare and open a pull request", () => {
  it("offers 'Compare & open pull request' on branches without one, only with actions on", async () => {
    open("/tidepool/branches");
    const row = (await screen.findByRole("link", { name: "topic/ui" }, T)).closest("li")!;
    expect(within(row).getByRole("link", { name: /Compare & open pull request/ })).toHaveAttribute("href", "/tidepool/compare?base=main&head=topic%2Fui");
    // A branch with an open PR links to it instead, and the default branch has no button.
    const withPr = screen.getByRole("link", { name: "pr/claims-docs" }).closest("li")!;
    expect(within(withPr).queryByRole("link", { name: /Compare/ })).toBeNull();
    expect(screen.getAllByRole("link", { name: /Compare & open pull request/ }).length).toBe(5); // every branch but main and those with an open PR
  });

  it("the same on the branch's file tree", async () => {
    open("/tidepool/tree/topic/ui");
    expect(await screen.findByRole("link", { name: /Compare & open pull request/ }, T)).toBeInTheDocument();
  });

  it("is read-only when actions are off: no buttons, a pointer to the CLI", async () => {
    open("/tidepool/branches", ro);
    await screen.findByRole("link", { name: "topic/ui" }, T);
    expect(screen.queryByRole("link", { name: /Compare/ })).toBeNull();
    document.body.innerHTML = "";
    open("/tidepool/compare?base=main&head=topic/ui", ro);
    expect(await screen.findByText(/Read-only/, undefined, T)).toBeInTheDocument();
    expect(screen.getByText(/pail pr create --head topic\/ui/)).toBeInTheDocument();
    expect(screen.queryByRole("form", { name: "New pull request" })).toBeNull();
    expect(await screen.findByText("Tweak the ui file", undefined, T)).toBeInTheDocument();
  });

  it("shows commits and the diff, then creates a draft and lands on its page", async () => {
    const user = userEvent.setup();
    open("/tidepool/compare?base=main&head=topic/ui");
    const form = await screen.findByRole("form", { name: "New pull request" }, T);
    // Defaults come from the first commit.
    expect(within(form).getByLabelText("Title")).toHaveValue("Add the ui file");
    expect(within(form).getByLabelText(/Description/)).toHaveValue("First paragraph of the body.");
    expect(screen.getByText("Tweak the ui file")).toBeInTheDocument();
    expect(screen.getByText(/changing/)).toHaveTextContent("1 file");
    await user.click(screen.getByRole("tab", { name: /Changes/ }));
    expect((await screen.findAllByText("ui.txt", undefined, T)).length).toBeGreaterThan(0);

    await user.clear(within(form).getByLabelText("Title"));
    await user.type(within(form).getByLabelText("Title"), "Add the UI");
    await user.click(within(form).getByRole("checkbox", { name: /Draft/ }));
    await user.click(within(form).getByRole("button", { name: "Create draft pull request" }));

    expect(await screen.findByRole("heading", { name: /Add the UI/, level: 1 }, T)).toBeInTheDocument();
    expect(screen.getByText("draft", { selector: ".pill" })).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "Draft" }, T)).toBeInTheDocument();
    const prs = await json(rw.base, "/api/repos/tidepool/prs");
    expect(prs.find((p: any) => p.title === "Add the UI")).toMatchObject({ draft: true, author: "Una UI", head: "topic/ui" });
  });

  it("the PR page: comment, edit, ready, close, reopen", async () => {
    const user = userEvent.setup();
    const { id } = (await json(rw.base, "/api/repos/tidepool/prs")).find((p: any) => p.title === "Add the UI");
    open(`/tidepool/pull/${id}`);
    await screen.findByRole("heading", { name: /Add the UI/, level: 1 }, T);

    // comment
    await user.type(await screen.findByPlaceholderText(/Leave a comment/), "Looks **good** to me");
    await user.click(screen.getByRole("button", { name: "Comment" }));
    expect(await screen.findByText("good", { selector: "strong" }, T)).toBeInTheDocument();
    expect(screen.getByLabelText("Comments")).toHaveTextContent("Una UI");

    // edit
    await user.click(screen.getByRole("button", { name: "Edit title and description" }));
    expect(screen.getByLabelText("Edit pull request")).toBeInTheDocument();
    const title = screen.getByLabelText("Title");
    await user.clear(title);
    await user.type(title, "Add the UI file");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("heading", { name: /Add the UI file/, level: 1 }, T)).toBeInTheDocument();

    // draft -> ready: the merge box stops saying draft
    expect(screen.getByRole("button", { name: /Merge with pail land/ })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "Ready for review" }));
    expect(await screen.findByRole("button", { name: "Convert to draft" }, T)).toBeInTheDocument();
    expect(screen.queryByText("draft", { selector: ".pill" })).toBeNull();
    expect(await screen.findByRole("heading", { name: "Not ready to merge" }, T)).toBeInTheDocument();

    // close, then reopen
    await user.click(screen.getByRole("button", { name: "Close pull request" }));
    expect(await screen.findByRole("heading", { name: "Closed" }, T)).toBeInTheDocument();
    expect(screen.getByText(/by Una UI/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Reopen pull request" }));
    expect(await screen.findByRole("button", { name: "Close pull request" }, T)).toBeInTheDocument();
  });

  it("the Changes tab: comment on a line, reply in the thread, see it outdated after a push", async () => {
    const user = userEvent.setup();
    const { id, headCommit } = (await json(rw.base, "/api/repos/tidepool/prs")).find((p: any) => p.title === "Add the UI file");
    open(`/tidepool/pull/${id}?tab=changes`);
    await user.click(await screen.findByRole("button", { name: "Comment on line 1" }, T));
    const form = screen.getByRole("form", { name: "Comment" });
    await user.type(within(form).getByRole("textbox"), "Why **ui2**?");
    await user.click(within(form).getByRole("button", { name: "Comment" }));
    const thread = (await screen.findByText("ui2", { selector: "strong" }, T)).closest(".thread") as HTMLElement;
    let d = await json(rw.base, `/api/repos/tidepool/prs/${id}`);
    const first = d.comments.filter((c: any) => c.path);
    expect(first).toHaveLength(1);
    expect(first[0]).toMatchObject({ path: "ui.txt", line: 1, side: "new", commit: headCommit, who: "Una UI" });

    await user.click(within(thread).getByRole("button", { name: "Reply" }));
    await user.type(within(thread).getByRole("textbox"), "Because it is the second draft");
    await user.click(within(within(thread).getByRole("form", { name: "Reply" })).getByRole("button", { name: "Reply" }));
    d = await eventually(async () => { const x = await json(rw.base, `/api/repos/tidepool/prs/${id}`); return x.comments.filter((c: any) => c.path).length === 2 && x; });
    expect(await screen.findByText("Because it is the second draft", { selector: "p" }, T)).toBeInTheDocument();
    const [root, reply] = d.comments.filter((c: any) => c.path);
    expect(reply).toMatchObject({ in_reply_to: root.id, thread: root.id, path: "ui.txt", line: 1 });
    // The conversation lists line threads only as a summary; replies and comments sit in their threads.
    await user.click(screen.getByRole("tab", { name: /Conversation/ }));
    expect(await screen.findByText(/1 line comment thread/, undefined, T)).toBeInTheDocument();
    expect(screen.queryByText("Why", { exact: false })).toBeNull();
    document.body.innerHTML = "";

    // A push that changes the line makes the thread outdated: it moves under the file header.
    pushTo(store, "topic/ui", { "ui.txt": "ui3\n" });
    await eventually(async () => (await json(rw.base, `/api/repos/tidepool/prs/${id}`)).comments.find((c: any) => c.path)?.outdated);
    open(`/tidepool/pull/${id}?tab=changes`);
    const group = await screen.findByText(/1 comment thread not shown on a line \(some outdated\)/, undefined, T);
    expect(document.querySelector(".inline")).toBeNull();
    await user.click(group);
    expect(await screen.findByText("Outdated", { selector: ".pill" }, T)).toBeInTheDocument();
    // A new comment on the new line is anchored on the new head.
    await user.click(await screen.findByRole("button", { name: "Comment on line 1" }, T));
    await user.type(screen.getByPlaceholderText(/Comment on line 1/), "fresh");
    await user.click(screen.getByRole("button", { name: "Comment" }));
    const after = await eventually(async () => { const x = await json(rw.base, `/api/repos/tidepool/prs/${id}`); return x.comments.some((c: any) => c.body === "fresh") && x; });
    expect(after.comments.find((c: any) => c.body === "fresh")).toMatchObject({ commit: after.headCommit, outdated: false });
    expect(after.headCommit).not.toBe(headCommit);
  });

  it("shows line threads and stale approvals read-only, with no forms", async () => {
    open("/tidepool/pull/2?tab=changes", ro);
    expect(await screen.findByText(/Should this saturate/, undefined, T)).toBeInTheDocument();
    expect(screen.getByText(/checked_add/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Comment on/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Reply" })).toBeNull();
    expect(document.querySelector(".inline")).not.toBeNull();
  });

  it("marks a stale approval, and a PR being merged has no buttons", async () => {
    const user = userEvent.setup();
    const { id, headCommit } = (await json(rw.base, "/api/repos/tidepool/prs")).find((p: any) => p.title === "Add the UI file");
    // The head moved since Bo approved the commit before it.
    const { execFileSync } = await import("node:child_process");
    const put = (path: string, rec: object, who = "Bo <b@x>") => execFileSync(process.env.PAIL_BIN!, ["--store", store, "put", "_meta", "-m", "t", "--file", `${path}=-`], { input: JSON.stringify(rec), env: { ...process.env, PAIL_AUTHOR: who } });
    put(`prs/${id}/reviews/bo.json`, { verdict: "approve", comment: "", at: 5, who: "Bo", commit: "c".repeat(64) });
    await eventually(async () => (await json(rw.base, `/api/repos/tidepool/prs/${id}`)).reviews.some((r: any) => r.who === "Bo"));
    open(`/tidepool/pull/${id}`);
    expect(await screen.findByText("approved an older commit", undefined, T)).toBeInTheDocument();
    expect(screen.getByText(/0 of 1 approval from someone other than the author \(1 on an older commit\)/)).toBeInTheDocument();
    expect(screen.getByText(/new commits since some approvals/)).toBeInTheDocument();
    document.body.innerHTML = "";

    const rec = (await json(rw.base, `/api/repos/tidepool/prs/${id}`)).pr;
    put(`prs/${id}.json`, { ...rec, state: "merging", merging: { by: "bo", commit: headCommit } });
    await eventually(async () => (await json(rw.base, `/api/repos/tidepool/prs/${id}`)).pr.state === "merging");
    open(`/tidepool/pull/${id}`);
    expect(await screen.findByRole("heading", { name: "Being merged" }, T)).toBeInTheDocument();
    expect(screen.getByText("merging", { selector: ".pill" })).toBeInTheDocument();
    expect(screen.getByText(/Being merged by/)).toHaveTextContent("pail pr merge");
    for (const name of [/Merge with pail land/, "Close pull request", "Convert to draft", "Reopen pull request", "Approve"]) expect(screen.queryByRole("button", { name })).toBeNull();
    // Comments are still welcome while it merges.
    await user.type(screen.getByPlaceholderText(/Leave a comment/), "merging now");
    await user.click(screen.getByRole("button", { name: "Comment" }));
    expect(await screen.findByText("merging now", undefined, T)).toBeInTheDocument();
    document.body.innerHTML = "";
    open("/tidepool/pulls");
    expect(await screen.findByText("merging", { selector: ".pill" }, T)).toBeInTheDocument();
  });

  it("shows comments and the draft state read-only, and says why a validation fails", async () => {
    const user = userEvent.setup();
    open("/tidepool/pull/2", ro);
    expect(await screen.findByText(/Is `ttl \/ 3` a hard rule|hard rule/, undefined, T)).toBeInTheDocument();
    expect(screen.getByText(/enforce it in the claim loop/)).toBeInTheDocument();
    expect(screen.queryByPlaceholderText(/Leave a comment/)).toBeNull();
    expect(screen.queryByRole("button", { name: /Edit title/ })).toBeNull();
    document.body.innerHTML = "";

    // A duplicate head/base is reported by the compare page rather than offered.
    open("/tidepool/compare?base=main&head=pr/lease-heartbeat");
    expect(await screen.findByText(/already exists/, undefined, T)).toBeInTheDocument();
    expect(screen.queryByRole("form", { name: "New pull request" })).toBeNull();
    expect(screen.getByRole("link", { name: "#2" })).toHaveAttribute("href", "/tidepool/pull/2");
    document.body.innerHTML = "";

    // A server-side refusal is shown inline.
    open("/tidepool/compare?base=main&head=topic/other");
    const form = await screen.findByRole("form", { name: "New pull request" }, T);
    await user.clear(within(form).getByLabelText("Title"));
    await user.type(within(form).getByLabelText("Title"), " ");
    expect(within(form).getByRole("button", { name: "Create pull request" })).toBeDisabled();
  });
});
