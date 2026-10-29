import { useState } from "react";
import { Link, useParams, useSearchParams } from "react-router-dom";
import type { PrDetail, PrDiff } from "../../shared/types";
import { ApiError, postJson, repoApi, useGet } from "../api";
import { Avatar } from "../components/Avatar";
import { CheckRows } from "../components/CheckRows";
import { CheckStrip } from "../components/Checks";
import { CommitList } from "../components/CommitList";
import { Empty, ErrorNote, Loading, PrState, Tabs, Time, useDocumentTitle } from "../components/Bits";
import { DiffView } from "../components/DiffView";
import { Icon } from "../components/Icon";
import { Markdown } from "../components/Markdown";
import { treePath } from "../lib/routes";
import { useCurrentRef, useRepo } from "../repoContext";

type Tab = "conversation" | "commits" | "changes";

export default function PullPage() {
  const { repo } = useRepo();
  const { n = "" } = useParams();
  const [sp, setSp] = useSearchParams();
  const tab = (sp.get("tab") as Tab) || "conversation";
  const d = useGet<PrDetail>(repoApi(repo, `/prs/${n}`));
  useCurrentRef(d.data?.headCommit ?? d.data?.baseCommit ?? "");
  useDocumentTitle(d.data ? `#${d.data.pr.id} ${d.data.pr.title}` : "Pull request");
  if (d.error) return <ErrorNote error={d.error} retry={d.reload} />;
  if (!d.data) return <Loading />;
  const { pr } = d.data;
  const iso = (t?: number) => (t ? new Date(t * 1000).toISOString() : "");
  return (
    <article className="pr">
      <header className="pr__head">
        <h1>{pr.title} <span className="muted">#{pr.id}</span></h1>
        <p className="pr__line">
          <PrState state={pr.state} />
          <Avatar name={pr.author} size={18} />
          <strong>{pr.author}</strong>
          {pr.created && <>opened <Time iso={iso(pr.created)} /></>}
          <span className="refs">
            <Link className="ref" to={treePath(repo, pr.head)}>{pr.head}</Link>
            <Icon name="chevron" size={12} />
            <Link className="ref" to={treePath(repo, pr.base)}>{pr.base}</Link>
          </span>
        </p>
      </header>
      <Tabs<Tab> label="Pull request sections" value={tab} onChange={(t) => setSp(t === "conversation" ? {} : { tab: t })} tabs={[
        { id: "conversation", label: "Conversation" },
        { id: "commits", label: "Commits", count: d.data.commits.length },
        { id: "changes", label: "Changes" },
      ]} />
      {tab === "conversation" && <Conversation d={d.data} repo={repo} reload={d.reload} />}
      {tab === "commits" && (d.data.commits.length ? <CommitList repo={repo} commits={d.data.commits} /> : <Empty title="No commits to show">The branch has nothing ahead of {pr.base}{pr.state === "merged" ? ", because it has landed" : ""}.</Empty>)}
      {tab === "changes" && <Changes n={pr.id} repo={repo} />}
    </article>
  );
}

function Changes({ n, repo }: { n: number; repo: string }) {
  const diff = useGet<PrDiff>(repoApi(repo, `/prs/${n}/diff`));
  if (diff.error) return <ErrorNote error={diff.error} retry={diff.reload} />;
  if (!diff.data) return <Loading />;
  if (!diff.data.patch) return <Empty title="No changes against the base">Everything on this branch is already in the base.</Empty>;
  return <DiffView patch={diff.data.patch} repo={repo} blobRef={diff.data.to ?? undefined} truncated={diff.data.truncated} />;
}

function Conversation({ d, repo, reload }: { d: PrDetail; repo: string; reload: () => void }) {
  const { pr } = d;
  return (
    <div className="pr__grid">
      <div className="pr__main">
        <section className="block">
          {pr.body ? <Markdown text={pr.body} ctx={{ repo, ref: pr.base, dir: "" }} /> : <p className="muted">No description.</p>}
        </section>
        <section className="block" aria-label="Reviews">
          <h2>Reviews</h2>
          {d.reviews.length === 0 ? <p className="muted">No reviews yet.</p> : (
            <ul className="reviews">
              {d.reviews.map((r) => (
                <li key={r.who} className={`review review--${r.verdict}`}>
                  <Avatar name={r.who} size={24} />
                  <div>
                    <p><strong>{r.who}</strong> {r.verdict === "approve" ? <span className="add">approved</span> : <span className="del">requested changes</span>}{r.who === pr.author && <span className="muted"> (author, does not count)</span>} {r.at && <Time iso={new Date(r.at * 1000).toISOString()} />}</p>
                    {r.comment && <p className="review__comment">{r.comment}</p>}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </section>
        <section className="block" aria-label="Checks">
          <h2>Checks {d.headCommit && <code className="sha">{d.headCommit.slice(0, 8)}</code>}</h2>
          <CheckRows checks={d.checks} empty={d.headCommit ? "No checks have run on the head commit yet." : "The head branch no longer exists."} />
        </section>
      </div>
      <aside className="pr__side">
        <MergeBox d={d} repo={repo} reload={reload} />
      </aside>
    </div>
  );
}

function MergeBox({ d, repo, reload }: { d: PrDetail; repo: string; reload: () => void }) {
  const { pr, policy, readiness } = d;
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [comment, setComment] = useState("");
  const base = `/api/repos/${encodeURIComponent(repo)}/prs/${pr.id}`;
  const act = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    setErr(null);
    try { await fn(); reload(); } catch (e) { setErr(e instanceof ApiError ? e.message : String(e)); } finally { setBusy(false); }
  };
  const approvals = d.reviews.filter((r) => r.verdict === "approve" && r.who !== pr.author).length;
  return (
    <section className={`merge ${readiness.ready ? "is-ready" : pr.state === "open" ? "is-blocked" : ""}`} aria-label="Merge policy">
      <h2>{pr.state === "merged" ? "Merged" : pr.state === "closed" ? "Closed" : readiness.ready ? "Ready to merge" : "Not ready to merge"}</h2>
      {pr.state === "merged" && pr.merged_commit && (
        <p>Landed as <Link to={`/${encodeURIComponent(repo)}/commit/${pr.merged_commit}`}><code className="sha">{pr.merged_commit.slice(0, 8)}</code></Link>{pr.merged_by ? ` by ${pr.merged_by}` : ""}.</p>
      )}
      <h3>Policy for {pr.base}</h3>
      <ul className="policy">
        <li className={approvals >= policy.approvals ? "ok" : "no"}><Icon name={approvals >= policy.approvals ? "check" : "x"} size={14} />{approvals} of {policy.approvals} approval{policy.approvals === 1 ? "" : "s"} from someone other than the author</li>
        {policy.required_checks.map((name) => {
          const c = d.checks.find((x) => x.name === name);
          const ok = c?.state === "success";
          return <li key={name} className={ok ? "ok" : c?.state === "pending" || !c ? "wait" : "no"}><Icon name={ok ? "check" : c?.state === "failure" ? "x" : "clock"} size={14} />Required check <code>{name}</code>: {c ? (c as any).recorded === false ? "not run" : c.state : "missing"}</li>;
        })}
        {policy.required_checks.length === 0 && <li className="muted">No required checks.</li>}
      </ul>
      <CheckStrip checks={d.checks} />
      {pr.state === "open" && d.actions && (
        <div className="actions">
          <label className="field"><span>Review comment</span><textarea value={comment} onChange={(e) => setComment(e.target.value)} rows={3} /></label>
          <div className="actions__row">
            <button className="btn" disabled={busy} onClick={() => act(() => postJson(`${base}/review`, { verdict: "approve", comment }))}>Approve</button>
            <button className="btn" disabled={busy} onClick={() => act(() => postJson(`${base}/review`, { verdict: "changes", comment }))}>Request changes</button>
          </div>
          <button className="btn btn--primary" disabled={busy || !readiness.ready} onClick={() => confirm(`Land ${pr.head} onto ${pr.base}, commit by commit?`) && act(() => postJson(`${base}/merge`, {}))}>Merge with pail land</button>
          {!readiness.ready && <p className="muted">{readiness.problems.join("; ")}.</p>}
          {err && <p className="del" role="alert">{err}</p>}
        </div>
      )}
      {pr.state === "open" && !d.actions && <p className="muted small">Read-only. Enable <code>"actions": true</code> to review and merge here.</p>}
    </section>
  );
}
