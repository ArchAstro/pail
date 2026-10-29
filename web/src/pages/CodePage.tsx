import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import type { BlobView, LogPage, TreeListing } from "../../shared/types";
import { q, repoApi, useGet } from "../api";
import { CodeView } from "../components/CodeView";
import { CopyButton, ErrorNote, Loading, Tabs, Time, useDocumentTitle } from "../components/Bits";
import { Icon } from "../components/Icon";
import { Markdown } from "../components/Markdown";
import { RefPicker } from "../components/RefPicker";
import { TreeTable } from "../components/TreeTable";
import { Avatar } from "../components/Avatar";
import { bytes } from "../lib/format";
import { blobPath, commitPath, commitsPath, rawUrl, treePath } from "../lib/routes";
import { useCurrentRef, useRepo } from "../repoContext";

export default function CodePage({ kind }: { kind: "tree" | "blob" }) {
  const { repo, overview, resolve } = useRepo();
  const params = useParams();
  const nav = useNavigate();
  const { ref, path } = resolve(params["*"] ?? "");
  useCurrentRef(ref);
  useDocumentTitle(`${path || repo} at ${ref.slice(0, 12)}`);
  const crumbs = path.split("/").filter(Boolean);

  const switchRef = (r: string) => nav(kind === "tree" ? treePath(repo, r, path) : blobPath(repo, r, path));
  return (
    <div className="codepage">
      <div className="code__bar">
        <RefPicker current={ref} branches={overview.branches} defaultBranch={overview.default} onPick={switchRef} />
        <nav className="crumbs" aria-label="Path">
          <Link to={treePath(repo, ref)}>{repo}</Link>
          {crumbs.map((c, i) => {
            const p = crumbs.slice(0, i + 1).join("/");
            const last = i === crumbs.length - 1;
            return (
              <span key={p}>
                <span className="crumbs__sep" aria-hidden="true">/</span>
                {last && kind === "blob" ? <strong aria-current="page">{c}</strong> : <Link to={treePath(repo, ref, p)}>{c}</Link>}
              </span>
            );
          })}
        </nav>
        <span className="top__fill" />
        <Link className="btn btn--small" to={commitsPath(repo, ref, path)}><Icon name="clock" size={14} />History</Link>
      </div>
      {kind === "tree" ? <TreeBody repo={repo} refName={ref} path={path} /> : <BlobBody repo={repo} refName={ref} path={path} />}
    </div>
  );
}

function LatestCommit({ repo, refName, path }: { repo: string; refName: string; path: string }) {
  const log = useGet<LogPage>(repoApi(repo, `/log${q({ ref: refName, path, limit: 1 })}`));
  const c = log.data?.commits[0];
  if (!c) return <div className="latest latest--skeleton"><span className="skeleton" /></div>;
  return (
    <div className="latest">
      <Avatar name={c.author.name} size={18} />
      <strong>{c.author.name}</strong>
      <Link to={commitPath(repo, c.id)} className="latest__subject">{c.subject}</Link>
      <code className="sha">{c.id.slice(0, 8)}</code>
      <Time iso={c.date} />
    </div>
  );
}

function TreeBody({ repo, refName, path }: { repo: string; refName: string; path: string }) {
  const tree = useGet<TreeListing>(repoApi(repo, `/tree${q({ ref: refName, path })}`));
  const readmeName = tree.data?.entries.find((e) => e.type === "blob" && /^readme(\.(md|markdown|txt))?$/i.test(e.name))?.name;
  const readme = useGet<BlobView>(readmeName ? repoApi(repo, `/blob${q({ ref: refName, path: path ? `${path}/${readmeName}` : readmeName })}`) : null);
  if (tree.error) return <ErrorNote error={tree.error} retry={tree.reload} />;
  if (!tree.data) return <Loading />;
  return (
    <>
      <LatestCommit repo={repo} refName={refName} path={path} />
      <TreeTable repo={repo} listing={tree.data} refName={refName} />
      {readme.data?.text && (
        <section className="readme">
          <h2 className="readme__title"><Icon name="file" size={15} />{readmeName}</h2>
          <Markdown text={readme.data.text} ctx={{ repo, ref: refName, dir: path }} />
        </section>
      )}
    </>
  );
}

function BlobBody({ repo, refName, path }: { repo: string; refName: string; path: string }) {
  const blob = useGet<BlobView>(repoApi(repo, `/blob${q({ ref: refName, path })}`));
  const isMd = /\.(md|markdown)$/i.test(path);
  const [view, setView] = useState<"preview" | "code">("preview");
  if (blob.error) return <ErrorNote error={blob.error} retry={blob.reload} />;
  const b = blob.data;
  if (!b) return <Loading />;
  const lines = b.text ? b.text.split("\n").length - (b.text.endsWith("\n") ? 1 : 0) : 0;
  return (
    <>
      <LatestCommit repo={repo} refName={refName} path={path} />
      <section className="file file--blob" aria-label={path}>
        <header className="file__head">
          <span className="file__path">{path.split("/").pop()}</span>
          <span className="muted">{b.text !== undefined ? `${lines} lines · ` : ""}{bytes(b.size)}</span>
          <span className="top__fill" />
          {isMd && b.text !== undefined && <Tabs label="View" value={view} onChange={setView} tabs={[{ id: "preview", label: "Preview" }, { id: "code", label: "Code" }]} />}
          {b.text !== undefined && <CopyButton text={b.text} label="Copy file contents" />}
          <a className="btn btn--small" href={rawUrl(repo, refName, path)} target="_blank" rel="noreferrer">Raw</a>
          <a className="btn btn--small" href={rawUrl(repo, refName, path, true)} download><Icon name="download" size={14} />Download</a>
        </header>
        <div className="file__body">
          {b.image ? <div className="imgwrap"><img src={rawUrl(repo, refName, path)} alt={path} /></div>
            : b.tooLarge ? <p className="muted pad">This file is {bytes(b.size)}, too large to show here. Use Raw or Download.</p>
            : b.binary ? <p className="muted pad">Binary file ({bytes(b.size)}). Use Download.</p>
            : isMd && view === "preview" ? <div className="pad"><Markdown text={b.text ?? ""} ctx={{ repo, ref: refName, dir: path.split("/").slice(0, -1).join("/") }} /></div>
            : <CodeView text={b.text ?? ""} path={path} />}
        </div>
      </section>
    </>
  );
}

