import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { repoApi, useGet } from "../api";
import { fuzzyFilter } from "../lib/fuzzy";
import { blobPath, pullPath, treePath } from "../lib/routes";
import { useRepo } from "../repoContext";
import { Icon } from "./Icon";
import type { PrSummary } from "../../shared/types";

interface Item { key: string; label: string; hint: string; icon: string; to: string }

/** `t` finds files in the current ref; `/` searches files, branches and pull requests. */
export function Palette({ mode, onClose }: { mode: "files" | "all"; onClose: () => void }) {
  const { repo, overview, currentRef } = useRepo();
  const nav = useNavigate();
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const ref = currentRef || overview.default;
  const files = useGet<string[]>(repoApi(repo, `/files?ref=${encodeURIComponent(ref)}`), { live: false });
  const prs = useGet<PrSummary[]>(mode === "all" ? repoApi(repo, "/prs") : null, { live: false });

  useEffect(() => input.current?.focus(), []);

  const items = useMemo(() => {
    const all: Item[] = [];
    if (mode === "all") {
      for (const b of overview.branches) all.push({ key: "b" + b.name, label: b.name, hint: "branch", icon: "branch", to: treePath(repo, b.name) });
      for (const p of prs.data ?? []) all.push({ key: "p" + p.id, label: `#${p.id} ${p.title}`, hint: p.state, icon: "pr", to: pullPath(repo, p.id) });
    }
    for (const f of files.data ?? []) all.push({ key: "f" + f, label: f, hint: "file", icon: "file", to: blobPath(repo, ref, f) });
    return all;
  }, [mode, overview.branches, prs.data, files.data, repo, ref]);

  const shown = useMemo(() => fuzzyFilter(items, query, (i) => i.label, 40), [items, query]);
  const go = (i?: Item) => {
    if (!i) return;
    onClose();
    nav(i.to);
  };

  return (
    <div className="scrim" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="palette" role="dialog" aria-modal="true" aria-label={mode === "files" ? "Find a file" : "Search"}>
        <div className="palette__field">
          <Icon name="search" size={16} />
          <input
            ref={input}
            value={query}
            placeholder={mode === "files" ? `Find a file in ${ref.length > 20 ? ref.slice(0, 12) : ref}` : "Search files, branches and pull requests"}
            role="combobox"
            aria-expanded="true"
            aria-controls="palette-list"
            aria-activedescendant={shown[active] ? `pal-${active}` : undefined}
            onChange={(e) => { setQuery(e.target.value); setActive(0); }}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") { e.preventDefault(); setActive(Math.min(active + 1, shown.length - 1)); }
              else if (e.key === "ArrowUp") { e.preventDefault(); setActive(Math.max(active - 1, 0)); }
              else if (e.key === "Enter") go(shown[active]);
              else if (e.key === "Escape") onClose();
            }}
          />
          <kbd>esc</kbd>
        </div>
        <ul id="palette-list" role="listbox" className="palette__list">
          {shown.map((it, i) => (
            <li key={it.key} id={`pal-${i}`} role="option" aria-selected={i === active} className={i === active ? "is-active" : ""} onMouseMove={() => setActive(i)} onClick={() => go(it)} ref={(el) => i === active && el?.scrollIntoView({ block: "nearest" })}>
              <Icon name={it.icon} size={15} />
              <span className="palette__label">{it.label}</span>
              <span className="palette__hint">{it.hint}</span>
            </li>
          ))}
          {!shown.length && <li className="palette__none">{files.loading ? "Loading files…" : `Nothing matches “${query}”.`}</li>}
        </ul>
      </div>
    </div>
  );
}
