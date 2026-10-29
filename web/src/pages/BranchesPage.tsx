import { Link } from "react-router-dom";
import type { PrSummary } from "../../shared/types";
import { repoApi, useGet } from "../api";
import { Avatar } from "../components/Avatar";
import { CheckBadge } from "../components/Checks";
import { Time, useDocumentTitle } from "../components/Bits";
import { Icon } from "../components/Icon";
import { commitPath, commitsPath, pullPath, treePath } from "../lib/routes";
import { useRepo } from "../repoContext";

export default function BranchesPage() {
  const { repo, overview } = useRepo();
  useDocumentTitle("Branches");
  const prs = useGet<PrSummary[]>(repoApi(repo, "/prs"));
  return (
    <div>
      <header className="pagehead"><h1>Branches</h1><span className="muted">{overview.branches.length} in this repository</span></header>
      <ul className="rows">
        {overview.branches.map((b) => {
          const pr = prs.data?.find((p) => p.head === b.name && p.state === "open");
          return (
            <li key={b.name} className="rows__item">
              <Icon name="branch" size={16} className="rows__ico" />
              <div className="rows__main">
                <div className="rows__title">
                  <Link to={treePath(repo, b.name)} className="ref ref--lg">{b.name}</Link>
                  {b.isDefault && <span className="pill pill--quiet">default</span>}
                  {pr && <Link to={pullPath(repo, pr.id)} className="pill pill--open"><Icon name="pr" size={12} />#{pr.id}</Link>}
                  <CheckBadge rollup={b.checks} />
                </div>
                <div className="rows__sub">
                  {b.author && <><Avatar name={b.author} size={16} /> {b.author}</>}
                  {b.subject && <Link to={commitPath(repo, b.commit)}>{b.subject}</Link>}
                  {b.date && <Time iso={b.date} />}
                </div>
              </div>
              <Link className="btn btn--small" to={commitsPath(repo, b.name)}>History</Link>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
