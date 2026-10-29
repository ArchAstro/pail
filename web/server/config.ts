import { readFileSync, existsSync } from "node:fs";
import { homedir } from "node:os";
import { delimiter, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export interface RepoConfig {
  name: string;
  store: string;
  description?: string;
  /** Bearer token for a `pail serve` https store. Prefer `tokenEnv`. */
  token?: string;
  /** Name of the environment variable holding the token. */
  tokenEnv?: string;
  /** Extra environment for this repo's pail processes (e.g. AWS_PROFILE). */
  env?: Record<string, string>;
}

export interface DiscoverConfig { store: string; description?: string; env?: Record<string, string> }

export interface Config {
  port: number;
  host: string;
  pail: string;
  actions: boolean;
  author?: string;
  cacheMb: number;
  /** Where bare clones live: pail's local content cache, so remote reads are warm. */
  cacheDir: string;
  /** "auto": remote stores only (a file:// store is already local). */
  localCache: "auto" | "on" | "off";
  pollMs: number;
  repos: RepoConfig[];
  discover: DiscoverConfig[];
}

const NAME = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

export function parseStoreList(s: string): RepoConfig[] {
  return s
    .split(/[,\n]/)
    .map((x) => x.trim())
    .filter(Boolean)
    .map((x) => {
      const i = x.indexOf("=");
      if (i > 0 && !/^[a-z0-9+.-]+:\/\//i.test(x.slice(0, i))) return { name: x.slice(0, i), store: x.slice(i + 1) };
      const last = x.replace(/\/+$/, "").split("/").pop() ?? "repo";
      return { name: last.replace(/\.pail$/, ""), store: x };
    });
}

/** `pail` on PATH, else a build in the pail checkout this server sits in, else plain `pail`. */
export function defaultPail(env: NodeJS.ProcessEnv = process.env): string {
  for (const dir of (env.PATH ?? "").split(delimiter)) if (dir && existsSync(join(dir, "pail"))) return "pail";
  let dir = dirname(fileURLToPath(import.meta.url));
  for (let i = 0; i < 4; i++, dir = dirname(dir))
    for (const p of ["target/release/pail", "target/debug/pail"]) if (existsSync(join(dir, p))) return join(dir, p);
  return "pail";
}

/** Defaults < config file < environment < argv. */
export function loadConfig(argv: string[] = [], env: NodeJS.ProcessEnv = process.env, cwd = process.cwd()): Config {
  let file: string | undefined = env.PAIL_WEB_CONFIG;
  const extraRepos: RepoConfig[] = [];
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--config") file = argv[++i];
    else if (argv[i] === "--store") extraRepos.push(...parseStoreList(argv[++i] ?? ""));
    else if (argv[i] === "--port") env = { ...env, PAIL_WEB_PORT: argv[++i] };
    else if (argv[i] === "--actions") env = { ...env, PAIL_WEB_ACTIONS: "1" };
  }
  if (!file && existsSync(resolve(cwd, "pail-web.config.json"))) file = resolve(cwd, "pail-web.config.json");
  const raw = file ? JSON.parse(readFileSync(resolve(cwd, file), "utf8")) : {};
  const cfg: Config = {
    port: Number(env.PAIL_WEB_PORT ?? raw.port ?? 4747),
    host: env.PAIL_WEB_HOST ?? raw.host ?? "127.0.0.1",
    pail: env.PAIL_BIN ?? raw.pail ?? defaultPail(env),
    actions: env.PAIL_WEB_ACTIONS ? /^(1|true|yes|on)$/i.test(env.PAIL_WEB_ACTIONS) : Boolean(raw.actions),
    author: env.PAIL_WEB_AUTHOR ?? raw.author,
    cacheMb: Number(raw.cacheMb ?? 128),
    cacheDir: env.PAIL_WEB_CACHE_DIR ?? raw.cacheDir ?? join(env.XDG_CACHE_HOME ?? join(homedir(), ".cache"), "pail-web"),
    localCache: (env.PAIL_WEB_LOCAL_CACHE ?? raw.localCache ?? "auto") as Config["localCache"],
    pollMs: Number(raw.pollMs ?? 2000),
    repos: [...(raw.repos ?? [])],
    discover: [...(raw.discover ?? [])],
  };
  if (env.PAIL_WEB_REPOS) cfg.repos.push(...parseStoreList(env.PAIL_WEB_REPOS));
  if (env.PAIL_WEB_DISCOVER) cfg.discover.push(...env.PAIL_WEB_DISCOVER.split(",").filter(Boolean).map((store) => ({ store: store.trim() })));
  cfg.repos.push(...extraRepos);
  if (!cfg.repos.length && !cfg.discover.length && env.PAIL_STORE) cfg.repos.push(...parseStoreList(env.PAIL_STORE));
  const seen = new Set<string>();
  for (const r of cfg.repos) {
    if (!NAME.test(r.name)) throw new Error(`repo name "${r.name}" must match ${NAME}`);
    if (!r.store) throw new Error(`repo "${r.name}" has no store`);
    if (seen.has(r.name)) throw new Error(`repo "${r.name}" is listed twice`);
    seen.add(r.name);
  }
  return cfg;
}

/** Store URL safe to show: no credentials in the userinfo or query. */
export function redactStore(store: string): string {
  try {
    const u = new URL(store);
    u.username = "";
    u.password = "";
    u.search = "";
    return u.toString().replace(/\/$/, store.endsWith("/") ? "/" : "");
  } catch {
    return store;
  }
}
