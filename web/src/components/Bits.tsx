import { useEffect, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import type { PrRecord } from "../../shared/types";
import { Icon } from "./Icon";
import { fullDate, timeAgo } from "../lib/format";

export function Time({ iso }: { iso: string }) {
  return <time dateTime={iso} title={fullDate(iso)}>{timeAgo(iso)}</time>;
}

export function Loading({ label = "Loading" }: { label?: string }) {
  return (
    <div className="loading" role="status" aria-live="polite">
      <span className="loading__bar" />
      <span className="sr-only">{label}</span>
    </div>
  );
}

export function ErrorNote({ error, retry }: { error: Error; retry?: () => void }) {
  return (
    <div className="note note--error" role="alert">
      <strong>Could not load this.</strong> <span>{error.message}</span>
      {retry && <button className="btn btn--small" onClick={retry}>Retry</button>}
    </div>
  );
}

export function Empty({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="empty">
      <p className="empty__title">{title}</p>
      {children && <p className="empty__body">{children}</p>}
    </div>
  );
}

export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [done, setDone] = useState(false);
  useEffect(() => {
    if (!done) return;
    const t = setTimeout(() => setDone(false), 1400);
    return () => clearTimeout(t);
  }, [done]);
  return (
    <button
      className="icon-btn"
      aria-label={done ? "Copied" : label}
      title={done ? "Copied" : label}
      onClick={() => navigator.clipboard?.writeText(text).then(() => setDone(true), () => {})}
    >
      <Icon name={done ? "check" : "copy"} size={14} />
    </button>
  );
}

export function Sha({ id, to }: { id: string; to?: string }) {
  const inner = <code className="sha">{id.slice(0, 8)}</code>;
  return to ? <Link to={to} className="sha-link">{inner}</Link> : inner;
}

export function PrState({ state, draft }: { state: PrRecord["state"]; draft?: boolean }) {
  const shown = state === "open" && draft ? "draft" : state;
  return <span className={`pill pill--${shown}`}><Icon name={state === "merged" || state === "merging" ? "merge" : "pr"} size={13} />{shown}</span>;
}

export function Tabs<T extends string>({ tabs, value, onChange, label }: { tabs: { id: T; label: string; count?: number }[]; value: T; onChange: (t: T) => void; label: string }) {
  return (
    <div className="seg" role="tablist" aria-label={label}>
      {tabs.map((t) => (
        <button key={t.id} role="tab" aria-selected={value === t.id} className={value === t.id ? "is-on" : ""} onClick={() => onChange(t.id)}>
          {t.label}
          {t.count !== undefined && <span className="seg__n">{t.count}</span>}
        </button>
      ))}
    </div>
  );
}

export function useDocumentTitle(title: string) {
  useEffect(() => {
    document.title = title ? `${title} · pail` : "pail";
  }, [title]);
}
