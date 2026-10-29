import { memo, useEffect, useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { parsePatch, splitRows, type DiffLine, type FileDiff, type Hunk } from "../lib/diff";
import { highlightLines, languageFor } from "../lib/highlight";
import { blobPath } from "../lib/routes";
import { Tabs } from "./Bits";
import { Icon } from "./Icon";

type Mode = "unified" | "split";

function storedMode(): Mode {
  try { return localStorage.getItem("pail-diff-mode") === "split" ? "split" : "unified"; } catch { return "unified"; }
}

export function DiffView({ patch, repo, blobRef, truncated }: { patch: string; repo: string; blobRef?: string; truncated?: boolean }) {
  const [mode, setMode] = useState<Mode>(storedMode);
  const files = useMemo(() => parsePatch(patch), [patch]);
  const totals = useMemo(() => files.reduce((a, f) => ({ adds: a.adds + f.adds, dels: a.dels + f.dels }), { adds: 0, dels: 0 }), [files]);
  const set = (m: Mode) => {
    setMode(m);
    try { localStorage.setItem("pail-diff-mode", m); } catch { /* ignore */ }
  };
  if (!files.length) return <p className="muted pad">No file changes.</p>;
  const heavy = totals.adds + totals.dels > 4000;
  return (
    <section className="diff" aria-label="Changes">
      <header className="diff__bar">
        <span className="diff__summary">
          {files.length} {files.length === 1 ? "file" : "files"} changed
          <span className="add"> +{totals.adds}</span> <span className="del">−{totals.dels}</span>
        </span>
        <Tabs label="Diff layout" value={mode} onChange={set} tabs={[{ id: "unified", label: "Unified" }, { id: "split", label: "Split" }]} />
      </header>
      {files.length > 1 && (
        <nav className="diff__files" aria-label="Files changed">
          {files.map((f) => (
            <a key={f.path} href={`#${anchor(f.path)}`} className={`diff__chip is-${f.status}`}>
              <span className="diff__chip-mark">{f.status === "added" ? "A" : f.status === "deleted" ? "D" : "M"}</span>
              {f.path}
            </a>
          ))}
        </nav>
      )}
      {files.map((f, i) => (
        <FileBlock key={f.path} file={f} mode={mode} repo={repo} blobRef={blobRef} startOpen={!heavy && i < 40 && f.adds + f.dels <= 800} />
      ))}
      {truncated && <p className="note">This diff lists the first 300 changed files only.</p>}
    </section>
  );
}

const anchor = (p: string) => "diff-" + p.replace(/[^\w]+/g, "-");

const FileBlock = memo(function FileBlock({ file, mode, repo, blobRef, startOpen }: { file: FileDiff; mode: Mode; repo: string; blobRef?: string; startOpen: boolean }) {
  const [open, setOpen] = useState(startOpen);
  const body = `body-${anchor(file.path)}`;
  return (
    <article className="file" id={anchor(file.path)}>
      <header className="file__head">
        <button className="file__toggle" aria-expanded={open} aria-controls={body} onClick={() => setOpen(!open)}>
          <Icon name="chevron" size={14} className={open ? "rot" : ""} />
          <span className="sr-only">{open ? "Collapse" : "Expand"}</span>
        </button>
        <span className={`file__status is-${file.status}`}>{file.status}</span>
        {blobRef && file.status !== "deleted" ? <Link className="file__path" to={blobPath(repo, blobRef, file.path)}>{file.path}</Link> : <span className="file__path">{file.path}</span>}
        {file.status === "renamed" && <span className="muted"> from {file.oldPath}</span>}
        <span className="file__stat"><span className="add">+{file.adds}</span> <span className="del">−{file.dels}</span></span>
      </header>
      {open && (
        <div id={body} className="file__body">
          {file.binary ? <p className="muted pad">Binary file not shown.</p> : file.hunks.length === 0 ? <p className="muted pad">No textual changes{file.status === "added" ? " (empty file)" : ""}.</p> : file.hunks.map((h, i) => <HunkView key={i} hunk={h} path={file.path} mode={mode} />)}
        </div>
      )}
      {!open && <button className="file__more" onClick={() => setOpen(true)}>Show {file.adds + file.dels} changed lines</button>}
    </article>
  );
});

/** Highlights each side of a hunk as one text so multi-line strings and comments stay intact. */
function useHunkHighlight(hunk: Hunk, path: string) {
  const [html, setHtml] = useState<{ old: string[]; new: string[] } | null>(null);
  useEffect(() => {
    let live = true;
    const lang = languageFor(path);
    if (!lang) return;
    const side = (k: "old" | "new") => hunk.lines.filter((l) => (k === "old" ? l.kind !== "add" : l.kind !== "del")).map((l) => l.text).join("\n");
    Promise.all([highlightLines(side("old"), lang), highlightLines(side("new"), lang)]).then(([o, n]) => live && o && n && setHtml({ old: o, new: n }));
    return () => { live = false; };
  }, [hunk, path]);
  return useMemo(() => {
    if (!html) return null;
    let oi = 0;
    let ni = 0;
    return hunk.lines.map((l) => (l.kind === "add" ? html.new[ni++] : l.kind === "del" ? html.old[oi++] : (oi++, html.new[ni++])));
  }, [html, hunk]);
}

function Code({ html, text }: { html?: string; text: string }) {
  return html !== undefined ? <span className="code" dangerouslySetInnerHTML={{ __html: html || " " }} /> : <span className="code">{text || " "}</span>;
}

function HunkView({ hunk, path, mode }: { hunk: Hunk; path: string; mode: Mode }) {
  const hl = useHunkHighlight(hunk, path);
  const index = useMemo(() => new Map(hunk.lines.map((l, i) => [l, i])), [hunk]);
  const at = (l?: DiffLine) => (l && hl ? hl[index.get(l)!] : undefined);
  const header = /@@[^@]*@@(.*)$/.exec(hunk.header)?.[1]?.trim();
  if (mode === "split") {
    const rows = splitRows(hunk.lines);
    return (
      <div className="hunk hunk--split">
        <div className="hunk__head">{hunk.header.split("@@")[1] ? `@@${hunk.header.split("@@")[1]}@@` : ""} {header}</div>
        {rows.map((r, i) => (
          <div className="row row--split" key={i}>
            <Side line={r.left} html={at(r.left)} num={r.left?.old} />
            <Side line={r.right} html={at(r.right)} num={r.right?.new} />
          </div>
        ))}
      </div>
    );
  }
  return (
    <div className="hunk">
      <div className="hunk__head">{hunk.header.split("@@")[1] ? `@@${hunk.header.split("@@")[1]}@@` : ""} {header}</div>
      {hunk.lines.map((l, i) => (
        <div className={`row row--${l.kind}`} key={i}>
          <span className="ln" aria-hidden="true">{l.old ?? ""}</span>
          <span className="ln" aria-hidden="true">{l.new ?? ""}</span>
          <span className="sign" aria-label={l.kind === "add" ? "added" : l.kind === "del" ? "removed" : undefined}>{l.kind === "add" ? "+" : l.kind === "del" ? "−" : " "}</span>
          <Code html={at(l)} text={l.text} />
        </div>
      ))}
    </div>
  );
}

function Side({ line, html, num }: { line?: DiffLine; html?: string; num?: number }) {
  if (!line) return <div className="side side--empty"><span className="ln" /><span className="code" /></div>;
  return (
    <div className={`side side--${line.kind}`}>
      <span className="ln" aria-hidden="true">{num}</span>
      <Code html={html} text={line.text} />
    </div>
  );
}
