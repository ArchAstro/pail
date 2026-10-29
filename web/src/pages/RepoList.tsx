import { Link } from "react-router-dom";
import type { AppConfig, RepoOverview, RepoSummary } from "../../shared/types";
import { repoApi, useGet } from "../api";
import { Empty, ErrorNote, Loading, Time, useDocumentTitle } from "../components/Bits";
import { Icon } from "../components/Icon";
import { TopBar } from "../components/Shell";
import { repoPath } from "../lib/routes";

export default function RepoList() {
  useDocumentTitle("");
  const cfg = useGet<AppConfig>("/api/config", { live: false });
  return (
    <>
      <TopBar />
      <main id="main" className="page page--narrow" tabIndex={-1}>
        <h1 className="display">Repositories</h1>
        <p className="lede">Read straight from the bucket. Nothing here is a copy.</p>
        {cfg.error ? <ErrorNote error={cfg.error} retry={cfg.reload} /> : !cfg.data ? <Loading /> : cfg.data.repos.length === 0 ? (
          <Empty title="No repositories yet">Add one to <code>pail-web.config.json</code>, or set <code>PAIL_WEB_REPOS=name=gs://bucket/prefix</code>.</Empty>
        ) : (
          <ul className="repos">{cfg.data.repos.map((r) => <RepoRow key={r.name} repo={r} />)}</ul>
        )}
      </main>
    </>
  );
}

function RepoRow({ repo }: { repo: RepoSummary }) {
  const o = useGet<RepoOverview>(repoApi(repo.name), { live: false });
  const tip = o.data?.branches.find((b) => b.isDefault);
  return (
    <li className="repos__item">
      <Link to={repoPath(repo.name)} className="repos__link">
        <span className="repos__name"><Icon name="repo" size={18} />{repo.name}</span>
        {repo.description && <span className="repos__desc">{repo.description}</span>}
        <span className="repos__store"><code>{repo.store}</code></span>
        <span className="repos__meta">
          {o.error ? <span className="del">unreachable: {o.error.message.slice(0, 80)}</span> : !o.data ? <span className="skeleton" /> : (
            <>
              <span><Icon name="branch" size={13} /> {o.data.branches.length} {o.data.branches.length === 1 ? "branch" : "branches"}</span>
              {tip?.date && <span>updated <Time iso={tip.date} /></span>}
            </>
          )}
        </span>
      </Link>
    </li>
  );
}
