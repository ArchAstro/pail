import { execFileSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
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
