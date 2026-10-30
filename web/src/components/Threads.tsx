import { createContext, useContext, useState, type FormEvent } from "react";
import type { NewComment, PrComment } from "../../shared/types";
import { ApiError } from "../api";
import { Avatar } from "./Avatar";
import { Time } from "./Bits";
import { Markdown } from "./Markdown";

/** A comment and the replies in its thread, in id (time) order. */
export interface Thread { id: string; root: PrComment; replies: PrComment[] }

export function groupThreads(comments: PrComment[]): Thread[] {
  const byId = new Map<string, Thread>();
  const sorted = [...comments].sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  for (const c of sorted) if (c.thread === c.id) byId.set(c.id, { id: c.id, root: c, replies: [] });
  for (const c of sorted) if (c.thread !== c.id) byId.get(c.thread)?.replies.push(c);
  return [...byId.values()];
}

/** Where a diff's lines can be commented on, and how: given by the pull request page. */
export interface InlineCtx {
  threads: Thread[];
  /** Replies and new comments (the `actions` setting). */
  actions: boolean;
  /** A new line comment needs a PR that is not merged. */
  anchorable: boolean;
  /** The head commit the diff was read at: what a new line comment says it was read at. */
  commit: string | null;
  post: (c: NewComment) => Promise<void>;
  md: { repo: string; ref: string; dir: string };
}

export const InlineContext = createContext<InlineCtx | null>(null);
export const useInline = () => useContext(InlineContext);

export const lineKey = (side: "new" | "old", line: number) => `${side}:${line}`;

export function CommentForm({ label, placeholder, submit, onCancel, autoFocus }: { label: string; placeholder: string; submit: (body: string) => Promise<void>; onCancel?: () => void; autoFocus?: boolean }) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const send = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    try {
      await submit(text);
      setText("");
    } catch (x) {
      setErr(x instanceof ApiError ? x.message : String(x));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form className="commentbox" aria-label={label} onSubmit={send}>
      <label className="field"><span className="sr-only">{label}</span><textarea value={text} onChange={(e) => setText(e.target.value)} rows={3} maxLength={60000} placeholder={placeholder} autoFocus={autoFocus} /></label>
      <div className="actions__row actions__row--end">
        {onCancel && <button type="button" className="btn btn--small" onClick={onCancel}>Cancel</button>}
        <button type="submit" className="btn btn--small" disabled={busy || !text.trim()}>{label}</button>
      </div>
      {err && <p className="del" role="alert">{err}</p>}
    </form>
  );
}

export function CommentItem({ c, md }: { c: PrComment; md: InlineCtx["md"] }) {
  return (
    <div className="comment">
      <Avatar name={c.who} size={24} />
      <div>
        <p className="comment__who"><strong>{c.who}</strong> {c.at && <Time iso={new Date(c.at * 1000).toISOString()} />}</p>
        <Markdown text={c.body} ctx={md} />
      </div>
    </div>
  );
}

/** A thread: its comments, then a Reply button (or the reply form) when replies are on. */
export function ThreadView({ t, ctx, where }: { t: Thread; ctx: Pick<InlineCtx, "actions" | "post" | "md">; where?: string }) {
  const [replying, setReplying] = useState(false);
  const outdated = t.root.outdated;
  return (
    <div className={`thread${outdated ? " is-outdated" : ""}`} data-thread={t.id}>
      {(outdated || where) && (
        <p className="thread__meta">
          {outdated && <span className="pill pill--outdated">Outdated</span>}
          {where && <code>{where}</code>}
        </p>
      )}
      {[t.root, ...t.replies].map((c) => <CommentItem key={c.id} c={c} md={ctx.md} />)}
      {ctx.actions && (replying
        ? <CommentForm label="Reply" placeholder="Reply (Markdown)" autoFocus onCancel={() => setReplying(false)} submit={async (body) => { await ctx.post({ body, in_reply_to: t.id }); setReplying(false); }} />
        : <div className="thread__reply"><button className="btn btn--small" onClick={() => setReplying(true)}>Reply</button></div>)}
    </div>
  );
}
