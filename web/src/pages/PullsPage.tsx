import { Link, useSearchParams } from "react-router-dom";
import type { PrSummary } from "../../shared/types";
import { repoApi, useGet } from "../api";
import { Avatar } from "../components/Avatar";
import { CheckBadge } from "../components/Checks";
import { Empty, ErrorNote, Loading, PrState, Tabs, Time, useDocumentTitle } from "../components/Bits";
import { Icon } from "../components/Icon";
import { pullPath } from "../lib/routes";
import { useRepo } from "../repoContext";

type Filter = "open" | "merged" | "closed" | "all";

export default function PullsPage() {
  const { repo } = useRepo();
  useDocumentTitle("Pull requests");
  const [sp, setSp] = useSearchParams();
  const filter = (sp.get("state") as Filter) || "open";
  const prs = useGet<PrSummary[]>(repoApi(repo, "/prs"));
  if (prs.error) return <ErrorNote error={prs.error} retry={prs.reload} />;
  if (!prs.data) return <Loading />;
  const count = (s: Filter) => (s === "all" ? prs.data!.length : prs.data!.filter((p) => p.state === s).length);
  const list = prs.data.filter((p) => filter === "all" || p.state === filter);
  return (
    <div>
      <header className="pagehead">
        <h1>Pull requests</h1>
        <Tabs<Filter> label="Filter by state" value={filter} onChange={(f) => setSp(f === "open" ? {} : { state: f })} tabs={(["open", "merged", "closed", "all"] as Filter[]).map((id) => ({ id, label: id[0].toUpperCase() + id.slice(1), count: count(id) }))} />
      </header>
      {list.length === 0 ? (
        <Empty title={prs.data.length ? `No ${filter} pull requests` : "No pull requests yet"}>
          {prs.data.length ? <>Try another filter.</> : <>Open one with <code>pail-pr open</code>; it records <code>_meta:prs/&lt;n&gt;.json</code>.</>}
        </Empty>
      ) : (
        <ul className="rows">
          {list.map((p) => (
            <li key={p.id} className="rows__item">
              <PrState state={p.state} />
              <div className="rows__main">
                <div className="rows__title">
                  <Link to={pullPath(repo, p.id)} className="rows__link">{p.title}</Link>
                  <CheckBadge rollup={p.checks} />
                </div>
                <div className="rows__sub">
                  <span>#{p.id}</span>
                  <Avatar name={p.author} size={16} /> {p.author}
                  {p.created && <Time iso={new Date(p.created * 1000).toISOString()} />}
                  <span className="refs"><code>{p.head}</code><Icon name="chevron" size={12} /><code>{p.base}</code></span>
                </div>
              </div>
              <span className="rows__aside">
                {p.state === "open" && <span className={p.changesRequested ? "del" : p.approvals ? "add" : "muted"} title="Approvals from reviewers other than the author">
                  <Icon name={p.changesRequested ? "x" : "eye"} size={14} /> {p.changesRequested ? "changes requested" : `${p.approvals} approved`}
                </span>}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
