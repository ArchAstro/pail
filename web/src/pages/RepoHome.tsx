import { Link, useNavigate } from "react-router-dom";
import type { LogPage, PrSummary, TreeListing } from "../../shared/types";
import { q, repoApi, useGet } from "../api";
import { Markdown } from "../components/Markdown";
import { CheckBadge } from "../components/Checks";
import { CommitList } from "../components/CommitList";
import { CopyButton, Empty, ErrorNote, Loading, Time, useDocumentTitle } from "../components/Bits";
import { Icon } from "../components/Icon";
import { RefPicker } from "../components/RefPicker";
import { TreeTable } from "../components/TreeTable";
import { branchesPath, commitsPath, pullPath, pullsPath, treePath } from "../lib/routes";
import { useCurrentRef, useRepo } from "../repoContext";

export default function RepoHome() {
  const { repo, overview } = useRepo();
  const nav = useNavigate();
  const ref = overview.default;
  useCurrentRef(ref);
  useDocumentTitle(repo);
  const tree = useGet<TreeListing>(repoApi(repo, `/tree${q({ ref })}`));
  const readme = useGet<{ path: string; text: string } | null>(repoApi(repo, `/readme${q({ ref })}`));
  const log = useGet<LogPage>(repoApi(repo, `/log${q({ ref, limit: 8 })}`));
  const prs = useGet<PrSummary[]>(repoApi(repo, "/prs"));
  const open = prs.data?.filter((p) => p.state === "open") ?? [];
  const others = overview.branches.filter((b) => !b.isDefault).slice(0, 5);

  return (
    <div className="home">
      <header className="hero">
        <div>
          <h1 className="display">{repo}</h1>
          {overview.description && <p className="lede">{overview.description}</p>}
          <p className="store"><code>{overview.store}</code><CopyButton text={overview.store} label="Copy store URL" /></p>
        </div>
        <RefPicker current={ref} branches={overview.branches} defaultBranch={overview.default} onPick={(r) => nav(treePath(repo, r))} />
      </header>

      <div className="home__grid">
        <div className="home__main">
          {tree.error ? <ErrorNote error={tree.error} retry={tree.reload} /> : !tree.data ? <Loading /> : <TreeTable repo={repo} listing={tree.data} refName={ref} />}
          {readme.data && (
            <section className="readme" aria-label={readme.data.path}>
              <h2 className="readme__title"><Icon name="file" size={15} />{readme.data.path}</h2>
              {/\.(md|markdown)$/i.test(readme.data.path) || !/\./.test(readme.data.path) ? (
                <Markdown text={readme.data.text} ctx={{ repo, ref, dir: "" }} />
              ) : (
                <pre className="plain">{readme.data.text}</pre>
              )}
            </section>
          )}
        </div>

        <aside className="home__side">
          <section className="side-block">
            <header><h2>Pull requests</h2><Link to={pullsPath(repo)}>All</Link></header>
            {open.length === 0 ? <p className="muted">No open pull requests.</p> : (
              <ul className="mini">
                {open.slice(0, 4).map((p) => (
                  <li key={p.id}>
                    <Link to={pullPath(repo, p.id)}>#{p.id} {p.title}</Link>
                    <span className="mini__sub">{p.author} <CheckBadge rollup={p.checks} compact /></span>
                  </li>
                ))}
              </ul>
            )}
          </section>
          <section className="side-block">
            <header><h2>Branches</h2><Link to={branchesPath(repo)}>All {overview.branches.length}</Link></header>
            <ul className="mini">
              {others.map((b) => (
                <li key={b.name}>
                  <Link to={treePath(repo, b.name)} className="ref">{b.name}</Link>
                  <span className="mini__sub">{b.date && <Time iso={b.date} />} <CheckBadge rollup={b.checks} compact /></span>
                </li>
              ))}
              {others.length === 0 && <li className="muted">Only {ref}.</li>}
            </ul>
          </section>
          <section className="side-block">
            <header><h2>Recent activity</h2><Link to={commitsPath(repo, ref)}>History</Link></header>
            {log.data ? <CommitList repo={repo} commits={log.data.commits.slice(0, 6)} /> : <Loading />}
            {log.data?.commits.length === 0 && <Empty title="No commits yet" />}
          </section>
        </aside>
      </div>
    </div>
  );
}

