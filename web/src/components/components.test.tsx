import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import type { CommitSummary } from "../../shared/types";
import { CheckBadge, CheckStrip } from "./Checks";
import { CommitList } from "./CommitList";
import { DiffView } from "./DiffView";
import { Markdown } from "./Markdown";
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
