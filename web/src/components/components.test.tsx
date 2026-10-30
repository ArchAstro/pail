import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import type { CommitSummary, NewComment, PrComment } from "../../shared/types";
import { CheckBadge, CheckStrip } from "./Checks";
import { CommitList } from "./CommitList";
import { DiffView } from "./DiffView";
import { Markdown } from "./Markdown";
import { InlineContext, groupThreads, type InlineCtx } from "./Threads";
import { RefPicker } from "./RefPicker";

const wrap = (ui: React.ReactNode) => render(<MemoryRouter>{ui}</MemoryRouter>);
const commit = (o: Partial<CommitSummary> = {}): CommitSummary => ({ id: "a".repeat(64), parent: null, author: { name: "Ada", email: "a@x" }, date: new Date().toISOString(), subject: "Do a thing", body: "", maintenance: false, rewrittenFrom: [], files: 1, ...o });

describe("CheckBadge", () => {
  it("says what the state is and how many passed", () => {
    wrap(<CheckBadge rollup={{ state: "failure", passed: 1, failed: 1, pending: 0, total: 2 }} />);
    expect(screen.getByRole("img")).toHaveAccessibleName(/Checks failed: 1 of 2 passed, 1 failed/);
    expect(screen.getByText("1/2")).toBeInTheDocument();
  });
  it("renders nothing without results", () => {
    const { container } = wrap(<CheckBadge rollup={{ state: "none", passed: 0, failed: 0, pending: 0, total: 0 }} />);
    expect(container).toBeEmptyDOMElement();
  });
  it("draws one segment per check", () => {
    const { container } = wrap(<CheckStrip checks={[{ name: "a", state: "success", required: true, declared: true }, { name: "b", state: "pending", required: false, declared: true }]} />);
    expect(container.querySelectorAll("i")).toHaveLength(2);
  });
});

describe("CommitList", () => {
  it("marks maintenance commits and links squashed and rewritten sources", () => {
    const src = ["b".repeat(64), "c".repeat(64), "d".repeat(64)];
    wrap(<CommitList repo="r" commits={[
      commit({ id: "1".repeat(64), subject: "Squashed", rewrittenFrom: src }),
      commit({ id: "2".repeat(64), subject: "Rebased", rewrittenFrom: [src[0]], landedFrom: "pr/x" }),
      commit({ id: "3".repeat(64), subject: "pail maintenance: repack", maintenance: true }),
    ]} />);
    expect(screen.getByText("Squashed from 3 commits")).toBeInTheDocument();
    expect(screen.getByText("Rewritten from")).toBeInTheDocument();
    expect(screen.getByText("maintenance")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "pr/x" })).toHaveAttribute("href", "/r/tree/pr/x");
    expect(document.querySelector(`a[href="/r/commit/${src[0]}"]`)).not.toBeNull();
    expect(screen.getByRole("link", { name: "Squashed" })).toHaveAttribute("href", `/r/commit/${"1".repeat(64)}`);
  });
  it("groups commits under day headings", () => {
    wrap(<CommitList repo="r" commits={[commit(), commit({ id: "9".repeat(64), date: "2020-01-02T10:00:00Z" })]} />);
    expect(screen.getByText("Today")).toBeInTheDocument();
    expect(screen.getAllByRole("heading", { level: 3 })).toHaveLength(2);
  });
});

const PATCH = `diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 same
-before
+after
`;

describe("DiffView", () => {
  it("shows totals, then switches between unified and split and remembers the choice", async () => {
    const user = userEvent.setup();
    const { container } = wrap(<DiffView patch={PATCH} repo="r" blobRef="abc" />);
    expect(screen.getByText(/1 file changed/)).toBeInTheDocument();
    expect(container.querySelector(".hunk--split")).toBeNull();
    await user.click(screen.getByRole("tab", { name: "Split" }));
    expect(container.querySelector(".hunk--split")).not.toBeNull();
    expect(localStorage.getItem("pail-diff-mode")).toBe("split");
    await user.click(screen.getByRole("tab", { name: "Unified" }));
    expect(container.querySelector(".hunk--split")).toBeNull();
  });
  it("collapses and expands a file", async () => {
    const user = userEvent.setup();
    wrap(<DiffView patch={PATCH} repo="r" />);
    expect(screen.getByText("before")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Collapse" }));
    expect(screen.queryByText("before")).toBeNull();
    await user.click(screen.getAllByRole("button", { name: /Expand|Show/ })[0]);
    expect(screen.getByText("before")).toBeInTheDocument();
  });
  it("says so when there is nothing to show", () => {
    wrap(<DiffView patch="" repo="r" />);
    expect(screen.getByText("No file changes.")).toBeInTheDocument();
  });
});

// ---- comments on diff lines --------------------------------------------------------------------

const LINES = `diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
`;
const HEAD = "f".repeat(64);
const cm = (id: string, o: Partial<PrComment> = {}): PrComment => ({ id, who: "bo", body: `body of ${id}`, at: 1700000000, thread: id, outdated: false, ...o });

function inlineWith(comments: PrComment[], o: Partial<InlineCtx> = {}) {
  const post = vi.fn<(c: NewComment) => Promise<void>>(async () => {});
  const ctx: InlineCtx = { threads: groupThreads(comments), actions: true, anchorable: true, commit: HEAD, post, md: { repo: "r", ref: "main", dir: "" }, ...o };
  return { post, ui: <InlineContext.Provider value={ctx}><DiffView patch={LINES} repo="r" /></InlineContext.Provider> };
}

describe("comments on diff lines", () => {
  it("groups comments into threads by root, replies in id order", () => {
    const ts = groupThreads([cm("3", { thread: "1", in_reply_to: "1" }), cm("1"), cm("2"), cm("4", { thread: "1", in_reply_to: "3" })]);
    expect(ts.map((t) => [t.id, t.replies.map((r) => r.id)])).toEqual([["1", ["3", "4"]], ["2", []]]);
  });

  it("offers + on added, removed and context lines only with actions on, and sends the side, line and commit", async () => {
    const user = userEvent.setup();
    const { ui, post } = inlineWith([]);
    wrap(ui);
    expect(screen.getAllByRole("button", { name: /^Comment on/ }).map((b) => b.getAttribute("aria-label"))).toEqual(["Comment on line 1", "Comment on removed line 2", "Comment on line 2", "Comment on line 3"]);
    await user.click(screen.getByRole("button", { name: "Comment on removed line 2" }));
    const form = screen.getByRole("form", { name: "Comment" });
    expect(screen.getByPlaceholderText("Comment on line 2 (Markdown)")).toBeInTheDocument();
    await user.type(within(form).getByRole("textbox"), "gone?");
    await user.click(within(form).getByRole("button", { name: "Comment" }));
    expect(post).toHaveBeenCalledWith({ body: "gone?", path: "a.txt", line: 2, side: "old", commit: HEAD });
    await waitFor(() => expect(screen.queryByRole("form", { name: "Comment" })).toBeNull());

    await user.click(screen.getByRole("button", { name: "Comment on line 2" }));
    await user.type(screen.getByRole("textbox"), "ok");
    await user.click(screen.getByRole("button", { name: "Comment" }));
    expect(post).toHaveBeenLastCalledWith({ body: "ok", path: "a.txt", line: 2, side: "new", commit: HEAD });
  });

  it("comments on the right side of a split row from its own side", async () => {
    const user = userEvent.setup();
    const { ui, post } = inlineWith([]);
    wrap(ui);
    await user.click(screen.getByRole("tab", { name: "Split" }));
    await user.click(screen.getByRole("button", { name: "Comment on removed line 2" }));
    await user.type(screen.getByRole("textbox"), "left");
    await user.click(screen.getByRole("button", { name: "Comment" }));
    expect(post).toHaveBeenCalledWith(expect.objectContaining({ side: "old", line: 2 }));
    await user.click(screen.getByRole("button", { name: "Comment on line 3" }));
    await user.type(screen.getByRole("textbox"), "right");
    await user.click(screen.getByRole("button", { name: "Comment" }));
    expect(post).toHaveBeenLastCalledWith(expect.objectContaining({ side: "new", line: 3 }));
    localStorage.setItem("pail-diff-mode", "unified");
  });

  it("shows no + without actions, on a PR that cannot take new line comments, or without a commit", () => {
    for (const o of [{ actions: false }, { anchorable: false }, { commit: null }]) {
      const { unmount } = wrap(inlineWith([], o).ui);
      expect(screen.queryAllByRole("button", { name: /^Comment on/ })).toHaveLength(0);
      unmount();
    }
    wrap(<DiffView patch={LINES} repo="r" />);
    expect(screen.queryAllByRole("button", { name: /^Comment on/ })).toHaveLength(0);
  });

  it("renders a thread under its line, with the replies, and replies in its thread", async () => {
    const user = userEvent.setup();
    const root = cm("100", { path: "a.txt", line: 2, side: "new", commit: HEAD, body: "why TWO?" });
    const reply = cm("101", { who: "chen", body: "shouting", path: "a.txt", line: 2, side: "new", commit: HEAD, thread: "100", in_reply_to: "100" });
    const { ui, post } = inlineWith([root, reply]);
    const { container } = wrap(ui);
    const thread = container.querySelector('[data-thread="100"]') as HTMLElement;
    expect(thread).not.toBeNull();
    // Directly under the added line "TWO" (new 2), not under the removed one.
    const row = [...container.querySelectorAll(".row")].find((r) => r.textContent?.includes("TWO"))!;
    expect(row.nextElementSibling).toBe(thread.parentElement);
    await waitFor(() => expect(within(thread).getByText("why TWO?")).toBeInTheDocument());
    expect(within(thread).getByText("shouting")).toBeInTheDocument();
    expect(within(thread).queryByText("Outdated")).toBeNull();
    await user.click(within(thread).getByRole("button", { name: "Reply" }));
    await user.type(within(thread).getByRole("textbox"), "fair");
    await user.click(within(within(thread).getByRole("form", { name: "Reply" })).getByRole("button", { name: "Reply" }));
    expect(post).toHaveBeenCalledWith({ body: "fair", in_reply_to: "100" });
  });

  it("matches the old side to removed lines and keeps outdated or off-diff threads in a group under the file header", async () => {
    const user = userEvent.setup();
    const old = cm("200", { path: "a.txt", line: 2, side: "old", commit: HEAD, body: "this went" });
    const outdated = cm("201", { path: "a.txt", line: 3, side: "new", commit: "e".repeat(64), outdated: true, body: "stale point" });
    const far = cm("202", { path: "a.txt", line: 40, side: "new", commit: HEAD, body: "far away" });
    const other = cm("203", { path: "b.txt", line: 1, side: "new", commit: HEAD, body: "other file" });
    const { ui } = inlineWith([old, outdated, far, other]);
    const { container } = wrap(ui);
    const removed = [...container.querySelectorAll(".row--del")][0];
    await waitFor(() => expect(removed.nextElementSibling).toHaveTextContent("this went"));
    expect(container.querySelector('[data-thread="201"]')).toBeInTheDocument();
    const group = screen.getByText(/2 comment threads not shown on a line \(some outdated\)/);
    expect(group.closest("details")).not.toHaveAttribute("open");
    expect(container.querySelector(".inline [data-thread='201']")).toBeNull();
    await user.click(group);
    const t = container.querySelector("details [data-thread='201']") as HTMLElement;
    expect(within(t).getByText("Outdated")).toBeInTheDocument();
    expect(within(t).getByText("after, line 3")).toBeInTheDocument();
    expect(container.querySelector("[data-thread='202']")).toHaveTextContent("far away");
    expect(container.textContent).not.toContain("other file");
  });

  it("is read-only without actions: threads show, no Reply", async () => {
    const { ui } = inlineWith([cm("300", { path: "a.txt", line: 1, side: "new", commit: HEAD, body: "only reading" })], { actions: false });
    wrap(ui);
    expect(await screen.findByText("only reading")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Reply" })).toBeNull();
  });

  it("keeps the form and says why when the server refuses", async () => {
    const user = userEvent.setup();
    const { ui, post } = inlineWith([]);
    const { ApiError } = await import("../api");
    post.mockRejectedValueOnce(new ApiError(400, "a.txt has no line 1"));
    wrap(ui);
    await user.click(screen.getByRole("button", { name: "Comment on line 1" }));
    await user.type(screen.getByRole("textbox"), "hmm");
    await user.click(screen.getByRole("button", { name: "Comment" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("a.txt has no line 1");
    expect(screen.getByRole("textbox")).toHaveValue("hmm");
  });
});

describe("RefPicker", () => {
  const branches = [{ name: "main", commit: "a", isDefault: true }, { name: "pr/x", commit: "b", isDefault: false }, { name: "fix/y", commit: "c", isDefault: false }];
  it("filters with the keyboard and picks with Enter", async () => {
    const user = userEvent.setup();
    const pick = vi.fn();
    render(<RefPicker current="main" branches={branches} defaultBranch="main" onPick={pick} />);
    await user.click(screen.getByRole("button", { name: /Switch branch/ }));
    await user.keyboard("pr");
    expect(screen.getAllByRole("option")).toHaveLength(1);
    await user.keyboard("{Enter}");
    expect(pick).toHaveBeenCalledWith("pr/x");
  });
  it("accepts a pasted commit id", async () => {
    const user = userEvent.setup();
    const pick = vi.fn();
    render(<RefPicker current="main" branches={branches} defaultBranch="main" onPick={pick} />);
    await user.click(screen.getByRole("button"));
    await user.keyboard("f".repeat(64));
    await user.keyboard("{Enter}");
    expect(pick).toHaveBeenCalledWith("f".repeat(64));
  });
  it("closes on Escape", async () => {
    const user = userEvent.setup();
    render(<RefPicker current="main" branches={branches} defaultBranch="main" onPick={() => {}} />);
    await user.click(screen.getByRole("button"));
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("listbox")).toBeNull();
  });
});

describe("Markdown", () => {
  it("renders, strips scripts and handlers, and points relative links into the repo", async () => {
    wrap(<Markdown ctx={{ repo: "r", ref: "main", dir: "docs" }} text={'# Title\n\n[design](design.md) and [src](../src)\n\n<script>alert(1)</script><img src="x.png" onerror="alert(2)">\n\n[out](https://example.com)'} />);
    await waitFor(() => expect(screen.getByRole("heading", { name: "Title" })).toBeInTheDocument());
    expect(document.querySelector("script")).toBeNull();
    expect(document.querySelector("[onerror]")).toBeNull();
    expect(screen.getByRole("link", { name: "design" })).toHaveAttribute("href", "/r/blob/main/docs/design.md");
    expect(screen.getByRole("link", { name: "src" })).toHaveAttribute("href", "/r/tree/main/src");
    expect(document.querySelector("img")?.getAttribute("src")).toBe("/api/repos/r/raw?ref=main&path=docs%2Fx.png");
    const out = screen.getByRole("link", { name: "out" });
    expect(out).toHaveAttribute("rel", "noopener noreferrer");
    fireEvent.click(out);
  });
});
