import { useEffect, useRef, useState } from "react";
import type { BranchInfo } from "../../shared/types";
import { Icon } from "./Icon";
import { CheckBadge } from "./Checks";

export function RefPicker({ current, branches, defaultBranch, onPick, label = "Switch branch or commit" }: { current: string; branches: BranchInfo[]; defaultBranch: string; onPick: (ref: string) => void; label?: string }) {
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState("");
  const [active, setActive] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const isCommit = /^[0-9a-f]{64}$/.test(current);
  const list = branches.filter((b) => b.name.toLowerCase().includes(filter.toLowerCase())).slice(0, 80);
  const asCommit = /^[0-9a-f]{64}$/i.test(filter.trim()) ? filter.trim().toLowerCase() : null;
  const items = [...(asCommit ? [{ name: asCommit, commit: true }] : []), ...list.map((b) => ({ name: b.name, commit: false }))];

  useEffect(() => {
    if (!open) return;
    setFilter("");
    setActive(0);
    input.current?.focus();
    const off = (e: MouseEvent) => !root.current?.contains(e.target as Node) && setOpen(false);
    document.addEventListener("mousedown", off);
    return () => document.removeEventListener("mousedown", off);
  }, [open]);

  const pick = (name: string) => {
    setOpen(false);
    onPick(name);
  };

  return (
    <div className="picker" ref={root}>
      <button className="btn btn--ref" aria-haspopup="listbox" aria-expanded={open} aria-label={`${label}: ${current}`} onClick={() => setOpen(!open)}>
        <Icon name={isCommit ? "commit" : "branch"} size={15} />
        <span className="btn__text">{isCommit ? current.slice(0, 8) : current}</span>
        <Icon name="down" size={13} />
      </button>
      {open && (
        <div className="pop" role="dialog" aria-label={label}>
          <input
            ref={input}
            className="pop__input"
            placeholder="Find a branch, or paste a commit id"
            value={filter}
            role="combobox"
            aria-expanded="true"
            aria-controls="ref-list"
            aria-activedescendant={items[active] ? `ref-opt-${active}` : undefined}
            onChange={(e) => { setFilter(e.target.value); setActive(0); }}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") { e.preventDefault(); setActive(Math.min(active + 1, items.length - 1)); }
              else if (e.key === "ArrowUp") { e.preventDefault(); setActive(Math.max(active - 1, 0)); }
              else if (e.key === "Enter" && items[active]) pick(items[active].name);
              else if (e.key === "Escape") { setOpen(false); }
            }}
          />
          <ul id="ref-list" role="listbox" className="pop__list">
            {items.map((it, i) => {
              const b = branches.find((x) => x.name === it.name);
              return (
                <li key={it.name} id={`ref-opt-${i}`} role="option" aria-selected={it.name === current} className={i === active ? "is-active" : ""} onMouseEnter={() => setActive(i)} onClick={() => pick(it.name)}>
                  <Icon name={it.commit ? "commit" : "branch"} size={14} />
                  <span className="pop__name">{it.commit ? it.name.slice(0, 12) + " (commit)" : it.name}</span>
                  {it.name === defaultBranch && <span className="pill pill--quiet">default</span>}
                  <CheckBadge rollup={b?.checks} compact />
                  {it.name === current && <Icon name="check" size={14} />}
                </li>
              );
            })}
            {!items.length && <li className="pop__none">No branch matches “{filter}”.</li>}
          </ul>
        </div>
      )}
    </div>
  );
}
