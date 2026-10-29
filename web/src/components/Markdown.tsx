import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { codeLanguage, renderMarkdown, type MdContext } from "../lib/markdown";
import { highlightBlock } from "../lib/highlight";

export function Markdown({ text, ctx, className = "" }: { text: string; ctx: MdContext; className?: string }) {
  const [html, setHtml] = useState<string | null>(null);
  const ref = useRef<HTMLDivElement>(null);
  const nav = useNavigate();
  useEffect(() => {
    let live = true;
    renderMarkdown(text, ctx).then((h) => live && setHtml(h));
    return () => { live = false; };
  }, [text, ctx.repo, ctx.ref, ctx.dir]);
  useEffect(() => {
    if (html === null || !ref.current) return;
    ref.current.querySelectorAll("pre code").forEach(async (el) => {
      const lang = codeLanguage(el);
      if (!lang) return;
      const out = await highlightBlock(el.textContent ?? "", lang);
      if (out && ref.current?.contains(el)) el.innerHTML = out;
    });
  }, [html]);
  return (
    <div
      ref={ref}
      className={`md ${className}`}
      onClick={(e) => {
        const a = (e.target as HTMLElement).closest("a[data-internal]");
        if (a && !(e.metaKey || e.ctrlKey || e.shiftKey)) {
          e.preventDefault();
          nav(a.getAttribute("href")!);
        }
      }}
      dangerouslySetInnerHTML={{ __html: html ?? "" }}
    />
  );
}
