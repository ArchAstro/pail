import { Link } from "react-router-dom";
import type { CheckResult, LogPage } from "../../shared/types";
import { q, repoApi, useGet } from "../api";
import { CheckRows } from "../components/CheckRows";
import { CheckBadge } from "../components/Checks";
import { ErrorNote, Loading, Sha, Time, useDocumentTitle } from "../components/Bits";
import { commitPath, treePath } from "../lib/routes";
import { useCurrentRef, useRepo } from "../repoContext";

export default function ChecksPage() {
  const { repo, overview } = useRepo();
  const ref = overview.default;
  useCurrentRef(ref);
  useDocumentTitle("Checks");
  const tip = useGet<{ commit: string; checks: CheckResult[] }>(repoApi(repo, `/checks${q({ ref })}`));
  const log = useGet<LogPage>(repoApi(repo, `/log${q({ ref, limit: 25 })}`));
  const withChecks = overview.branches.filter((b) => b.checks && b.checks.state !== "none");
  return (
    <div className="checksPage">
      <header className="pagehead"><h1>Checks</h1><span className="muted">Results recorded on <code>_meta</code>, keyed by commit</span></header>
      <section className="block">
        <h2>Latest on <Link className="ref" to={treePath(repo, ref)}>{ref}</Link> {tip.data && <Sha id={tip.data.commit} to={commitPath(repo, tip.data.commit)} />}</h2>
        {tip.error ? <ErrorNote error={tip.error} retry={tip.reload} /> : !tip.data ? <Loading /> : <CheckRows checks={tip.data.checks} empty={`${ref} declares no checks (pail.ci.json) and has no results.`} />}
      </section>
      <section className="block">
        <h2>Branches with results</h2>
        {withChecks.length === 0 ? <p className="muted">No branch tip has check results.</p> : (
          <ul className="rows rows--tight">
            {withChecks.map((b) => (
              <li key={b.name} className="rows__item">
                <div className="rows__main"><Link className="ref" to={treePath(repo, b.name)}>{b.name}</Link> <span className="muted">{b.subject}</span></div>
                <CheckBadge rollup={b.checks} />
              </li>
            ))}
          </ul>
        )}
      </section>
      <section className="block">
        <h2>Recent commits on {ref}</h2>
        {!log.data ? <Loading /> : (
          <ul className="rows rows--tight">
            {log.data.commits.map((c) => (
              <li key={c.id} className="rows__item">
                <div className="rows__main"><Link to={commitPath(repo, c.id)} className="rows__link">{c.subject}</Link> <Time iso={c.date} /></div>
                {c.checks ? <CheckBadge rollup={c.checks} /> : <span className="muted small">no results</span>}
                <Sha id={c.id} />
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}
