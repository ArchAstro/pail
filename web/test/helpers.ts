import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import type { AddressInfo } from "node:net";
import type { Server } from "node:http";
import { createApp } from "../server/app";
import { loadConfig } from "../server/config";

export interface Booted { base: string; close: () => Promise<void>; stores: string }

/** The API on an ephemeral port, over the demo repository. */
/** A private copy of the demo repository, for tests that write. */
export function freshDemo(): string {
  const dir = mkdtempSync(join(tmpdir(), "pail-web-rw-"));
  execFileSync("node", [join(import.meta.dirname, "..", "scripts", "make-demo.mjs"), dir], { env: process.env, stdio: "pipe" });
  return `file://${join(dir, "store")}`;
}

export async function boot(extra: Record<string, unknown> = {}, store = process.env.PAIL_WEB_TEST_STORE!): Promise<Booted> {
  const cfg = loadConfig([], { PAIL_BIN: process.env.PAIL_BIN, PAIL_WEB_REPOS: `tidepool=${store}`, PAIL_WEB_PORT: "0" } as NodeJS.ProcessEnv);
  Object.assign(cfg, extra);
  const { app, registry } = createApp(cfg);
  const server: Server = await new Promise((res) => {
    const s = app.listen(0, "127.0.0.1", () => res(s));
  });
  const { port } = server.address() as AddressInfo;
  return {
    base: `http://127.0.0.1:${port}`,
    stores: store,
    close: () => new Promise<void>((r) => { registry.close(); server.close(() => r()); }),
  };
}

export async function json<T = any>(base: string, path: string): Promise<T> {
  const res = await fetch(base + path);
  if (!res.ok) throw new Error(`${path}: ${res.status} ${await res.text()}`);
  return res.json() as Promise<T>;
}

/** The pail binary the tests use. */
export const PAIL = () => process.env.PAIL_BIN!;

/** The environment for pail commands that act as `who`, with a private global config. */
export function pailEnv(who = "Tess Tester <tess@example.com>", extra: Record<string, string> = {}): NodeJS.ProcessEnv {
  return { ...process.env, PAIL_AUTHOR: who, PAIL_CONFIG_GLOBAL: join(tmpdir(), "pail-web-test-global-config"), ...extra };
}

/**
 * Publish `branch` off main in `store`, holding one commit per entry of `commits`
 * ([message, {path: text}]). Made in a throwaway working copy, as a person would.
 */
export function makeBranch(store: string, branch: string, commits: [string, Record<string, string>][]): void {
  const dir = mkdtempSync(join(tmpdir(), "pail-web-wc-"));
  const wc = join(dir, "wc");
  const run = (args: string[]) => execFileSync(PAIL(), args, { cwd: dir, env: pailEnv(), stdio: "pipe" });
  run(["clone", store, wc]);
  const inWc = (args: string[]) => execFileSync(PAIL(), args, { cwd: wc, env: pailEnv(), stdio: "pipe" });
  inWc(["switch", "-c", branch]);
  for (const [msg, files] of commits) {
    for (const [p, text] of Object.entries(files)) {
      mkdirSync(dirname(join(wc, p)), { recursive: true });
      writeFileSync(join(wc, p), text);
    }
    inWc(["add", "-A"]);
    inWc(["commit", "-m", msg]);
  }
  rmSync(dir, { recursive: true, force: true });
}

/** Poll until `fn` returns a truthy value (the server re-reads refs every few seconds at most). */
export async function eventually<T>(fn: () => Promise<T | false | undefined | null>, ms = 20000): Promise<T> {
  const end = Date.now() + ms;
  let last: unknown;
  while (Date.now() < end) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) { last = e; }
    await new Promise((r) => setTimeout(r, 150));
  }
  throw new Error(`timed out waiting${last ? `: ${last}` : ""}`);
}

export async function send(base: string, method: string, path: string, body?: unknown): Promise<{ status: number; body: any }> {
  const res = await fetch(base + path, { method, headers: { "Content-Type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
  return { status: res.status, body: await res.json().catch(() => ({})) };
}
