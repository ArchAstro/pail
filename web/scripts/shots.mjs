#!/usr/bin/env node
// Captures the key pages with the agent-browser CLI: node scripts/shots.mjs [baseUrl] [repo]
import { execFileSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const base = process.argv[2] ?? "http://127.0.0.1:4747";
const repo = process.argv[3] ?? "tidepool";
const out = join(dirname(fileURLToPath(import.meta.url)), "..", "docs", "screenshots");
mkdirSync(out, { recursive: true });
const ab = (...a) => execFileSync("agent-browser", a, { encoding: "utf8" });

const only = process.argv.slice(4);
const shot = (name, path, { w = 1360, h = 900, theme, wait = 3000, full = false } = {}) => {
  if (only.length && !only.includes(name)) return;
  ab("set", "viewport", String(w), String(h));
  ab("open", base + path);
  if (theme) ab("eval", `localStorage.setItem('pail-theme','${theme}'); document.documentElement.dataset.theme='${theme}'`);
  else ab("eval", `localStorage.removeItem('pail-theme'); delete document.documentElement.dataset.theme`);
  const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
  sleep(wait);
  // Wait for every loading bar to go (up to 20 s), so no shot catches a half-loaded page.
  for (let i = 0; i < 20 && ab("eval", "(document.querySelectorAll('.loading, .skeleton').length === 0 && (document.querySelector('#main')?.innerText.length ?? 0) > 200) ? 'ready' : 'wait'").trim() !== '"ready"'; i++) sleep(1000);
  ab("screenshot", ...(full ? ["--full"] : []), join(out, `${name}.png`));
  console.log("captured", name);
};

const main = JSON.parse(execFileSync("curl", ["-s", `${base}/api/repos/${repo}/log?ref=main&limit=1`], { encoding: "utf8" })).commits[0].id;
const squash = JSON.parse(execFileSync("curl", ["-s", `${base}/api/repos/${repo}/log?ref=main&limit=6`], { encoding: "utf8" })).commits.find((c) => c.rewrittenFrom.length > 1)?.id ?? main;

shot("repo-home", `/${repo}`);
shot("file-view", `/${repo}/blob/main/src/claim.rs`);
shot("commit-diff", `/${repo}/commit/${squash}`);
shot("commits", `/${repo}/commits/main`);
shot("pull-request", `/${repo}/pull/2`);
shot("pull-request-changes", `/${repo}/pull/2?tab=changes`);
shot("checks", `/${repo}/checks`);
// With actions on (`PAIL_WEB_ACTIONS=1`): the compare page, and a draft PR with a comment. This
// opens the PR on the demo repository if there is none for the branch yet.
const cfg = JSON.parse(execFileSync("curl", ["-s", `${base}/api/config`], { encoding: "utf8" }));
if (cfg.actions) {
  const head = "fix/typo-in-design";
  const post = (path, body) => execFileSync("curl", ["-s", "-X", "POST", "-H", "Content-Type: application/json", "-d", JSON.stringify(body), `${base}/api/repos/${repo}${path}`], { encoding: "utf8" });
  shot("compare", `/${repo}/compare?base=main&head=${head}`);
  let pr = JSON.parse(execFileSync("curl", ["-s", `${base}/api/repos/${repo}/prs`], { encoding: "utf8" })).find((p) => p.head === head && p.state === "open");
  if (!pr) {
    pr = JSON.parse(post("/prs", { title: "Fix typo in design notes", body: "One word: *heartbeating* reads badly.\n\nOpened as a draft while I look for more of these.", head, base: "main", draft: true })).pr;
    post(`/prs/${pr.id}/comments`, { body: "Found two more in `docs/design.md`; I will fold them in before marking this ready." });
  }
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 5000); // let the "branch moved" toast go
  shot("pull-request-draft", `/${repo}/pull/${pr.id}`);
}
shot("repo-home-dark", `/${repo}`, { theme: "dark" });
shot("commit-diff-dark", `/${repo}/commit/${squash}`, { theme: "dark" });
shot("mobile-home", `/${repo}`, { w: 390, h: 844 });
shot("mobile-pr", `/${repo}/pull/2`, { w: 390, h: 844 });
ab("close");
