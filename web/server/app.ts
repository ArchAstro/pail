import express, { type Request, type Response, type NextFunction } from "express";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { Config } from "./config.js";
import { redactStore } from "./config.js";
import { Registry } from "./registry.js";
import { HttpError, type Repo } from "./repo.js";
import { PailError } from "./pail.js";
import type { AppConfig, RepoOverview } from "../shared/types.js";

const here = dirname(fileURLToPath(import.meta.url));
export const VERSION = "0.1.0";

const wrap = (fn: (req: Request, res: Response) => Promise<unknown>) => (req: Request, res: Response, next: NextFunction) => {
  fn(req, res).catch(next);
};

const str = (v: unknown, d = "") => (typeof v === "string" ? v : d);
const int = (v: unknown, d: number) => (Number.isFinite(Number(v)) && v !== undefined && v !== "" ? Number(v) : d);

export function createApp(cfg: Config, registry = new Registry(cfg)) {
  const app = express();
  app.disable("x-powered-by");
  app.use(express.json({ limit: "160kb" }));

  const repoOf = async (req: Request): Promise<Repo> => {
    const r = await registry.get(req.params.repo);
    if (!r) throw new HttpError(404, `no repository "${req.params.repo}"`);
    return r;
  };
  // Commit-addressed responses never change; refs-addressed ones are re-read each time.
  const immutable = (res: Response, ref: string) => {
    if (/^[0-9a-f]{64}$/.test(ref)) res.set("Cache-Control", "public, max-age=31536000, immutable");
    else res.set("Cache-Control", "no-cache");
  };

  app.get("/api/config", wrap(async (_req, res) => {
    const repos = await registry.list();
    const body: AppConfig = {
      actions: cfg.actions,
      version: VERSION,
      repos: repos.map((r) => ({ name: r.name, description: r.cfg.description, store: redactStore(r.cfg.store) })),
    };
    res.json(body);
  }));

  app.get("/api/repos/:repo", wrap(async (req, res) => {
    const r = await repoOf(req);
    const b = await r.branches();
    const head = b.branches.find((x) => x.isDefault)?.commit ?? "";
    void r.warm();
    const body: RepoOverview = { name: r.name, description: r.cfg.description, store: redactStore(r.cfg.store), default: b.default, branches: b.branches, head };
    res.set("Cache-Control", "no-cache").json(body);
  }));

  app.get("/api/repos/:repo/log", wrap(async (req, res) => {
    const r = await repoOf(req);
    const ref = str(req.query.ref, "HEAD");
    const page = await r.log({ ref, path: str(req.query.path) || undefined, limit: int(req.query.limit, 30), skip: int(req.query.skip, 0), maintenance: req.query.maintenance === "1" });
    page.commits = await r.decorate(page.commits);
    res.set("Cache-Control", "no-cache").json(page);
  }));

  app.get("/api/repos/:repo/commit/:id", wrap(async (req, res) => {
    const r = await repoOf(req);
    const id = await r.resolve(req.params.id);
    const [commit, diff, checks] = await Promise.all([r.commit(id), r.patch(id), r.checks(id)]);
    const [decorated] = await r.decorate([commit]);
    // Squashed/rewritten sources, resolved to subjects for the "squashed from" links.
    const sources = await Promise.all(decorated.rewrittenFrom.map((s) => r.commit(s).then((c) => ({ id: c.id, subject: c.subject })).catch(() => ({ id: s, subject: "" }))));
    immutable(res, id);
    res.json({ commit: decorated, patch: diff.patch, truncated: diff.truncated, checks: checks.filter((c) => (c as any).recorded || c.declared), sources });
  }));

  app.get("/api/repos/:repo/tree", wrap(async (req, res) => {
    const r = await repoOf(req);
    res.set("Cache-Control", "no-cache").json(await r.tree(str(req.query.ref, "HEAD"), str(req.query.path)));
  }));

  app.get("/api/repos/:repo/blob", wrap(async (req, res) => {
    const r = await repoOf(req);
    const ref = str(req.query.ref, "HEAD");
    res.set("Cache-Control", "no-cache").json(await r.blob(ref, str(req.query.path)));
  }));

  app.get("/api/repos/:repo/raw", wrap(async (req, res) => {
    const r = await repoOf(req);
    const path = str(req.query.path);
    const { data } = await r.raw(str(req.query.ref, "HEAD"), path);
    const name = path.split("/").pop() ?? "file";
    const ext = name.split(".").pop()?.toLowerCase() ?? "";
    const types: Record<string, string> = { png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif", webp: "image/webp", avif: "image/avif", ico: "image/x-icon", bmp: "image/bmp", pdf: "application/pdf" };
    const isImage = ext in types;
    // SVG and HTML are served as plain text: raw bytes from a repo must not run in our origin.
    res.set("Content-Type", isImage ? types[ext] : data.subarray(0, 8000).includes(0) ? "application/octet-stream" : "text/plain; charset=utf-8");
    res.set("X-Content-Type-Options", "nosniff");
    res.set("Content-Security-Policy", "sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'");
    if (req.query.download === "1") res.set("Content-Disposition", `attachment; filename="${name.replace(/"/g, "")}"`);
    res.send(data);
  }));

  app.get("/api/repos/:repo/files", wrap(async (req, res) => {
    const r = await repoOf(req);
    res.set("Cache-Control", "no-cache").json(await r.files(str(req.query.ref, "HEAD")));
  }));

  app.get("/api/repos/:repo/readme", wrap(async (req, res) => {
    const r = await repoOf(req);
    res.set("Cache-Control", "no-cache").json(await r.readme(str(req.query.ref, "HEAD")));
  }));

  app.get("/api/repos/:repo/prs", wrap(async (req, res) => {
    const r = await repoOf(req);
    res.set("Cache-Control", "no-cache").json(await r.prs());
  }));

  app.get("/api/repos/:repo/prs/:n", wrap(async (req, res) => {
    const r = await repoOf(req);
    res.set("Cache-Control", "no-cache").json(await r.pr(int(req.params.n, -1)));
  }));

  app.get("/api/repos/:repo/prs/:n/diff", wrap(async (req, res) => {
    const r = await repoOf(req);
    res.set("Cache-Control", "no-cache").json(await r.prDiff(int(req.params.n, -1)));
  }));

  app.get("/api/repos/:repo/compare", wrap(async (req, res) => {
    const r = await repoOf(req);
    res.set("Cache-Control", "no-cache").json(await r.compare(str(req.query.base), str(req.query.head)));
  }));

  app.post("/api/repos/:repo/prs", wrap(async (req, res) => {
    const r = await repoOf(req);
    res.status(201).json({ ok: true, pr: await r.createPr(req.body ?? {}) });
  }));

  app.patch("/api/repos/:repo/prs/:n", wrap(async (req, res) => {
    const r = await repoOf(req);
    await r.edit(int(req.params.n, -1), { title: req.body?.title, body: req.body?.body });
    res.json({ ok: true });
  }));

  app.post("/api/repos/:repo/prs/:n/comments", wrap(async (req, res) => {
    const r = await repoOf(req);
    res.status(201).json({ ok: true, comment: await r.comment(int(req.params.n, -1), req.body?.body) });
  }));

  app.post("/api/repos/:repo/prs/:n/close", wrap(async (req, res) => {
    const r = await repoOf(req);
    await r.close(int(req.params.n, -1));
    res.json({ ok: true });
  }));

  app.post("/api/repos/:repo/prs/:n/reopen", wrap(async (req, res) => {
    const r = await repoOf(req);
    await r.reopen(int(req.params.n, -1));
    res.json({ ok: true });
  }));

  app.post("/api/repos/:repo/prs/:n/draft", wrap(async (req, res) => {
    const r = await repoOf(req);
    await r.setDraft(int(req.params.n, -1), req.body?.draft === true);
    res.json({ ok: true });
  }));

  app.post("/api/repos/:repo/prs/:n/review", wrap(async (req, res) => {
    const r = await repoOf(req);
    const verdict = req.body?.verdict === "changes" ? "changes" : "approve";
    await r.review(int(req.params.n, -1), verdict, str(req.body?.comment));
    res.json({ ok: true });
  }));

  app.post("/api/repos/:repo/prs/:n/merge", wrap(async (req, res) => {
    const r = await repoOf(req);
    res.json({ ok: true, output: await r.merge(int(req.params.n, -1), req.body?.force === true) });
  }));

  app.get("/api/repos/:repo/checks", wrap(async (req, res) => {
    const r = await repoOf(req);
    const id = await r.resolve(str(req.query.ref, "HEAD"));
    res.set("Cache-Control", "no-cache").json({ commit: id, checks: await r.checks(id) });
  }));

  // Server-sent events: one `ref` event per branch change in this repository.
  app.get("/api/repos/:repo/events", wrap(async (req, res) => {
    const r = await repoOf(req);
    res.set({ "Content-Type": "text/event-stream", "Cache-Control": "no-cache", Connection: "keep-alive", "X-Accel-Buffering": "no" });
    res.flushHeaders();
    res.write(": connected\n\n");
    const send = (e: { repo: string }) => e.repo === r.name && res.write(`event: ref\ndata: ${JSON.stringify(e)}\n\n`);
    registry.hub.on("ref", send);
    const stop = registry.hub.watch(r);
    const ping = setInterval(() => res.write(": ping\n\n"), 25_000);
    req.on("close", () => {
      clearInterval(ping);
      registry.hub.off("ref", send);
      stop();
    });
  }));

  app.get("/api/stats", (_req, res) => res.json({ cache: registry.cache.stats(), spawned: registry.pail.spawned }));
  app.use("/api", (_req, _res, next) => next(new HttpError(404, "no such endpoint")));

  // The built frontend, with history-API fallback.
  const pub = [join(here, "..", "public"), join(here, "..", "dist", "public")].find((p) => existsSync(join(p, "index.html")));
  if (pub) {
    app.use(express.static(pub, { index: false, maxAge: "1h", setHeaders: (res, p) => p.includes("/assets/") && res.set("Cache-Control", "public, max-age=31536000, immutable") }));
    app.get("*", (_req, res) => res.sendFile(join(pub, "index.html")));
  } else {
    app.get("/", (_req, res) => res.type("text").send("pail-web API is running. Build the UI with `npm run build`, or use `npm run dev`.\n"));
  }

  app.use((err: unknown, _req: Request, res: Response, _next: NextFunction) => {
    // body-parser errors (bad JSON, too large) carry their own 4xx status.
    const own = (err as { status?: unknown; statusCode?: unknown } | null)?.status;
    const status = err instanceof HttpError ? err.status : err instanceof PailError ? 502 : typeof own === "number" && own >= 400 && own < 500 ? own : 500;
    res.status(status).json({ error: err instanceof Error ? err.message : String(err) });
  });

  return { app, registry };
}
