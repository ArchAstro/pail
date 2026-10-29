// End to end: the real server over the demo repository (built with the real pail binary),
// and the real React app loading each main page through it.
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { App } from "../src/App";
import { clearApiMemo } from "../src/api";
import { boot, json, type Booted } from "./helpers";

let srv: Booted;
const realFetch = globalThis.fetch;

beforeAll(async () => {
  srv = await boot();
  Element.prototype.scrollIntoView = () => {};
  // jsdom's AbortSignal is not Node's: drop it.
  vi.stubGlobal("fetch", (input: any, init: any = {}) => realFetch(typeof input === "string" && input.startsWith("/") ? srv.base + input : input, { ...init, signal: undefined }));
});
afterAll(async () => {
  vi.unstubAllGlobals();
  await srv.close();
});

const open = (path: string) => {
  clearApiMemo();
  return render(<MemoryRouter initialEntries={[path]}><App /></MemoryRouter>);
};
const T = { timeout: 15000 };

describe("each main page loads", () => {
  it("repository list", async () => {
    open("/");
    expect(await screen.findByRole("heading", { name: "Repositories" }, T)).toBeInTheDocument();
    expect(await screen.findByText("tidepool", { selector: ".repos__name" }, T)).toBeInTheDocument();
    expect(await screen.findByText(/6 branches/, undefined, T)).toBeInTheDocument();
  });

  it("repo home: README rendered, file tree, branches, activity, open PRs", async () => {
    open("/tidepool");
    expect(await screen.findByRole("heading", { name: "tidepool", level: 1 }, T)).toBeInTheDocument();
    expect(await screen.findByRole("link", { name: "src" }, T)).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "Why" }, T)).toBeInTheDocument();
    expect(await screen.findByRole("link", { name: /#2 Heartbeat claims/ }, T)).toBeInTheDocument();
    expect(await screen.findByText("pr/lease-heartbeat", undefined, T)).toBeInTheDocument();
    expect(await screen.findByText("Add CI and merge policy", undefined, T)).toBeInTheDocument();
  });

  it("code: a directory, then a file with line numbers", async () => {
    open("/tidepool/tree/main/src");
    expect(await screen.findByRole("link", { name: "claim.rs" }, T)).toBeInTheDocument();
  });

  it("code: a file with line numbers", async () => {
    open("/tidepool/blob/main/src/claim.rs");
    expect(await screen.findByText(/claim_next/, undefined, T)).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Line 1" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Raw" })).toHaveAttribute("href", expect.stringContaining("/api/repos/tidepool/raw?ref=main&path=src%2Fclaim.rs"));
  });

  it("code on a slashed branch name", async () => {
    open("/tidepool/blob/feature/retry/src/retry.rs");
    expect(await screen.findByText(/with_jitter/, undefined, T)).toBeInTheDocument();
  });

  it("commits: maintenance hidden, then shown; squash links", async () => {
    const user = userEvent.setup();
    open("/tidepool/commits/main");
    expect(await screen.findByText("Squashed from 3 commits", undefined, T)).toBeInTheDocument();
    expect(screen.getAllByText("Rewritten from").length).toBeGreaterThan(0);
    expect(screen.queryByText("maintenance")).toBeNull();
    await user.click(screen.getByRole("checkbox", { name: /maintenance/ }));
    expect(await screen.findByText("maintenance", undefined, T)).toBeInTheDocument();
  });

  it("commit page with a diff", async () => {
    const { commits } = await json(srv.base, "/api/repos/tidepool/log?ref=main&limit=20");
    const squash = commits.find((c: any) => c.rewrittenFrom.length > 1);
    open(`/tidepool/commit/${squash.id}`);
    expect(await screen.findByRole("heading", { name: "Add capped, jittered backoff" }, T)).toBeInTheDocument();
    expect(await screen.findByText("Squashed from 3 commits", undefined, T)).toBeInTheDocument();
    expect(await screen.findByText(/2 files changed/, undefined, T)).toBeInTheDocument();
    expect(screen.getAllByText("src/retry.rs").length).toBeGreaterThan(0);
  });

  it("branches", async () => {
    open("/tidepool/branches");
    expect(await screen.findByRole("heading", { name: "Branches" }, T)).toBeInTheDocument();
    expect(await screen.findByRole("link", { name: "pr/lease-heartbeat" }, T)).toBeInTheDocument();
    expect(await screen.findByText("default", undefined, T)).toBeInTheDocument();
  });

  it("pull requests: filters", async () => {
    const user = userEvent.setup();
    open("/tidepool/pulls");
    expect(await screen.findByRole("link", { name: "Heartbeat claims so slow jobs keep their lease" }, T)).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "Fix typo in design notes" })).toBeNull();
    await user.click(screen.getByRole("tab", { name: /Merged/ }));
    expect(await screen.findByRole("link", { name: "Move exhausted jobs to dead/" }, T)).toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: /Closed/ }));
    expect(await screen.findByRole("link", { name: "Fix typo in design notes" }, T)).toBeInTheDocument();
  });

  it("pull request: description, reviews, failing check with its log, merge policy, and the diff", async () => {
    const user = userEvent.setup();
    open("/tidepool/pull/2");
    expect(await screen.findByRole("heading", { name: /Heartbeat claims/, level: 1 }, T)).toBeInTheDocument();
    expect(await screen.findByText("Not ready to merge", undefined, T)).toBeInTheDocument();
    expect(await screen.findByText(/requested changes/, undefined, T)).toBeInTheDocument();
    expect(await screen.findByText(/heartbeat_extends/, undefined, T)).toBeInTheDocument(); // failure log is open by default
    expect(screen.getAllByText("required").length).toBe(2);
    expect(screen.getByText(/Read-only/)).toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Changes" }));
    expect((await screen.findAllByText("src/lease.rs", undefined, T)).length).toBeGreaterThan(0);
    await user.click(screen.getByRole("tab", { name: /Commits/ }));
    expect(await screen.findByText("Lease: test expiry", undefined, T)).toBeInTheDocument();
  });

  it("checks", async () => {
    open("/tidepool/checks");
    expect(await screen.findByRole("heading", { name: "Checks", level: 1 }, T)).toBeInTheDocument();
    const list = await screen.findByText("web-types", undefined, T);
    expect(list).toBeInTheDocument();
    expect(await screen.findByText("Branches with results", undefined, T)).toBeInTheDocument();
  });

  it("finds a file with `t` and opens it", async () => {
    const user = userEvent.setup();
    open("/tidepool");
    await screen.findByRole("link", { name: "src" }, T);
    await user.keyboard("t");
    await user.type(await screen.findByRole("combobox"), "claim");
    await user.click(await screen.findByRole("option", { name: /src\/claim\.rs/ }, T));
    expect(await screen.findByText(/claim_next/, undefined, T)).toBeInTheDocument();
  });

  it("unknown repository", async () => {
    open("/nope");
    expect(await screen.findByText(/no repository "nope"/, undefined, T)).toBeInTheDocument();
  });
});
