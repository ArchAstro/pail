import { useState, type FormEvent } from "react";
import { Link, useNavigate, useSearchParams } from "react-router-dom";
import type { Compare } from "../../shared/types";
import { ApiError, postJson, q, repoApi, useGet } from "../api";
import { CommitList } from "../components/CommitList";
import { Empty, ErrorNote, Loading, Tabs, useDocumentTitle } from "../components/Bits";
import { DiffView } from "../components/DiffView";
import { Icon } from "../components/Icon";
import { comparePath, pullPath, treePath } from "../lib/routes";
import { useRepo } from "../repoContext";

type Tab = "commits" | "changes";

/** base...head: what a pull request from head into base would hold, and the form to open it. */
export default function ComparePage() {
  const { repo, overview, actions } = useRepo();
  const [sp] = useSearchParams();
  const nav = useNavigate();
  const base = sp.get("base") || overview.default;
  const head = sp.get("head") || "";
  useDocumentTitle(head ? `Compare ${base}...${head}` : "Compare changes");
  const names = overview.branches.map((b) => b.name);
  const go = (b: string, h: string) => nav(comparePath(repo, b, h), { replace: true });
  return (
    <div>
      <header className="pagehead">
        <h1>{head ? "Open a pull request" : "Compare changes"}</h1>
      </header>
      <div className="compare__bar">
        <label className="field"><span>Base: merge into</span>
          <select className="select" aria-label="Base branch" value={base} onChange={(e) => go(e.target.value, head)}>
            {names.map((n) => <option key={n} value={n}>{n}</option>)}
          </select>
        </label>
        <span className="compare__arrow" aria-hidden="true">←</span>
        <label className="field"><span>Head: your changes</span>
          <select className="select" aria-label="Head branch" value={head} onChange={(e) => go(base, e.target.value)}>
            <option value="" disabled>choose a branch to compare</option>
            {names.map((n) => <option key={n} value={n}>{n}</option>)}
          </select>
        </label>
      </div>
      {head ? <Body repo={repo} base={base} head={head} actions={actions} /> : <Empty title="Choose a head branch">Pick the branch with your changes; the commits and diff against {base} appear here.</Empty>}
    </div>
  );
}

function Body({ repo, base, head, actions }: { repo: string; base: string; head: string; actions: boolean }) {
  const c = useGet<Compare>(repoApi(repo, `/compare${q({ base, head })}`));
  const [tab, setTab] = useState<Tab>("commits");
  if (c.error) return <ErrorNote error={c.error} retry={c.reload} />;
  if (!c.data) return <Loading />;
  const d = c.data;
  const same = base === head;
  const files = (d.patch.match(/^diff --git /gm) ?? []).length;
  return (
    <>
      {same || !d.commits.length ? (
        <Empty title={same ? "Choose two different branches" : "Nothing to compare"}>
          {same ? <>A branch cannot be merged into itself.</> : <><code>{head}</code> has no commits that <code>{base}</code> does not.</>}
        </Empty>
      ) : (
        <>
          {d.existing && <div className="note" role="status">An open pull request already exists for these branches: <Link to={pullPath(repo, d.existing)}>#{d.existing}</Link>.</div>}
          <p className="compare__sum">
            <strong>{d.commits.length}</strong> commit{d.commits.length === 1 ? "" : "s"}{files > 0 && <> changing <strong>{files}</strong> file{files === 1 ? "" : "s"}</>}
            {" "}on <Link className="ref" to={treePath(repo, head)}>{head}</Link> not in <Link className="ref" to={treePath(repo, base)}>{base}</Link>
          </p>
          {actions
            ? !d.existing && <NewPrForm repo={repo} d={d} />
            : <div className="note" role="status">Read-only. Enable <code>"actions": true</code> to open pull requests here, or run <code>pail pr create --head {head} --base {base}</code>.</div>}
          <Tabs<Tab> label="Comparison sections" value={tab} onChange={setTab} tabs={[{ id: "commits", label: "Commits", count: d.commits.length }, { id: "changes", label: "Changes", count: files }]} />
          {tab === "commits" ? <CommitList repo={repo} commits={d.commits} /> : d.patch ? <DiffView patch={d.patch} repo={repo} blobRef={d.headCommit} truncated={d.truncated} /> : <Empty title="No file changes" />}
        </>
      )}
    </>
  );
}

function NewPrForm({ repo, d }: { repo: string; d: Compare }) {
  const nav = useNavigate();
  // Until edited, the title and body follow the first commit's message, as `pail pr create` does.
  const [title, setTitle] = useState<string | null>(null);
  const [body, setBody] = useState<string | null>(null);
  const [draft, setDraft] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const t = title ?? d.title;
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    try {
      const r = await postJson<{ pr: { id: number } }>(repoApi(repo, "/prs"), { title: t, body: body ?? d.body, head: d.head, base: d.base, draft });
      nav(pullPath(repo, r.pr.id));
    } catch (x) {
      setErr(x instanceof ApiError ? x.message : String(x));
      setBusy(false);
    }
  };
  return (
    <form className="compare__form" onSubmit={submit} aria-label="New pull request">
      <label className="field"><span>Title</span><input className="input" value={t} maxLength={256} onChange={(e) => setTitle(e.target.value)} required /></label>
      <label className="field"><span>Description (Markdown)</span><textarea value={body ?? d.body} onChange={(e) => setBody(e.target.value)} rows={6} maxLength={60000} /></label>
      <div className="actions__row">
        <button type="submit" className="btn btn--primary" disabled={busy || !t.trim()}>{draft ? "Create draft pull request" : "Create pull request"}</button>
        <label className="tickbox"><input type="checkbox" checked={draft} onChange={(e) => setDraft(e.target.checked)} />Draft (cannot be merged until marked ready)</label>
      </div>
      {err && <p className="del" role="alert">{err}</p>}
    </form>
  );
}
