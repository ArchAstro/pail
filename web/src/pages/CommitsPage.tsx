import { Link, useNavigate, useParams, useSearchParams } from "react-router-dom";
import type { LogPage } from "../../shared/types";
import { q, repoApi, useGet } from "../api";
import { CommitList } from "../components/CommitList";
import { Empty, ErrorNote, Loading, useDocumentTitle } from "../components/Bits";
import { RefPicker } from "../components/RefPicker";
import { commitsPath, treePath } from "../lib/routes";
import { useCurrentRef, useRepo } from "../repoContext";

const PAGE = 30;

export default function CommitsPage() {
  const { repo, overview, resolve } = useRepo();
  const params = useParams();
  const nav = useNavigate();
  const [sp, setSp] = useSearchParams();
  const { ref } = resolve(params["*"] ?? "");
  const path = sp.get("path") ?? "";
  const maintenance = sp.get("maintenance") === "1";
  const page = Math.max(1, Number(sp.get("page") ?? 1));
  useCurrentRef(ref);
  useDocumentTitle(`Commits on ${ref.slice(0, 12)}`);
  const log = useGet<LogPage>(repoApi(repo, `/log${q({ ref, path, limit: PAGE, skip: (page - 1) * PAGE, maintenance })}`));

  const update = (patch: Record<string, string | null>) => {
    const next = new URLSearchParams(sp);
    for (const [k, v] of Object.entries(patch)) v === null ? next.delete(k) : next.set(k, v);
    setSp(next);
  };

  return (
    <div className="commits">
      <header className="pagehead">
        <h1>Commits</h1>
        <RefPicker current={ref} branches={overview.branches} defaultBranch={overview.default} onPick={(r) => nav(commitsPath(repo, r, path))} />
        {path && (
          <span className="filterchip">
            history of <Link to={treePath(repo, ref, path)}>{path}</Link>
            <button className="icon-btn" aria-label="Clear path filter" onClick={() => update({ path: null, page: null })}>×</button>
          </span>
        )}
        <span className="top__fill" />
        <label className="switch">
          <input type="checkbox" checked={maintenance} onChange={(e) => update({ maintenance: e.target.checked ? "1" : null, page: null })} />
          <span className="switch__track" aria-hidden="true" />
          Show maintenance commits
        </label>
      </header>
      {log.error ? <ErrorNote error={log.error} retry={log.reload} /> : !log.data ? <Loading /> : log.data.commits.length === 0 ? (
        <Empty title="No commits here">{path ? "No commit has touched this path on this branch." : "This branch has no history."}</Empty>
      ) : (
        <>
          <CommitList repo={repo} commits={log.data.commits} animate />
          <nav className="pager" aria-label="Pagination">
            <button className="btn" disabled={page <= 1} onClick={() => update({ page: page > 2 ? String(page - 1) : null })}>Newer</button>
            <span className="muted">Page {page}</span>
            <button className="btn" disabled={!log.data.more} onClick={() => update({ page: String(page + 1) })}>Older</button>
          </nav>
        </>
      )}
    </div>
  );
}
