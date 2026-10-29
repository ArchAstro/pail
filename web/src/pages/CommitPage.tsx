import { Link, useParams } from "react-router-dom";
import type { CommitDetail } from "../../shared/types";
import { repoApi, useGet } from "../api";
import { Avatar } from "../components/Avatar";
import { CheckRows } from "../components/CheckRows";
import { Provenance } from "../components/CommitList";
import { CopyButton, ErrorNote, Loading, Time, useDocumentTitle } from "../components/Bits";
import { DiffView } from "../components/DiffView";
import { fullDate } from "../lib/format";
import { commitPath, treePath } from "../lib/routes";
import { useCurrentRef, useRepo } from "../repoContext";

export default function CommitPage() {
  const { repo } = useRepo();
  const { id = "" } = useParams();
  const d = useGet<CommitDetail>(repoApi(repo, `/commit/${encodeURIComponent(id)}`));
  useCurrentRef(d.data?.commit.id ?? "");
  useDocumentTitle(d.data ? d.data.commit.subject : "Commit");
  if (d.error) return <ErrorNote error={d.error} retry={d.reload} />;
  if (!d.data) return <Loading />;
  const { commit: c, patch, checks, sources, truncated } = d.data;
  return (
    <article className="commit">
      <header className="commit__head">
        <h1>{c.subject}</h1>
        {c.maintenance && <p className="note">Maintenance commit: it changes how files are stored, not what they are.</p>}
        {c.body && <pre className="commit__body">{c.body}</pre>}
        <p className="commit__by">
          <Avatar name={c.author.name} size={22} />
          <strong>{c.author.name}</strong>
          <span className="muted">authored</span>
          <time dateTime={c.date} title={fullDate(c.date)}><Time iso={c.date} /></time>
        </p>
        <Provenance commit={c} repo={repo} sources={sources} />
        <dl className="facts">
          <dt>Commit</dt>
          <dd><code>{c.id}</code><CopyButton text={c.id} label="Copy commit id" /></dd>
          <dt>Parent</dt>
          <dd>{c.parent ? <Link to={commitPath(repo, c.parent)}><code>{c.parent}</code></Link> : <span className="muted">none, this is the first commit</span>}</dd>
          {c.refs && <><dt>Branches</dt><dd>{c.refs.map((r) => <Link key={r} className="ref" to={treePath(repo, r)}>{r}</Link>)}</dd></>}
          <dt>Files</dt>
          <dd><Link to={treePath(repo, c.id)}>Browse the tree at this commit</Link></dd>
        </dl>
      </header>
      {checks.length > 0 && (
        <section className="block" aria-label="Checks">
          <h2>Checks</h2>
          <CheckRows checks={checks} />
        </section>
      )}
      <DiffView patch={patch} repo={repo} blobRef={c.id} truncated={truncated} />
    </article>
  );
}
