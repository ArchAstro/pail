import { Fragment } from "react";
import { Link } from "react-router-dom";
import type { CommitSummary } from "../../shared/types";
import { dayLabel } from "../lib/format";
import { commitPath, treePath } from "../lib/routes";
import { Avatar } from "./Avatar";
import { CheckBadge } from "./Checks";
import { Sha, Time, CopyButton } from "./Bits";
import { Icon } from "./Icon";

export function Provenance({ commit, repo, sources }: { commit: CommitSummary; repo: string; sources?: { id: string; subject: string }[] }) {
  const n = commit.rewrittenFrom.length;
  if (!n && !commit.landedFrom) return null;
  const label = n > 1 ? `Squashed from ${n} commits` : n === 1 ? "Rewritten from" : "";
  return (
    <span className="prov">
      {n > 0 && (
        <span className="prov__item">
          <Icon name="merge" size={13} />
          {label}
          {commit.rewrittenFrom.map((id) => {
            const s = sources?.find((x) => x.id === id);
            return <Link key={id} to={commitPath(repo, id)} className="sha-link" title={s?.subject}><code className="sha">{id.slice(0, 8)}</code></Link>;
          })}
        </span>
      )}
      {commit.landedFrom && (
        <span className="prov__item">
          landed from <Link to={treePath(repo, commit.landedFrom)} className="ref">{commit.landedFrom}</Link>
        </span>
      )}
    </span>
  );
}

/** Commits as a rail: a node per commit, tinted by its checks, grouped by day. */
export function CommitList({ repo, commits, animate }: { repo: string; commits: CommitSummary[]; animate?: boolean }) {
  let day = "";
  return (
    <ol className={`rail${animate ? " rail--enter" : ""}`}>
      {commits.map((c, i) => {
        const d = dayLabel(c.date);
        const head = d !== day;
        day = d;
        return (
          <Fragment key={c.id}>
            {head && <li className="rail__day" aria-hidden="false"><h3>{d}</h3></li>}
            <li className={`rail__item${c.maintenance ? " is-maintenance" : ""}`} style={{ ["--i" as string]: Math.min(i, 12) }}>
              <span className={`rail__node node--${c.checks?.state ?? "none"}`} aria-hidden="true" />
              <div className="rail__main">
                <Link to={commitPath(repo, c.id)} className="rail__subject">{c.subject}</Link>
                {c.maintenance && <span className="pill pill--quiet" title="Changes how files are stored, not what they are">maintenance</span>}
                {c.refs?.map((r) => <Link key={r} to={treePath(repo, r)} className="ref">{r}</Link>)}
                <CheckBadge rollup={c.checks} />
                <div className="rail__meta">
                  <Avatar name={c.author.name} size={16} />
                  <span>{c.author.name}</span>
                  <Time iso={c.date} />
                  <Provenance commit={c} repo={repo} />
                </div>
              </div>
              <span className="rail__id"><Sha id={c.id} to={commitPath(repo, c.id)} /><CopyButton text={c.id} label="Copy commit id" /></span>
            </li>
          </Fragment>
        );
      })}
    </ol>
  );
}
