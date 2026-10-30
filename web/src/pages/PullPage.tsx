import { useMemo, useState, type FormEvent } from "react";
import { Link, useParams, useSearchParams } from "react-router-dom";
import type { NewComment, PrDetail, PrDiff } from "../../shared/types";
import { ApiError, fetchJson, postJson, repoApi, useGet } from "../api";
import { Avatar } from "../components/Avatar";
import { CheckRows } from "../components/CheckRows";
import { CheckStrip } from "../components/Checks";
import { CommitList } from "../components/CommitList";
import { Empty, ErrorNote, Loading, PrState, Tabs, Time, useDocumentTitle } from "../components/Bits";
import { DiffView } from "../components/DiffView";
import { Icon } from "../components/Icon";
import { Markdown } from "../components/Markdown";
import { InlineContext, ThreadView, CommentForm, groupThreads, type InlineCtx } from "../components/Threads";
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
          <PrState state={pr.state} draft={pr.draft} />
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
      {tab === "changes" && <Changes d={d.data} repo={repo} reload={d.reload} />}
    </article>
  );
}

/** Posts a comment on the PR and reloads it; a failure goes back to the form that sent it. */
function usePost(d: PrDetail, repo: string, reload: () => void) {
  return async (c: NewComment) => {
    await postJson(repoApi(repo, `/prs/${d.pr.id}/comments`), c);
    reload();
  };
}

function Changes({ d, repo, reload }: { d: PrDetail; repo: string; reload: () => void }) {
  const diff = useGet<PrDiff>(repoApi(repo, `/prs/${d.pr.id}/diff`));
  const post = usePost(d, repo, reload);
  const threads = useMemo(() => groupThreads(d.comments), [d.comments]);
  if (diff.error) return <ErrorNote error={diff.error} retry={diff.reload} />;
  if (!diff.data) return <Loading />;
  if (!diff.data.patch) return <Empty title="No changes against the base">Everything on this branch is already in the base.</Empty>;
  // A new line comment says which head its line number was read at: the one this diff is of.
  const inline: InlineCtx = { threads, actions: d.actions, anchorable: d.pr.state !== "merged", commit: d.pr.state === "merged" ? null : diff.data.to ?? d.headCommit, post, md: { repo, ref: d.pr.base, dir: "" } };
  return (
    <InlineContext.Provider value={inline}>
      <DiffView patch={diff.data.patch} repo={repo} blobRef={diff.data.to ?? undefined} truncated={diff.data.truncated} />
    </InlineContext.Provider>
  );
}

function Conversation({ d, repo, reload }: { d: PrDetail; repo: string; reload: () => void }) {
  const { pr } = d;
  // Once the head has moved, a review of an earlier commit counts for nothing (merged: it is history).
  const live = pr.state === "open" || pr.state === "merging";
  return (
    <div className="pr__grid">
      <div className="pr__main">
        <Description d={d} repo={repo} reload={reload} />
        <section className="block" aria-label="Reviews">
          <h2>Reviews</h2>
          {d.reviews.some((r) => r.stale && live && r.verdict === "approve" && !r.byAuthor) && <p className="muted">The branch has new commits since some approvals; those no longer count. Approve again to renew them.</p>}
          {d.reviews.length === 0 ? <p className="muted">No reviews yet.</p> : (
            <ul className="reviews">
              {d.reviews.map((r) => (
                <li key={r.who} className={`review review--${r.verdict}`}>
                  <Avatar name={r.who} size={24} />
                  <div>
                    <p>
                      <strong>{r.who}</strong>{" "}
                      {r.verdict === "approve" ? (r.stale && live ? <span className="warn">approved an older commit</span> : <span className="add">approved</span>) : <span className="del">requested changes</span>}
                      {r.stale && live && r.commit && <> <code className="sha" title="The commit this review was given on">{r.commit.slice(0, 8)}</code></>}
                      {r.byAuthor && <span className="muted"> (author, does not count)</span>} {r.at && <Time iso={new Date(r.at * 1000).toISOString()} />}
                    </p>
                    {r.comment && <p className="review__comment">{r.comment}</p>}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </section>
        <Comments d={d} repo={repo} reload={reload} />
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
  const { busy, err, act } = useAct(reload);
  const [comment, setComment] = useState("");
  const base = `/api/repos/${encodeURIComponent(repo)}/prs/${pr.id}`;
  const approved = d.reviews.filter((r) => r.verdict === "approve" && !r.byAuthor);
  const approvals = approved.filter((r) => r.commit && r.commit === d.headCommit).length;
  const older = approved.length - approvals;
  return (
    <section className={`merge ${readiness.ready ? "is-ready" : pr.state === "open" ? "is-blocked" : ""}`} aria-label="Merge policy">
      <h2>{pr.state === "merged" ? "Merged" : pr.state === "closed" ? "Closed" : pr.state === "merging" ? "Being merged" : pr.draft ? "Draft" : readiness.ready ? "Ready to merge" : "Not ready to merge"}</h2>
      {pr.state === "merged" && pr.merged_commit && (
        <p>Landed as <Link to={`/${encodeURIComponent(repo)}/commit/${pr.merged_commit}`}><code className="sha">{pr.merged_commit.slice(0, 8)}</code></Link>{pr.merged_by ? ` by ${pr.merged_by}` : ""}.</p>
      )}
      {pr.state === "merging" && (
        <p>Being merged by <strong>{pr.merging?.by ?? "someone"}</strong>{pr.merging?.commit ? <> (commit <code className="sha">{pr.merging.commit.slice(0, 8)}</code>)</> : ""}. It cannot be edited or closed until that finishes. If the merge died, run <code>pail pr merge {pr.id} --takeover</code>.</p>
      )}
      {pr.state === "closed" && pr.closed_at && <p>Closed <Time iso={new Date(pr.closed_at * 1000).toISOString()} />{pr.closed_by ? ` by ${pr.closed_by}` : ""}.</p>}
      {pr.state === "open" && pr.draft && <p className="muted">A draft cannot be merged until it is marked ready for review.</p>}
      <h3>Policy for {pr.base}</h3>
      <ul className="policy">
        <li className={approvals >= policy.approvals ? "ok" : "no"}><Icon name={approvals >= policy.approvals ? "check" : "x"} size={14} />{approvals} of {policy.approvals} approval{policy.approvals === 1 ? "" : "s"} from someone other than the author{older > 0 && ` (${older} on an older commit)`}</li>
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
            <button className="btn" disabled={busy} onClick={() => act(() => postJson(`${base}/review`, { verdict: "approve", comment, commit: d.headCommit ?? undefined }))}>Approve</button>
            <button className="btn" disabled={busy} onClick={() => act(() => postJson(`${base}/review`, { verdict: "changes", comment, commit: d.headCommit ?? undefined }))}>Request changes</button>
          </div>
          <button className="btn btn--primary" disabled={busy || !readiness.ready} onClick={() => confirm(`Land ${pr.head} onto ${pr.base}, commit by commit?`) && act(() => postJson(`${base}/merge`, {}))}>Merge with pail land</button>
          {!readiness.ready && <p className="muted">{readiness.problems.join("; ")}.</p>}
          <div className="actions__row">
            {pr.draft
              ? <button className="btn btn--small" disabled={busy} onClick={() => act(() => postJson(`${base}/draft`, { draft: false }))}>Ready for review</button>
              : <button className="btn btn--small" disabled={busy} onClick={() => act(() => postJson(`${base}/draft`, { draft: true }))}>Convert to draft</button>}
            <button className="btn btn--small btn--danger" disabled={busy} onClick={() => act(() => postJson(`${base}/close`, {}))}>Close pull request</button>
          </div>
          {err && <p className="del" role="alert">{err}</p>}
        </div>
      )}
      {pr.state === "closed" && d.actions && (
        <div className="actions">
          <button className="btn" disabled={busy || !d.headCommit} onClick={() => act(() => postJson(`${base}/reopen`, {}))}>Reopen pull request</button>
          {!d.headCommit && <p className="muted">The branch {pr.head} no longer exists.</p>}
          {err && <p className="del" role="alert">{err}</p>}
        </div>
      )}
      {pr.state === "open" && !d.actions && <p className="muted small">Read-only. Enable <code>"actions": true</code> to review, comment and merge here.</p>}
    </section>
  );
}

/** Runs a write, reloads on success, and keeps the error to show. */
function useAct(reload: () => void) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const act = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    setErr(null);
    try { await fn(); reload(); return true; } catch (e) { setErr(e instanceof ApiError ? e.message : String(e)); return false; } finally { setBusy(false); }
  };
  return { busy, err, act };
}

function Description({ d, repo, reload }: { d: PrDetail; repo: string; reload: () => void }) {
  const { pr } = d;
  const [editing, setEditing] = useState(false);
  const [title, setTitle] = useState(pr.title);
  const [body, setBody] = useState(pr.body);
  const { busy, err, act } = useAct(reload);
  const save = async (e: FormEvent) => {
    e.preventDefault();
    if (await act(() => fetchJson(repoApi(repo, `/prs/${pr.id}`), "PATCH", { title, body }))) setEditing(false);
  };
  if (editing) {
    return (
      <section className="block compare__form" aria-label="Edit pull request">
        <form onSubmit={save} className="pr__title-edit">
          <label className="field"><span>Title</span><input className="input" value={title} maxLength={256} onChange={(e) => setTitle(e.target.value)} required /></label>
          <label className="field"><span>Description (Markdown)</span><textarea value={body} rows={8} maxLength={60000} onChange={(e) => setBody(e.target.value)} /></label>
          <div className="actions__row">
            <button type="submit" className="btn btn--primary" disabled={busy || !title.trim()}>Save</button>
            <button type="button" className="btn" onClick={() => setEditing(false)}>Cancel</button>
          </div>
          {err && <p className="del" role="alert">{err}</p>}
        </form>
      </section>
    );
  }
  return (
    <section className="block">
      {pr.body ? <Markdown text={pr.body} ctx={{ repo, ref: pr.base, dir: "" }} /> : <p className="muted">No description.</p>}
      {d.actions && <div className="actions__row actions__row--end"><button className="btn btn--small" onClick={() => { setTitle(pr.title); setBody(pr.body); setEditing(true); }}>Edit title and description</button></div>}
    </section>
  );
}

function Comments({ d, repo, reload }: { d: PrDetail; repo: string; reload: () => void }) {
  const post = usePost(d, repo, reload);
  const threads = useMemo(() => groupThreads(d.comments), [d.comments]);
  const plain = threads.filter((t) => !t.root.path);
  const onLines = threads.length - plain.length;
  const outdated = threads.filter((t) => t.root.path && t.root.outdated).length;
  const ctx = { actions: d.actions, post, md: { repo, ref: d.pr.base, dir: "" } };
  return (
    <section className="block" aria-label="Comments">
      <h2>Comments <span className="muted">{d.comments.length || ""}</span></h2>
      {plain.length === 0 ? <p className="muted">No comments yet.</p> : (
        <ul className="comments">
          {plain.map((t) => <li key={t.id}><ThreadView t={t} ctx={ctx} /></li>)}
        </ul>
      )}
      {onLines > 0 && (
        <p className="muted">
          {onLines} line comment {onLines === 1 ? "thread" : "threads"}{outdated > 0 && ` (${outdated} outdated)`}: see <Link to="?tab=changes">Changes</Link>.
        </p>
      )}
      {d.actions && <CommentForm label="Comment" placeholder="Leave a comment (Markdown)" submit={async (body) => post({ body })} />}
    </section>
  );
}
