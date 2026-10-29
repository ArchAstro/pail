import { readdirSync, existsSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import { spawn, type ChildProcess } from "node:child_process";
import { createInterface } from "node:readline";
import { EventEmitter } from "node:events";
import { Cache } from "./cache.js";
import type { Config, RepoConfig } from "./config.js";
import { PailRunner } from "./pail.js";
import { Repo } from "./repo.js";
import type { RefEvent } from "../shared/types.js";

/** Candidate repositories under a bucket prefix or directory. */
export async function discover(pail: PailRunner, d: { store: string; env?: Record<string, string> }): Promise<RepoConfig[]> {
  const base = d.store.replace(/\/+$/, "");
  let names: string[] = [];
  if (base.startsWith("file://")) {
    const dir = fileURLToPath(base);
    if (existsSync(dir)) {
      names = readdirSync(dir).filter((n) => statSync(join(dir, n)).isDirectory() && existsSync(join(dir, n, "refs")));
    }
  } else {
    // Listing a bucket is the cloud CLI's job; pail has no `list repos` command.
    const [cmd, args] = base.startsWith("gs://") ? ["gcloud", ["storage", "ls", base + "/"]] : base.startsWith("s3://") ? ["aws", ["s3", "ls", base + "/"]] : ["", []];
    if (cmd) {
      const out = await new Promise<string>((res) => {
        const p = spawn(cmd as string, args as string[], { env: { ...process.env, ...(d.env ?? {}) } });
        let s = "";
        p.stdout.on("data", (b) => (s += b));
        p.on("error", () => res(""));
        p.on("close", () => res(s));
      });
      names = out
        .split("\n")
        .map((l) => l.trim().replace(/^PRE\s+/, "").replace(/\/$/, "").split("/").pop() ?? "")
        .filter(Boolean);
    }
  }
  const found: RepoConfig[] = [];
  await Promise.all(
    names.map(async (name) => {
      const repo = { name, store: `${base}/${name}`, env: d.env };
      try {
        await pail.run(repo, ["branch", "--json"], { timeoutMs: 20_000 });
        found.push(repo);
      } catch { /* not a pail repository */ }
    }),
  );
  return found.sort((a, b) => a.name.localeCompare(b.name));
}

/** One `pail events` process per repository that someone is watching. */
export class EventHub extends EventEmitter {
  private procs = new Map<string, { p: ChildProcess; subs: number; stop?: NodeJS.Timeout }>();
  constructor(private pail: PailRunner, private bin: string, private pollMs: number) { super(); this.setMaxListeners(0); }

  watch(repo: Repo): () => void {
    let e = this.procs.get(repo.name);
    if (!e) {
      e = { p: this.spawn(repo), subs: 0 };
      this.procs.set(repo.name, e);
    }
    if (e.stop) clearTimeout(e.stop);
    e.stop = undefined;
    e.subs++;
    const entry = e;
    return () => {
      entry.subs--;
      if (entry.subs <= 0) {
        entry.stop = setTimeout(() => {
          entry.p.kill();
          this.procs.delete(repo.name);
        }, 30_000);
        entry.stop.unref();
      }
    };
  }

  private spawn(repo: Repo): ChildProcess {
    const p = spawn(this.bin, ["--store", repo.cfg.store, "-P", "events", "--interval-ms", String(this.pollMs)], {
      env: this.pail.env(repo.cfg),
      stdio: ["ignore", "pipe", "ignore"],
    });
    createInterface({ input: p.stdout! }).on("line", (line) => {
      try {
        const e = JSON.parse(line);
        repo.invalidateRefs();
        const ev: RefEvent = { repo: repo.name, branch: e.branch, commit: e.commit, old: e.old, at: e.at };
        this.emit("ref", ev);
      } catch { /* not an event line */ }
    });
    p.on("close", () => {
      const cur = this.procs.get(repo.name);
      if (cur && cur.p === p) {
        // Restart with a pause so a bad store doesn't spin.
        const t = setTimeout(() => cur.subs > 0 && this.procs.set(repo.name, { ...cur, p: this.spawn(repo) }), 5000);
        t.unref();
      }
    });
    return p;
  }

  close() {
    for (const e of this.procs.values()) e.p.kill();
    this.procs.clear();
  }
}

export class Registry {
  pail: PailRunner;
  cache: Cache;
  hub: EventHub;
  private repos = new Map<string, Repo>();
  private discovered: { at: number; list: RepoConfig[] } | null = null;

  constructor(public cfg: Config) {
    this.pail = new PailRunner(cfg.pail, 8, cfg.author, cfg.cacheDir, cfg.localCache);
    this.cache = new Cache(cfg.cacheMb * 1024 * 1024);
    this.hub = new EventHub(this.pail, cfg.pail, cfg.pollMs);
    for (const r of cfg.repos) this.add(r);
  }

  private add(r: RepoConfig) {
    if (!this.repos.has(r.name)) this.repos.set(r.name, new Repo(r, this.pail, this.cache, this.cfg.actions, this.cfg.author));
  }

  async list(): Promise<Repo[]> {
    if (this.cfg.discover.length && (!this.discovered || Date.now() - this.discovered.at > 60_000)) {
      const lists = await Promise.all(this.cfg.discover.map((d) => discover(this.pail, d)));
      this.discovered = { at: Date.now(), list: lists.flat() };
      for (const r of this.discovered.list) this.add(r);
    }
    const names = new Set([...this.cfg.repos.map((r) => r.name), ...(this.discovered?.list ?? []).map((r) => r.name)]);
    return [...names].map((n) => this.repos.get(n)!);
  }

  async get(name: string): Promise<Repo | undefined> {
    if (!this.repos.has(name)) await this.list();
    return this.repos.get(name);
  }

  /** Read the first page of each configured repository in the background. */
  warmAll() { for (const r of this.cfg.repos) void this.get(r.name).then((x) => x?.warm()); }

  close() { this.hub.close(); }
}
