import { useEffect, useMemo, useState } from "react";
import { highlightLines, languageFor, escapeHtml } from "../lib/highlight";

const CHUNK = 2500;

/** Numbered, highlighted source. `#L12` or `#L12-L20` in the URL marks lines. */
export function CodeView({ text, path }: { text: string; path: string }) {
  const lines = useMemo(() => {
    const l = text.split("\n");
    if (l[l.length - 1] === "") l.pop();
    return l;
  }, [text]);
  const [html, setHtml] = useState<string[] | null>(null);
  const [shown, setShown] = useState(CHUNK);
  const [mark, setMark] = useState<[number, number] | null>(() => parseHash(typeof location !== "undefined" ? location.hash : ""));

  useEffect(() => {
    let live = true;
    setHtml(null);
    if (lines.length > 12000) return;
    highlightLines(text, languageFor(path)).then((h) => live && setHtml(h));
    return () => { live = false; };
  }, [text, path, lines.length]);

  useEffect(() => {
    const on = () => setMark(parseHash(location.hash));
    window.addEventListener("hashchange", on);
    return () => window.removeEventListener("hashchange", on);
  }, []);

  useEffect(() => {
    if (!mark) return;
    if (mark[0] > shown) setShown(mark[0] + 200);
    document.getElementById(`L${mark[0]}`)?.scrollIntoView({ block: "center" });
    // Only on first mark: later clicks already have the line in view.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [html === null]);

  const click = (n: number, e: React.MouseEvent) => {
    const next: [number, number] = e.shiftKey && mark ? [Math.min(mark[0], n), Math.max(mark[1], n)] : [n, n];
    history.replaceState(null, "", `#L${next[0]}${next[1] !== next[0] ? `-L${next[1]}` : ""}`);
    setMark(next);
  };

  const width = String(lines.length).length;
  return (
    <div className="src" style={{ ["--ln-w" as string]: `${width + 1}ch` }}>
      {lines.slice(0, shown).map((line, i) => {
        const n = i + 1;
        const on = mark && n >= mark[0] && n <= mark[1];
        return (
          <div key={n} id={`L${n}`} className={`src__row${on ? " is-marked" : ""}`}>
            <a className="src__n" href={`#L${n}`} onClick={(e) => { e.preventDefault(); click(n, e); }} aria-label={`Line ${n}`}>{n}</a>
            <span className="code" dangerouslySetInnerHTML={{ __html: (html ? html[i] : escapeHtml(line)) || " " }} />
          </div>
        );
      })}
      {shown < lines.length && (
        <button className="btn src__more" onClick={() => setShown(shown + CHUNK * 2)}>Show {Math.min(CHUNK * 2, lines.length - shown)} more lines ({lines.length - shown} left)</button>
      )}
    </div>
  );
}

export function parseHash(h: string): [number, number] | null {
  const m = /^#L(\d+)(?:-L(\d+))?$/.exec(h);
  if (!m) return null;
  const a = Number(m[1]);
  const b = m[2] ? Number(m[2]) : a;
  return [Math.min(a, b), Math.max(a, b)];
}
