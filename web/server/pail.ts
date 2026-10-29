import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import type { RepoConfig } from "./config.js";

export class PailError extends Error {
  constructor(public code: number | null, message: string, public args: string[]) {
    super(message);
  }
}

/** Runs the `pail` CLI against one store, a few at a time. */
export class PailRunner {
  private active = 0;
  private queue: (() => void)[] = [];
  spawned = 0;

  private bare = new Map<string, Promise<string | null>>();

  constructor(private bin = "pail", private limit = 8, private author?: string, private cacheDir?: string, private localCache: "auto" | "on" | "off" = "auto") {}

  /**
   * A bare clone of the store in the cache directory. `pail --repo <bare>` keeps pages and
   * blobs it has read, so a second read of a big repository's tree costs 0.3 s, not 20 s.
   * Null when the store is local already, or the cache is off or cannot be made.
   */
  private async bareFor(repo: Pick<RepoConfig, "store" | "token" | "tokenEnv" | "env">): Promise<string | null> {
    if (!this.cacheDir || this.localCache === "off") return null;
    if (this.localCache === "auto" && repo.store.startsWith("file://")) return null;
    let p = this.bare.get(repo.store);
    if (!p) {
      p = (async () => {
        const dir = join(this.cacheDir!, createHash("sha256").update(repo.store).digest("hex").slice(0, 16) + ".pail");
        try {
          if (!existsSync(join(dir, "config"))) {
            mkdirSync(this.cacheDir!, { recursive: true });
            await this.exec(["clone", "--bare", repo.store, dir], this.env(repo), { timeoutMs: 60_000 });
          }
          return dir;
        } catch {
          return null;
        }
      })();
      this.bare.set(repo.store, p);
    }
    return p;
  }

  /** Like `run`, but through the local cache when there is one (for reads of immutable data). */
  async runCached(repo: Pick<RepoConfig, "store" | "token" | "tokenEnv" | "env">, args: string[], opts: { stdin?: Buffer | string; timeoutMs?: number } = {}): Promise<Buffer> {
    const dir = await this.bareFor(repo);
    if (!dir) return this.run(repo, args, opts);
    await this.acquire();
    try {
      return await this.exec(["--repo", dir, "-P", ...args], this.env(repo), opts);
    } finally {
      this.release();
    }
  }

  env(repo: Pick<RepoConfig, "token" | "tokenEnv" | "env">): NodeJS.ProcessEnv {
    const env: NodeJS.ProcessEnv = { ...process.env, ...(repo.env ?? {}), NO_COLOR: "1" };
    if (this.author) env.PAIL_AUTHOR = this.author;
    const token = repo.token ?? (repo.tokenEnv ? process.env[repo.tokenEnv] : undefined);
    if (token) {
      // pail runs the helper as `sh -c '<cmd> get'`; the token stays out of argv.
      env.PAIL_WEB_TOKEN = token;
      env.PAIL_CREDENTIAL_HELPER = 'printf %s "$PAIL_WEB_TOKEN" #';
    }
    return env;
  }

  async run(repo: Pick<RepoConfig, "store" | "token" | "tokenEnv" | "env">, args: string[], opts: { stdin?: Buffer | string; timeoutMs?: number } = {}): Promise<Buffer> {
    await this.acquire();
    try {
      return await this.exec(["--store", repo.store, "-P", ...args], this.env(repo), opts);
    } finally {
      this.release();
    }
  }

  /** No store: for `pail --version` and discovery probes. */
  exec(args: string[], env: NodeJS.ProcessEnv, opts: { stdin?: Buffer | string; timeoutMs?: number } = {}): Promise<Buffer> {
    this.spawned++;
    return new Promise((resolve, reject) => {
      const p = spawn(this.bin, args, { env, stdio: ["pipe", "pipe", "pipe"] });
      const out: Buffer[] = [];
      const err: Buffer[] = [];
      const timer = setTimeout(() => p.kill("SIGKILL"), opts.timeoutMs ?? 120_000);
      p.stdout.on("data", (b) => out.push(b));
      p.stderr.on("data", (b) => err.push(b));
      p.on("error", (e) => {
        clearTimeout(timer);
        reject(new PailError(null, `cannot run ${this.bin}: ${e.message}`, args));
      });
      p.on("close", (code) => {
        clearTimeout(timer);
        if (code === 0) resolve(Buffer.concat(out));
        else {
          const msg = Buffer.concat(err).toString().trim() || Buffer.concat(out).toString().trim() || `pail exited ${code}`;
          reject(new PailError(code, msg, args));
        }
      });
      p.stdin.on("error", () => {});
      p.stdin.end(opts.stdin);
    });
  }

  private acquire(): Promise<void> {
    if (this.active < this.limit) {
      this.active++;
      return Promise.resolve();
    }
    return new Promise((r) => this.queue.push(() => { this.active++; r(); }));
  }

  private release() {
    this.active--;
    this.queue.shift()?.();
  }
}
