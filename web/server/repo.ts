import { structuredPatch } from "diff";
import type { Cache } from "./cache.js";
import type { RepoConfig } from "./config.js";
import { PailError, type PailRunner } from "./pail.js";
import type {
  BlobView, BranchInfo, CheckResult, CheckRollup, CheckState, CommitSummary, LogPage, Policy,
  Compare, NewComment, NewPr, PrComment, PrDetail, PrDiff, PrRecord, PrSummary, Review, Side, TreeEntry, TreeListing,
} from "../shared/types.js";

export const META = "_meta";
const HEX64 = /^[0-9a-f]{64}$/;
const IMAGE = /\.(png|jpe?g|gif|webp|avif|ico|bmp|svg)$/i;
const MAX_TEXT = 1024 * 1024;
export const MAX_TITLE = 256;
export const MAX_BODY = 60_000;

/** A person's name as a file name, the way `pail pr` writes it: `Alice Smith` is `Alice-Smith`. */
export function slug(name: string): string {
  return name.replace(/[^A-Za-z0-9._-]+/g, "-").replace(/^[.-]+|[.-]+$/g, "").slice(0, 64) || "user";
}

/** A one-line title or a body, within the limits `pail pr` applies. */
export function checkText(label: string, v: unknown, max: number, oneLine: boolean, required = false): string {
  if (v === undefined || v === null) v = "";
  if (typeof v !== "string") throw new HttpError(400, `${label} must be text`);
  const t = oneLine ? v.trim() : v;
  if (oneLine && /[\r\n\0]/.test(t)) throw new HttpError(400, `${label} must be one line`);
  if (t.length > max) throw new HttpError(400, `${label} is too long (max ${max} characters)`);
  if (required && !t.trim()) throw new HttpError(400, `${label} is required`);
  return t;
}

export class HttpError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

interface RawReview { key: string; who: string; verdict: "approve" | "changes"; comment: string; at?: number; commit?: string | null }
type RawComment = Omit<PrComment, "thread" | "outdated">;

const COMMENT_ID = /^\d{13}-[A-Za-z0-9._-]+$/;
const MAX_TRIES = 50;

interface RawCommit {
  id: string; parent: string | null; author: string; author_date: string; committer: string;
  committer_date: string; message: string; meta: Record<string, string>; files: number; bytes: number;
}

export function parsePerson(s: string) {
  const m = /^(.*?)\s*<([^>]*)>\s*$/.exec(s);
  return m ? { name: m[1], email: m[2] } : { name: s, email: "" };
}

export function parseDate(s: string): string {
  const secs = Number(s.split(" ")[0]);
  return new Date((Number.isFinite(secs) ? secs : 0) * 1000).toISOString();
}

export function toSummary(c: RawCommit): CommitSummary {
  const [subject = "", ...rest] = c.message.replace(/\s+$/, "").split("\n");
  const from = c.meta["rewritten.from"];
  return {
    id: c.id,
    parent: c.parent,
    author: parsePerson(c.author),
    date: parseDate(c.committer_date),
    subject,
    body: rest.join("\n").trim(),
    maintenance: "pail.maintenance" in c.meta,
    rewrittenFrom: from ? from.split(/\s+/).filter(Boolean) : [],
    landedFrom: c.meta["landed.from"],
    files: c.files,
  };
}

/** Parse the output of `cat-file --batch`: `<id> blob <size>\n<bytes>\n` or `<name> missing\n`. */
export function parseBatch(buf: Buffer, names: string[]): Map<string, Buffer | null> {
  const out = new Map<string, Buffer | null>();
  let pos = 0;
  for (const name of names) {
    const nl = buf.indexOf(10, pos);
    if (nl < 0) break;
    const header = buf.subarray(pos, nl).toString();
    pos = nl + 1;
    const m = /^\S+ \S+ (\d+)$/.exec(header);
    if (!m) {
      out.set(name, null);
      continue;
    }
    const size = Number(m[1]);
    out.set(name, buf.subarray(pos, pos + size));
    pos += size + 1;
  }
  return out;
}

/** Like parseBatch, with the blob id the header carries: Map(name -> {id, data} | null). */
export function parseBatchIds(buf: Buffer, names: string[]): Map<string, { id: string; data: Buffer } | null> {
  const out = new Map<string, { id: string; data: Buffer } | null>();
  let pos = 0;
  for (const name of names) {
    const nl = buf.indexOf(10, pos);
    if (nl < 0) break;
    const m = /^(\S+) \S+ (\d+)$/.exec(buf.subarray(pos, nl).toString());
    pos = nl + 1;
    if (!m) {
      out.set(name, null);
      continue;
    }
    const size = Number(m[2]);
    out.set(name, { id: m[1], data: buf.subarray(pos, pos + size) });
    pos += size + 1;
  }
  return out;
}

function safeRef(ref: string) {
  if (!ref || ref.startsWith("-") || /[\s\0]/.test(ref) || ref.length > 300) throw new HttpError(400, `bad ref "${ref}"`);
  return ref;
}

export class Repo {
  constructor(public cfg: RepoConfig, private pail: PailRunner, private cache: Cache, private actionsOn = false, private author?: string) {}

  get name() { return this.cfg.name; }
  private k(...parts: (string | number)[]) { return `${this.name}\0${parts.join("\0")}`; }
  invalidateRefs() { this.cache.invalidate(this.k("refs")); }

  /** Straight to the store: for refs and history, which must be fresh or cannot use the cache. */
  private run(args: string[], stdin?: Buffer | string) {
    return this.pail.run(this.cfg, args, { stdin });
  }

  /** Through the local content cache: for trees, blobs and diffs, which never change. */
  private runCached(args: string[], stdin?: Buffer | string) {
    return this.pail.runCached(this.cfg, args, { stdin });
  }

  private async lines<T>(args: string[]): Promise<T[]> {
    const out = (await this.run(args)).toString();
    return out.split("\n").filter(Boolean).map((l) => JSON.parse(l) as T);
  }

  // ---- refs (the only mutable state) -----------------------------------------------------

  async refs(): Promise<{ default: string; branches: { name: string; commit: string }[] }> {
    return this.cache.ttl(this.k("refs"), 3000, async () => JSON.parse((await this.run(["branch", "--json"])).toString()));
  }

  async resolve(ref: string): Promise<string> {
    if (HEX64.test(ref)) return ref;
    safeRef(ref);
    const refs = await this.refs();
    const b = refs.branches.find((x) => x.name === ref);
    if (b) return b.commit;
    if (ref === "HEAD") return refs.branches.find((x) => x.name === refs.default)?.commit ?? this.notFound(ref);
    try {
      const out = (await this.run(["rev-parse", "--verify", ref])).toString().trim();
      if (HEX64.test(out)) return out;
    } catch { /* fall through */ }
    return this.notFound(ref);
  }

  private notFound(what: string): never {
    throw new HttpError(404, `not found: ${what}`);
  }

  /**
   * Start reading what a first visit shows (the root tree, README and latest commits) so the
   * page is ready when someone opens it. Once per tip; failures are ignored.
   */
  private warmed = new Set<string>();
  async warm(): Promise<void> {
    try {
      const refs = await this.refs();
      const tip = refs.branches.find((b) => b.name === refs.default)?.commit;
      if (!tip || this.warmed.has(tip)) return;
      this.warmed.add(tip);
      await Promise.allSettled([this.tree(tip), this.readme(tip), this.log({ ref: tip, limit: 30 })]);
    } catch { /* the page will show the error */ }
  }

  // ---- commits -----------------------------------------------------------------------------

  async commit(id: string): Promise<CommitSummary> {
    const h = await this.resolve(id);
    return this.cache.get(this.k("commit", h), async () => {
      const [c] = await this.lines<RawCommit>(["log", "--json", "--maintenance", "-n", "1", h]);
      if (!c) this.notFound(id);
      return toSummary(c);
    });
  }

  async log(opts: { ref: string; path?: string; limit?: number; skip?: number; maintenance?: boolean; range?: string }): Promise<LogPage> {
    const h = await this.resolve(opts.ref);
    const limit = Math.min(Math.max(opts.limit ?? 30, 1), 200);
    const skip = Math.max(opts.skip ?? 0, 0);
    const key = this.k("log", h, opts.path ?? "", limit, skip, opts.maintenance ? 1 : 0, opts.range ?? "");
    const page = await this.cache.get(key, async () => {
      const args = ["log", "--json", "-n", String(limit + 1), "--skip", String(skip)];
      if (opts.maintenance) args.push("--maintenance");
      args.push(opts.range ?? h);
      if (opts.path) args.push("--", opts.path);
      const raw = await this.lines<RawCommit>(args);
      return { commits: raw.slice(0, limit).map(toSummary), more: raw.length > limit } as LogPage;
    });
    return { more: page.more, commits: await this.withChecks(page.commits) };
  }

  /** Branch names pointing at each commit id, for decorating a log. */
  async decorate(commits: CommitSummary[]): Promise<CommitSummary[]> {
    const refs = await this.refs();
    const by = new Map<string, string[]>();
    for (const b of refs.branches) if (!b.name.startsWith("_")) by.set(b.commit, [...(by.get(b.commit) ?? []), b.name]);
    return commits.map((c) => (by.has(c.id) ? { ...c, refs: by.get(c.id) } : c));
  }

  // ---- trees and blobs -----------------------------------------------------------------------

  async tree(ref: string, path = ""): Promise<TreeListing> {
    const h = await this.resolve(ref);
    path = path.replace(/^\/+|\/+$/g, "");
    const entries = await this.cache.get(this.k("tree", h, path), async () => {
      // `ls -l` is half the work of `ls-tree -l` on a large directory (no Git ids to compute).
      const out = (await this.runCached(["ls", "-l", `${h}:${path}`])).toString();
      const list: TreeEntry[] = [];
      for (const line of out.split("\n")) {
        const m = /^(file|dir)\s+(\d+)\s+(.*)$/.exec(line);
        if (!m) continue;
        const dir = m[1] === "dir";
        // A directory's number is its total size, which says little in a listing: omit it.
        list.push({ mode: dir ? "040000" : "100644", type: dir ? "tree" : "blob", size: dir ? undefined : Number(m[2]), name: dir ? m[3].replace(/\/$/, "") : m[3] });
      }
      list.sort((a, b) => (a.type === b.type ? a.name.localeCompare(b.name) : a.type === "tree" ? -1 : 1));
      return list;
    }).catch((e) => {
      if (e instanceof PailError) throw new HttpError(404, `no such directory: ${path || "/"}`);
      throw e;
    });
    return { commit: h, path, entries };
  }

  async fileSize(h: string, path: string): Promise<number> {
    return this.cache.get(this.k("size", h, path), async () => Number((await this.runCached(["cat-file", "-s", `${h}:${path}`])).toString().trim()));
  }

  async raw(ref: string, path: string): Promise<{ commit: string; data: Buffer }> {
    const h = await this.resolve(ref);
    const data = await this.cache.get(this.k("blob", h, path), () => this.runCached(["cat-file", "-p", `${h}:${path}`])).catch((e) => {
      if (e instanceof PailError) throw new HttpError(404, `no such file: ${path}`);
      throw e;
    });
    return { commit: h, data };
  }

  async blob(ref: string, path: string): Promise<BlobView> {
    const h = await this.resolve(ref);
    path = path.replace(/^\/+/, "");
    let size: number;
    try { size = await this.fileSize(h, path); } catch { throw new HttpError(404, `no such file: ${path}`); }
    const image = IMAGE.test(path);
    if (size > MAX_TEXT || image) return { commit: h, path, size, binary: image, image, tooLarge: size > MAX_TEXT && !image };
    const { data } = await this.raw(h, path);
    const binary = data.subarray(0, 8000).includes(0);
    return { commit: h, path, size, binary, image: false, tooLarge: false, text: binary ? undefined : data.toString("utf8") };
  }

  async files(ref: string): Promise<string[]> {
    const h = await this.resolve(ref);
    return this.cache.get(this.k("files", h), async () => (await this.runCached(["ls", "-r", h])).toString().split("\n").filter(Boolean).slice(0, 200_000));
  }

  async readme(ref: string): Promise<{ path: string; text: string } | null> {
    const t = await this.tree(ref);
    const f = t.entries.find((e) => e.type === "blob" && /^readme(\.(md|markdown|txt))?$/i.test(e.name));
    if (!f || (f.size ?? 0) > MAX_TEXT) return null;
    const { data } = await this.raw(t.commit, f.name);
    return { path: f.name, text: data.toString("utf8") };
  }

  /** Many files of one commit in one process: Map(path -> bytes | null). */
  async readMany(h: string, paths: string[]): Promise<Map<string, Buffer | null>> {
    if (!paths.length) return new Map();
    const names = paths.map((p) => `${h}:${p}`);
    const out = await this.runCached(["cat-file", "--batch"], names.join("\n") + "\n");
    const got = parseBatch(out, names);
    return new Map(paths.map((p, i) => [p, got.get(names[i]) ?? null]));
  }

  async readJson<T>(h: string, path: string): Promise<T | null> {
    return this.cache.get(this.k("json", h, path), async () => {
      const m = await this.readMany(h, [path]);
      const b = m.get(path);
      if (!b) return null;
      try { return JSON.parse(b.toString()) as T; } catch { return null; }
    });
  }

  // ---- diffs ---------------------------------------------------------------------------------

  /**
   * A commit's diff against its parent. Built from the blobs of the changed paths rather than
   * `show -p`, which walks history (hundreds of sequential reads on a large repository).
   */
  async patch(id: string): Promise<{ patch: string; truncated: boolean }> {
    const h = await this.resolve(id);
    return this.cache.get(this.k("patch", h), async () => {
      const c = await this.commit(h);
      const out = (await this.runCached(["show", h, "--pretty=format:", "--name-only", "--no-renames"])).toString();
      const paths = [...new Set(out.split("\n").filter(Boolean))].sort();
      return this.diffPaths(c.parent, h, paths);
    });
  }

  private async diffPaths(from: string | null, to: string, paths: string[]): Promise<{ patch: string; truncated: boolean }> {
    const LIMIT = 300;
    const use = paths.slice(0, LIMIT);
    const [a, b] = await Promise.all([from ? this.readMany(from, use) : Promise.resolve(new Map<string, Buffer | null>()), this.readMany(to, use)]);
    let patch = "";
    for (const p of use) {
      const x = a.get(p) ?? null;
      const y = b.get(p) ?? null;
      if ((x && y && x.equals(y)) || (!x && !y)) continue;
      patch += formatFilePatch(p, x, y);
    }
    return { patch, truncated: paths.length > LIMIT };
  }

  /** A combined diff `from..to`, from the paths the commits between touch. */
  async rangeDiff(from: string, to: string): Promise<PrDiff> {
    return this.cache.get(this.k("rdiff", from, to), async () => {
      const out = (await this.run(["log", "--pretty=format:", "--name-only", "--no-renames", `${from}..${to}`])).toString();
      const paths = [...new Set(out.split("\n").map((l) => l.trim()).filter(Boolean))].sort();
      return { ...(await this.diffPaths(from, to, paths)), from, to };
    });
  }

  // ---- branches ------------------------------------------------------------------------------

  async branches(): Promise<{ default: string; branches: BranchInfo[] }> {
    const refs = await this.refs();
    const list = refs.branches.filter((b) => !b.name.startsWith("_")).slice(0, 300);
    const infos = await Promise.all(
      list.map(async (b): Promise<BranchInfo> => {
        const info: BranchInfo = { name: b.name, commit: b.commit, isDefault: b.name === refs.default };
        try {
          const c = await this.commit(b.commit);
          info.date = c.date; info.subject = c.subject; info.author = c.author.name;
          info.checks = await this.rollup(b.commit);
        } catch { /* leave bare */ }
        return info;
      }),
    );
    infos.sort((a, b) => Number(b.isDefault) - Number(a.isDefault) || (b.date ?? "").localeCompare(a.date ?? ""));
    return { default: refs.default, branches: infos };
  }

  // ---- _meta: checks, pull requests -----------------------------------------------------------

  async metaTip(): Promise<string | null> {
    const refs = await this.refs();
    return refs.branches.find((b) => b.name === META)?.commit ?? null;
  }

  private async lsRecursive(h: string, dir: string): Promise<string[]> {
    try {
      // `ls -r <rev>:<dir>` prints paths from the repository root.
      const lines = (await this.runCached(["ls", "-r", `${h}:${dir}`])).toString().split("\n").filter(Boolean);
      return lines.map((l) => (l.startsWith(dir + "/") ? l.slice(dir.length + 1) : l));
    } catch { return []; }
  }

  /** Commits that have any check result recorded: one listing per `_meta` tip. */
  private async checkedCommits(tip: string): Promise<Set<string>> {
    return this.cache.get(this.k("checkidx", tip), async () => {
      try {
        const out = (await this.runCached(["ls", `${tip}:checks`])).toString();
        return new Set(out.split("\n").map((s) => s.trim().replace(/\/$/, "")).filter(Boolean));
      } catch { return new Set<string>(); }
    });
  }

  async checkResults(commit: string): Promise<Record<string, Omit<CheckResult, "name" | "required" | "declared">>> {
    const tip = await this.metaTip();
    if (!tip || !(await this.checkedCommits(tip)).has(commit)) return {};
    return this.cache.get(this.k("checks", tip, commit), async () => {
      const files = (await this.lsRecursive(tip, `checks/${commit}`)).filter((p) => p.endsWith(".json") && !p.includes("/"));
      const got = await this.readMany(tip, files.map((f) => `checks/${commit}/${f}`));
      const out: Record<string, any> = {};
      for (const f of files) {
        const b = got.get(`checks/${commit}/${f}`);
        if (!b) continue;
        try { out[f.slice(0, -5)] = JSON.parse(b.toString()); } catch { /* skip malformed */ }
      }
      return out;
    });
  }

  async ci(commit: string): Promise<{ checks: Record<string, string>; required: string[] }> {
    const ci = await this.readJson<{ checks?: Record<string, string>; required?: string[] }>(commit, "pail.ci.json");
    const checks = ci?.checks ?? {};
    return { checks, required: ci?.required ?? Object.keys(checks) };
  }

  /** Results joined with the checks the commit declares, required first. */
  async checks(commit: string, required?: string[]): Promise<CheckResult[]> {
    const [results, ci] = await Promise.all([this.checkResults(commit), this.ci(commit).catch(() => ({ checks: {} as Record<string, string>, required: [] as string[] }))]);
    const req = new Set(required ?? ci.required);
    const names = new Set([...Object.keys(ci.checks), ...Object.keys(results), ...req]);
    const out: CheckResult[] = [];
    for (const name of names) {
      const r = results[name];
      out.push({
        ...(r ?? {}),
        name,
        state: ((r?.state as CheckState) ?? "pending"),
        recorded: Boolean(r),
        required: req.has(name),
        declared: name in ci.checks,
        command: ci.checks[name] ?? (r as any)?.command,
      } as CheckResult);
    }
    return out.sort((a, b) => Number(b.required) - Number(a.required) || a.name.localeCompare(b.name));
  }

  async rollup(commit: string): Promise<CheckRollup> {
    const results = Object.values(await this.checkResults(commit));
    return rollupOf(results.map((r) => r.state as CheckState));
  }

  private async withChecks(commits: CommitSummary[]): Promise<CommitSummary[]> {
    const tip = await this.metaTip();
    if (!tip) return commits;
    const idx = await this.checkedCommits(tip);
    return Promise.all(commits.map(async (c) => (idx.has(c.id) ? { ...c, checks: await this.rollup(c.id) } : c)));
  }

  async policy(base: string): Promise<Policy> {
    const h = await this.resolve(base).catch(() => null);
    if (!h) return { approvals: 1, required_checks: [] };
    const [pol, ci] = await Promise.all([this.readJson<Partial<Policy>>(h, "pail.pr.json"), this.ci(h)]);
    return { approvals: pol?.approvals ?? 1, required_checks: pol?.required_checks ?? ci.required };
  }

  private async prIndex(tip: string): Promise<{ records: PrRecord[]; reviews: Map<number, RawReview[]>; comments: Map<number, RawComment[]> }> {
    return this.cache.get(this.k("prindex", tip), async () => {
      const paths = await this.lsRecursive(tip, "prs");
      const prFiles = paths.filter((p) => /^\d+\.json$/.test(p));
      const revFiles = paths.filter((p) => /^\d+\/reviews\/[^/]+\.json$/.test(p));
      const comFiles = paths.filter((p) => /^\d+\/comments\/[^/]+\.json$/.test(p)).sort();
      const got = await this.readMany(tip, [...prFiles, ...revFiles, ...comFiles].map((p) => `prs/${p}`));
      const records: PrRecord[] = [];
      for (const f of prFiles) {
        try { records.push(JSON.parse(got.get(`prs/${f}`)!.toString())); } catch { /* skip */ }
      }
      const reviews = new Map<number, RawReview[]>();
      for (const f of revFiles) {
        const [n, , file] = f.split("/");
        try {
          const r = JSON.parse(got.get(`prs/${f}`)!.toString());
          const key = file.slice(0, -5);
          reviews.set(Number(n), [...(reviews.get(Number(n)) ?? []), { key, who: typeof r.who === "string" && r.who ? r.who : key, verdict: r.verdict, comment: r.comment ?? "", at: r.at, commit: typeof r.commit === "string" ? r.commit : null }]);
        } catch { /* skip */ }
      }
      const comments = new Map<number, RawComment[]>();
      for (const f of comFiles) {
        const [n, , file] = f.split("/");
        try {
          const c = JSON.parse(got.get(`prs/${f}`)!.toString());
          if (typeof c.body !== "string") continue;
          const id = file.slice(0, -5);
          const rec: RawComment = { id, who: typeof c.who === "string" && c.who ? c.who : id.replace(/^\d+-/, ""), body: c.body, at: c.at };
          if (typeof c.in_reply_to === "string") rec.in_reply_to = c.in_reply_to;
          if (typeof c.commit === "string") rec.commit = c.commit;
          if (typeof c.path === "string" && Number.isInteger(c.line) && (c.side === "new" || c.side === "old")) Object.assign(rec, { path: c.path, line: c.line, side: c.side });
          comments.set(Number(n), [...(comments.get(Number(n)) ?? []), rec]);
        } catch { /* skip */ }
      }
      records.sort((a, b) => b.id - a.id);
      return { records, reviews, comments };
    });
  }

  async prs(): Promise<PrSummary[]> {
    const tip = await this.metaTip();
    if (!tip) return [];
    const { records, reviews, comments } = await this.prIndex(tip);
    const refs = await this.refs();
    return Promise.all(
      records.map(async (pr) => {
        const headCommit = refs.branches.find((b) => b.name === pr.head)?.commit ?? null;
        const revs = (reviews.get(pr.id) ?? []).filter((r) => r.key !== slug(pr.author));
        return {
          ...pr,
          headCommit,
          checks: pr.state === "open" && headCommit ? await this.rollup(headCommit) : undefined,
          approvals: revs.filter((r) => r.verdict === "approve" && r.commit && r.commit === headCommit).length,
          changesRequested: revs.filter((r) => r.verdict === "changes").length,
          comments: comments.get(pr.id)?.length ?? 0,
        };
      }),
    );
  }

  /** `rec`: judge this record (one just read for a change) rather than the indexed one. */
  async pr(n: number, rec?: PrRecord): Promise<PrDetail> {
    const tip = await this.metaTip();
    const idx = tip ? await this.prIndex(tip) : null;
    const pr = rec ?? idx?.records.find((p) => p.id === n);
    if (!pr) throw new HttpError(404, `no pull request #${n}`);
    const refs = await this.refs();
    const headCommit = refs.branches.find((b) => b.name === pr.head)?.commit ?? null;
    const baseCommit = refs.branches.find((b) => b.name === pr.base)?.commit ?? null;
    const policy = await this.policy(pr.base);
    let commits: CommitSummary[] = [];
    if (headCommit && baseCommit && pr.state !== "merged") {
      commits = (await this.log({ ref: headCommit, range: `${baseCommit}..${headCommit}`, limit: 200 })).commits;
    } else if (pr.merged_commit) {
      commits = [await this.commit(pr.merged_commit).catch(() => null as any)].filter(Boolean);
    } else if (headCommit && baseCommit) {
      commits = (await this.log({ ref: headCommit, range: `${baseCommit}..${headCommit}`, limit: 200 })).commits;
    }
    const checkCommit = headCommit ?? pr.merged_commit ?? null;
    const checks = checkCommit ? await this.checks(checkCommit, policy.required_checks) : [];
    const reviews: Review[] = (idx?.reviews.get(n) ?? []).sort((a, b) => (a.at ?? 0) - (b.at ?? 0)).map(({ key, ...r }) => ({ ...r, byAuthor: key === slug(pr.author), stale: Boolean(headCommit && r.commit !== headCommit) }));
    // An approval is of the commit it was given on: a push makes it stale. No commit, no count.
    const approved = reviews.filter((r) => r.verdict === "approve" && !r.byAuthor);
    const approvals = approved.filter((r) => r.commit && r.commit === headCommit).length;
    const problems: string[] = [];
    if (pr.state === "merging") problems.push(`pull request is being merged by ${pr.merging?.by ?? "?"}`);
    else if (pr.state !== "open") problems.push(`pull request is ${pr.state}`);
    if (pr.draft) problems.push("pull request is a draft");
    if (!headCommit && pr.state === "open") problems.push(`branch ${pr.head} no longer exists`);
    const failing = policy.required_checks.filter((c) => checks.find((x) => x.name === c)?.state !== "success");
    if (failing.length) problems.push(`required checks not passing: ${failing.join(", ")}`);
    if (approvals < policy.approvals) problems.push(`${approvals} of ${policy.approvals} approvals${approved.length > approvals ? ` (${approved.length - approvals} on an older commit)` : ""}`);
    const comments = await this.markComments(pr, pr.state === "merged" || pr.state === "merging" ? null : headCommit, commits.map((c) => c.id), idx?.comments.get(n) ?? []);
    return { pr, headCommit, baseCommit, reviews, comments, commits, checks, policy, readiness: { ready: !problems.length, problems }, actions: this.actionsOn };
  }

  /**
   * Adds `thread` and `outdated` to a PR's comments. A line comment is outdated when the head has
   * moved since its commit and its line, on the new side, is not what it was there (changed, or
   * gone), or when its commit left the PR; never once the PR is merging or merged. The old side is the fork point's file, which a push
   * does not change.
   */
  private async markComments(pr: PrRecord, head: string | null, ids: string[], raw: RawComment[]): Promise<PrComment[]> {
    const byId = new Map(raw.map((c) => [c.id, c]));
    const threadOf = (c: RawComment) => {
      const seen = new Set<string>();
      while (c.in_reply_to && byId.has(c.in_reply_to) && !seen.has(c.id)) { seen.add(c.id); c = byId.get(c.in_reply_to)!; }
      return c.id;
    };
    const out: PrComment[] = raw.map((c) => ({ ...c, thread: threadOf(c), outdated: false }));
    const live = new Set(ids);
    const todo: PrComment[] = [];
    for (const c of out) {
      if (!c.commit || !head || c.commit === head || c.side !== "new") continue;
      if (!live.has(c.commit)) c.outdated = true;
      else todo.push(c);
    }
    if (todo.length) {
      const lineAt = new Map<string, string[] | null>();
      const revs = new Set([head!, ...todo.map((c) => c.commit!)]);
      await Promise.all([...revs].map(async (rev) => {
        const paths = [...new Set(todo.flatMap((c) => (rev === head || rev === c.commit ? [c.path!] : [])))];
        const got = await this.readMany(rev, paths);
        for (const p of paths) lineAt.set(`${rev}\0${p}`, got.get(p) ? splitLines(got.get(p)!) : null);
      }));
      for (const c of todo) {
        const i = c.line! - 1;
        const now = lineAt.get(`${head}\0${c.path}`);
        const then = lineAt.get(`${c.commit}\0${c.path}`);
        c.outdated = (now?.[i] ?? null) !== (then?.[i] ?? null);
      }
    }
    return out;
  }

  async prDiff(n: number): Promise<PrDiff> {
    const d = await this.pr(n);
    if (!d.headCommit || !d.baseCommit || !d.commits.length || d.pr.state !== "open") {
      if (d.pr.merged_commit) return { ...(await this.patch(d.pr.merged_commit)), from: null, to: d.pr.merged_commit };
      return { patch: "", from: null, to: d.headCommit, truncated: false };
    }
    const oldest = d.commits[d.commits.length - 1];
    const fork = oldest.parent ?? d.baseCommit;
    return this.rangeDiff(fork, d.headCommit);
  }

  // ---- compare: what a pull request from head into base would contain ---------------------------

  private async branchTip(name: string, label: string): Promise<string> {
    if (typeof name !== "string" || !name || name.startsWith("-") || name.startsWith("_") || /[\s\0]/.test(name) || name.length > 300) throw new HttpError(400, `bad ${label} branch "${String(name).slice(0, 80)}"`);
    const b = (await this.refs()).branches.find((x) => x.name === name);
    if (!b) throw new HttpError(404, `no ${label} branch "${name}"`);
    return b.commit;
  }

  async compare(base: string, head: string): Promise<Compare> {
    const [baseCommit, headCommit] = await Promise.all([this.branchTip(base, "base"), this.branchTip(head, "head")]);
    const commits = baseCommit === headCommit ? [] : (await this.log({ ref: headCommit, range: `${baseCommit}..${headCommit}`, limit: 200 })).commits;
    let diff: PrDiff = { patch: "", from: null, to: headCommit, truncated: false };
    if (commits.length) diff = await this.rangeDiff(commits[commits.length - 1].parent ?? baseCommit, headCommit);
    const tip = await this.metaTip();
    const existing = tip ? (await this.prIndex(tip)).records.find((p) => p.state === "open" && p.head === head && p.base === base)?.id ?? null : null;
    const first = commits[commits.length - 1];
    return { base, head, baseCommit, headCommit, commits, patch: diff.patch, truncated: diff.truncated, existing, title: first?.subject ?? "", body: first?.body ?? "" };
  }

  // ---- actions (opt-in) ------------------------------------------------------------------------

  private requireActions() {
    if (!this.actionsOn) throw new HttpError(403, "actions are disabled: set \"actions\": true in the config");
  }

  /** The name to show for who acts, and the file-name form of it. */
  private who() {
    const a = this.author ?? process.env.PAIL_AUTHOR;
    return (a ? a.split("<")[0].trim() : process.env.USER ?? "web") || "web";
  }

  /**
   * Commit one file to `_meta`. `ifAbsent`: it must not exist yet; `expect`: it must still be the
   * blob the caller read (`--expect`). Either failing is a PailError with code 4.
   */
  private async put(message: string, files: [string, string][], opts: { ifAbsent?: boolean; expect?: string } = {}) {
    const args = ["put", META, "-m", message];
    // One stdin stream can feed only one `-` file, so write each from its own read of stdin.
    if (files.length !== 1) throw new Error("one file per put");
    args.push("--file", `${files[0][0]}=-`);
    if (opts.ifAbsent) args.push("--if-absent");
    if (opts.expect) args.push("--expect", `${files[0][0]}=${opts.expect}`);
    try {
      await this.run(args, files[0][1]);
    } finally {
      this.invalidateRefs();
    }
  }

  /** Files of `_meta` as they are now (not the cached tip), with their blob ids. */
  private async readFresh(paths: string[]): Promise<Map<string, { id: string; data: Buffer } | null>> {
    const names = paths.map((p) => `${META}:${p}`);
    const got = parseBatchIds(await this.run(["cat-file", "--batch"], names.join("\n") + "\n"), names);
    return new Map(paths.map((p, i) => [p, got.get(names[i]) ?? null]));
  }

  /** PR n's record as it is now, and the blob id to write it against. */
  private async readRecord(n: number): Promise<{ rec: PrRecord; id: string }> {
    const f = (await this.readFresh([`prs/${n}.json`])).get(`prs/${n}.json`);
    let rec: PrRecord | null = null;
    try { rec = f ? JSON.parse(f.data.toString()) : null; } catch { /* unreadable */ }
    if (!f || !rec) throw new HttpError(404, `no pull request #${n}`);
    return { rec, id: f.id };
  }

  /**
   * Change PR n's record: `decide` gets the record as it is now and returns the next one, null to
   * leave it, or throws (an HttpError says why not). The write is against the blob that was read,
   * so it never overwrites a record it did not see; when the record changed meanwhile, read and
   * decide again.
   */
  private async update(n: number, message: string, decide: (rec: PrRecord) => PrRecord | null | Promise<PrRecord | null>): Promise<PrRecord> {
    for (let tries = 0; tries < MAX_TRIES; tries++) {
      const { rec, id } = await this.readRecord(n);
      const next = await decide({ ...rec });
      if (!next) return rec;
      next.updated = now();
      try {
        await this.put(message, [[`prs/${n}.json`, JSON.stringify(next, null, 1)]], { expect: id });
        return next;
      } catch (e) {
        if (!(e instanceof PailError && e.code === 4)) throw e; // 4: the record changed since we read it
      }
    }
    throw new HttpError(503, `pull request #${n} keeps changing; try again`);
  }

  /** A fresh read of one record's detail (not the 3-second-old ref). */
  private async fresh(n: number): Promise<PrDetail> {
    this.invalidateRefs();
    return this.pr(n);
  }

  /** Only a merge changes a PR that is being merged. */
  private static busy(rec: PrRecord) {
    if (rec.state === "merging") throw new HttpError(409, `PR ${rec.id} is being merged by ${rec.merging?.by ?? "?"}`);
  }

  /** A branch's tip now, or null. */
  private async tipNow(branch: string): Promise<string | null> {
    this.invalidateRefs();
    return (await this.refs()).branches.find((b) => b.name === branch)?.commit ?? null;
  }

  /** Open a pull request: the same record `pail pr create` writes, its id claimed with --if-absent. */
  async createPr(input: Partial<NewPr>): Promise<PrRecord> {
    this.requireActions();
    const title = checkText("title", input.title, MAX_TITLE, true, true);
    const body = checkText("body", input.body, MAX_BODY, false);
    const draft = input.draft === true;
    const head = String(input.head ?? "");
    const base = String(input.base ?? "");
    const c = await this.compare(base, head);
    if (head === base) throw new HttpError(400, "head and base are the same branch");
    if (!c.commits.length) throw new HttpError(400, `no commits between ${base} and ${head}`);
    if (c.existing) throw new HttpError(409, `pull request #${c.existing} is already open for ${head} into ${base}`);
    this.invalidateRefs();
    const tip = await this.metaTip();
    let n = Math.max(0, ...(tip ? (await this.prIndex(tip)).records.map((p) => p.id) : [])) + 1;
    for (let tries = 0; tries < MAX_TRIES; tries++, n++) {
      const t = now();
      const rec: PrRecord = { id: n, title, body, head, base, author: this.who(), state: "open", draft, created: t, updated: t };
      try {
        await this.put(`Open PR ${n}: ${title}`, [[`prs/${n}.json`, JSON.stringify(rec, null, 1)]], { ifAbsent: true });
        return rec;
      } catch (e) {
        if (!(e instanceof PailError && e.code === 4)) throw e; // 4: someone claimed this id first
      }
    }
    throw new HttpError(503, "could not claim a pull request id");
  }

  /** The PR's commits (oldest first) and the fork point (the oldest one's parent), as `base..head` says now. */
  private async prCommits(rec: PrRecord, head: string | null): Promise<{ fork: string | null; ids: string[] }> {
    const base = rec.state === "merged" || !head ? null : await this.tipNow(rec.base);
    if (!base || !head) return { fork: null, ids: [] };
    const commits = (await this.log({ ref: head, range: `${base}..${head}`, limit: 200 })).commits;
    return { fork: commits.length ? commits[commits.length - 1].parent : null, ids: commits.map((c) => c.id).reverse() };
  }

  /** `commit` (a full id or a prefix of one) as one of the PR's commits, or 400. */
  private pickCommit(n: number, ids: string[], commit: string): string {
    const hit = /^[0-9a-f]{4,64}$/.test(commit) ? ids.filter((i) => i.startsWith(commit)) : [];
    if (hit.length !== 1) throw new HttpError(400, `commit ${commit.slice(0, 64)} is not one of PR ${n}'s commits`);
    return hit[0];
  }

  async review(n: number, verdict: "approve" | "changes", comment: string, commit?: unknown) {
    this.requireActions();
    const rec = (await this.fresh(n)).pr;
    const who = this.who();
    const text = checkText("comment", comment, MAX_BODY, false);
    const head = await this.tipNow(rec.head);
    let at: string | null = head;
    if (commit !== undefined && commit !== null && commit !== "") {
      if (typeof commit !== "string") throw new HttpError(400, "commit must be text");
      at = this.pickCommit(n, (await this.prCommits(rec, head)).ids, commit); // the head the reviewer read, when a push may have come since
    }
    const r = { verdict, comment: text, at: now(), who, commit: at };
    await this.put(`Review PR ${n}: ${verdict}`, [[`prs/${n}/reviews/${slug(who)}.json`, JSON.stringify(r)]]);
  }

  /** The anchor of a line comment, or 400: the commit (default: the head) must be one of the PR's, the path changed by the PR at that commit, and the line a line of that side's file. */
  private async anchorFor(rec: PrRecord, input: NewComment): Promise<{ path: string; line: number; side: Side; commit: string }> {
    const { path, line } = input;
    const side = input.side ?? "new";
    if (typeof path !== "string" || !path || line === undefined || line === null) throw new HttpError(400, "a line comment needs a path and a line");
    if (!Number.isInteger(line)) throw new HttpError(400, "line must be a whole number");
    if (side !== "new" && side !== "old") throw new HttpError(400, "side is new or old");
    if (input.commit !== undefined && typeof input.commit !== "string") throw new HttpError(400, "commit must be text");
    const head = await this.tipNow(rec.head);
    if (!head) throw new HttpError(400, `branch ${rec.head} no longer exists`);
    const { fork, ids } = await this.prCommits(rec, head);
    const commit = input.commit ? this.pickCommit(rec.id, ids, input.commit) : this.pickCommit(rec.id, ids, head);
    // Changed by the PR: the file differs between the fork point and the commit (`pail diff` needs a working copy).
    const [after, before] = await Promise.all([this.readMany(commit, [path]), fork ? this.readMany(fork, [path]) : new Map<string, Buffer | null>()]);
    const [neu, old] = [after.get(path) ?? null, before.get(path) ?? null];
    if (!(neu || old) || (neu && old && neu.equals(old))) throw new HttpError(400, `${path} is not changed by PR ${rec.id} at ${commit.slice(0, 12)}`);
    const file = side === "new" ? neu : old;
    if (!file || !(line >= 1 && line <= splitLines(file).length)) throw new HttpError(400, `${path} has no line ${line} on the ${side} side at ${commit.slice(0, 12)}`);
    return { path, line, side, commit };
  }

  async comment(n: number, input: Partial<NewComment>): Promise<PrComment> {
    this.requireActions();
    const rec = (await this.fresh(n)).pr;
    const body = checkText("comment", input.body, MAX_BODY, false, true);
    const who = this.who();
    let anchor: Partial<Pick<PrComment, "path" | "line" | "side" | "commit" | "in_reply_to">> = {};
    let thread: string | undefined;
    if (input.in_reply_to !== undefined && input.in_reply_to !== null) {
      // A reply is in its parent's thread and on its parent's line.
      if (input.path !== undefined || input.line !== undefined || input.side !== undefined || input.commit !== undefined) throw new HttpError(400, "a reply takes its parent's line: drop path, line, side and commit");
      const pid = input.in_reply_to;
      if (typeof pid !== "string" || !COMMENT_ID.test(pid)) throw new HttpError(400, `bad comment id ${JSON.stringify(pid).slice(0, 80)}`);
      const file = `prs/${n}/comments/${pid}.json`;
      const got = (await this.readFresh([file])).get(file);
      let parent: Record<string, unknown> | null = null;
      try { parent = got ? JSON.parse(got.data.toString()) : null; } catch { /* unreadable */ }
      if (!parent) throw new HttpError(404, `PR ${n} has no comment ${pid}`);
      for (const k of ["path", "line", "side", "commit"] as const) if (k in parent) (anchor as Record<string, unknown>)[k] = parent[k];
      anchor.in_reply_to = pid;
      // The thread is its first comment's: follow the parents up, as far as they exist.
      thread = pid;
      for (let up = parent, hops = 0; typeof up.in_reply_to === "string" && hops < MAX_TRIES; hops++) {
        const f = `prs/${n}/comments/${up.in_reply_to}.json`;
        const g = (await this.readFresh([f])).get(f);
        if (!g) break;
        thread = up.in_reply_to;
        try { up = JSON.parse(g.data.toString()); } catch { break; }
      }
    } else if (input.path !== undefined || input.line !== undefined || input.side !== undefined || input.commit !== undefined) {
      anchor = await this.anchorFor(rec, input as NewComment);
    }
    let ms = Date.now();
    for (let tries = 0; tries < MAX_TRIES; tries++, ms++) {
      const id = `${String(ms).padStart(13, "0")}-${slug(who)}`;
      const row = { who, body, at: now(), ...anchor };
      try {
        await this.put(`Comment on PR ${n}`, [[`prs/${n}/comments/${id}.json`, JSON.stringify(row)]], { ifAbsent: true });
        return { id, ...row, thread: thread ?? id, outdated: false } as PrComment;
      } catch (e) {
        if (!(e instanceof PailError && e.code === 4)) throw e;
      }
    }
    throw new HttpError(503, "could not store the comment");
  }

  async edit(n: number, input: { title?: unknown; body?: unknown }) {
    this.requireActions();
    if (input.title === undefined && input.body === undefined) throw new HttpError(400, "nothing to change");
    const title = input.title === undefined ? undefined : checkText("title", input.title, MAX_TITLE, true, true);
    const body = input.body === undefined ? undefined : checkText("body", input.body, MAX_BODY, false);
    await this.update(n, `Edit PR ${n}`, (rec) => {
      Repo.busy(rec);
      if (rec.state === "merged") throw new HttpError(409, `PR ${n} is merged`);
      if (title !== undefined) rec.title = title;
      if (body !== undefined) rec.body = body;
      return rec;
    });
  }

  async setDraft(n: number, draft: boolean) {
    this.requireActions();
    await this.update(n, `PR ${n} ${draft ? "back to draft" : "ready for review"}`, (rec) => {
      Repo.busy(rec);
      if (rec.state !== "open") throw new HttpError(409, `PR ${n} is ${rec.state}`);
      rec.draft = draft;
      return rec;
    });
  }

  async close(n: number) {
    this.requireActions();
    await this.update(n, `Close PR ${n}`, (rec) => {
      Repo.busy(rec);
      if (rec.state !== "open") throw new HttpError(409, `PR ${n} is ${rec.state}`);
      return { ...rec, state: "closed", closed_at: now(), closed_by: this.who() };
    });
  }

  async reopen(n: number) {
    this.requireActions();
    await this.update(n, `Reopen PR ${n}`, async (rec) => {
      Repo.busy(rec);
      if (rec.state !== "closed") throw new HttpError(409, `PR ${n} is ${rec.state}; only a closed PR can be reopened`);
      if (!(await this.tipNow(rec.head))) throw new HttpError(409, `branch ${rec.head} no longer exists`);
      const { closed_at: _a, closed_by: _b, ...open } = rec;
      return { ...open, state: "open" };
    });
  }

  /**
   * Merge: judge the PR, claim it (state `merging`, which edit, close, ready and reopen refuse),
   * land exactly the commit that was judged, then record the merge. Any failure before the land
   * releases the claim. `takeover` merges a PR whose earlier merge died after claiming it.
   */
  async merge(n: number, force: boolean, takeover = false) {
    this.requireActions();
    const me = slug(this.who());
    let rec!: PrRecord;
    let judged = "";
    for (let tries = 0; ; tries++) {
      if (tries >= MAX_TRIES) throw new HttpError(503, `pull request #${n} keeps changing; try again`);
      this.invalidateRefs();
      const read = await this.readRecord(n);
      const d = await this.pr(n, read.rec);
      let problems = d.readiness.problems;
      if (read.rec.state === "merging" && takeover) problems = problems.filter((p) => !p.startsWith("pull request is being merged"));
      // Policy can be overridden; a closed, merged, draft or merging PR or a missing branch cannot.
      const hard = problems.filter((p) => p.startsWith("pull request is ") || p.includes("no longer exists"));
      if (hard.length) throw new HttpError(409, `cannot merge: ${hard.join("; ")}`);
      if (problems.length && !force) throw new HttpError(409, `cannot merge: ${problems.join("; ")}`);
      rec = { ...read.rec, state: "merging", merging: { by: me, commit: d.headCommit! }, updated: now() };
      judged = d.headCommit!;
      try {
        await this.put(`Merge PR ${n}: claim`, [[`prs/${n}.json`, JSON.stringify(rec, null, 1)]], { expect: read.id });
        break;
      } catch (e) {
        if (!(e instanceof PailError && e.code === 4)) throw e;
      }
    }
    let out: string;
    try {
      out = (await this.run(["land", rec.head, "--stack", "--onto", rec.base, "--expect-tip", judged])).toString();
    } catch (e) {
      const why = e instanceof Error ? e.message : String(e);
      const released = await this.update(n, `Merge PR ${n}: released`, (r) => {
        if (r.state !== "merging" || r.merging?.by !== me) return null;
        const { merging: _m, ...open } = r;
        return { ...open, state: "open" };
      }).then(() => true, () => false);
      throw new HttpError(409, `cannot merge: ${why} (nothing landed${released ? "" : "; the PR is still marked merging, release it with pail pr merge --takeover"})`);
    }
    this.invalidateRefs();
    const tip = await this.landedTip(out, rec.base);
    await this.update(n, `Merge PR ${n}`, (r) => {
      if (r.state === "merged") return null;
      if (r.state !== "merging" || r.merging?.by !== me) throw new HttpError(409, `PR ${n} was changed while merging; its commits are on ${rec.base}`);
      const { merging: _m, ...rest } = r;
      return { ...rest, state: "merged", merged_commit: tip, merged_by: this.who(), merged_at: now() };
    });
    return out.trim();
  }

  /** The full id of the tip `land` printed (it prints twelve digits); the base's tip if it printed none. */
  private async landedTip(out: string, base: string): Promise<string | undefined> {
    const m = /@ ([0-9a-f]{12})/.exec(out);
    if (m) {
      const log = await this.lines<RawCommit>(["log", "--json", "-n", "50", base]).catch(() => []);
      const hit = log.find((c) => c.id.startsWith(m[1]));
      if (hit) return hit.id;
    }
    return (await this.tipNow(base)) ?? undefined;
  }
}

const now = () => Math.floor(Date.now() / 1000);

export function rollupOf(states: CheckState[]): CheckRollup {
  const failed = states.filter((s) => s === "failure").length;
  const pending = states.filter((s) => s === "pending").length;
  const passed = states.filter((s) => s === "success").length;
  const total = states.length;
  return { state: !total ? "none" : failed ? "failure" : pending ? "pending" : "success", passed, failed, pending, total };
}

function splitLines(b: Buffer) {
  const s = b.toString("utf8");
  const lines = s.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}

/** `diff --git` text for one path, from the blobs on either side (null: absent). */
export function formatFilePatch(path: string, a: Buffer | null, b: Buffer | null): string {
  let out = `diff --git a/${path} b/${path}\n`;
  if (!a) out += "new file mode 100644\n";
  else if (!b) out += "deleted file mode 100644\n";
  const binary = (a?.subarray(0, 8000).includes(0) ?? false) || (b?.subarray(0, 8000).includes(0) ?? false);
  if (binary) return out + `Binary files ${a ? "a/" + path : "/dev/null"} and ${b ? "b/" + path : "/dev/null"} differ\n`;
  if ((a?.length ?? 0) > 512 * 1024 || (b?.length ?? 0) > 512 * 1024) return out + `Binary files ${a ? "a/" + path : "/dev/null"} and ${b ? "b/" + path : "/dev/null"} differ\n`;
  const old = a ? splitLines(a) : [];
  const neu = b ? splitLines(b) : [];
  const sp = structuredPatch(path, path, old.join("\n") + (old.length ? "\n" : ""), neu.join("\n") + (neu.length ? "\n" : ""), "", "", { context: 3 });
  out += `--- ${a ? "a/" + path : "/dev/null"}\n+++ ${b ? "b/" + path : "/dev/null"}\n`;
  for (const h of sp.hunks) {
    const os = h.oldLines === 0 ? h.oldStart - 1 : h.oldStart;
    const ns = h.newLines === 0 ? h.newStart - 1 : h.newStart;
    out += `@@ -${os}${h.oldLines === 1 ? "" : "," + h.oldLines} +${ns}${h.newLines === 1 ? "" : "," + h.newLines} @@\n`;
    out += h.lines.join("\n") + "\n";
  }
  return out;
}
