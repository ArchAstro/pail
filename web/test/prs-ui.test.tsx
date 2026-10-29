// The new pages in a browser-like environment, over a real server and the real pail binary:
// compare and open, then comment, edit, draft, close and reopen, and the same pages read-only.
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { App } from "../src/App";
import { clearApiMemo } from "../src/api";
import { boot, freshDemo, json, makeBranch, type Booted } from "./helpers";

let rw: Booted;
let ro: Booted;
let current = "";
const realFetch = globalThis.fetch;

beforeAll(async () => {
  const store = freshDemo();
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
