import { createContext, useContext, useEffect, useRef, useState, createElement, type ReactNode } from "react";
import type { RefEvent } from "../shared/types";

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

export async function getJson<T>(url: string, signal?: AbortSignal): Promise<T> {
  const res = await fetch(url, { signal, headers: { Accept: "application/json" } });
  if (!res.ok) {
    let msg = res.statusText;
    try { msg = ((await res.json()) as { error?: string }).error ?? msg; } catch { /* not JSON */ }
    throw new ApiError(res.status, msg);
  }
  return res.json() as Promise<T>;
}

export const postJson = <T,>(url: string, body: unknown) => fetchJson<T>(url, "POST", body);

export async function fetchJson<T>(url: string, method: "POST" | "PATCH", body: unknown): Promise<T> {
  const res = await fetch(url, { method, headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  const data = (await res.json().catch(() => ({}))) as { error?: string } & T;
  if (!res.ok) throw new ApiError(res.status, data.error ?? res.statusText);
  return data;
}

export const repoApi = (repo: string, tail = "") => `/api/repos/${encodeURIComponent(repo)}${tail}`;
export const q = (o: Record<string, string | number | boolean | undefined | null>) => {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(o)) if (v !== undefined && v !== null && v !== "" && v !== false) p.set(k, v === true ? "1" : String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
};

// ---- live updates ------------------------------------------------------------------------------

interface Live { tick: number; last: RefEvent | null; connected: boolean }
const LiveCtx = createContext<Live>({ tick: 0, last: null, connected: false });

/** Subscribes to a repository's ref events over SSE; every event bumps `tick`, refetching queries. */
export function LiveProvider({ repo, children }: { repo: string; children: ReactNode }) {
  const [live, setLive] = useState<Live>({ tick: 0, last: null, connected: false });
  useEffect(() => {
    if (typeof EventSource === "undefined") return;
    const es = new EventSource(repoApi(repo, "/events"));
    es.onopen = () => setLive((l) => ({ ...l, connected: true }));
    es.onerror = () => setLive((l) => ({ ...l, connected: false }));
    es.addEventListener("ref", (e) => {
      const ev = JSON.parse((e as MessageEvent).data) as RefEvent;
      setLive((l) => ({ ...l, tick: l.tick + 1, last: ev }));
    });
    return () => es.close();
  }, [repo]);
  return createElement(LiveCtx.Provider, { value: live }, children);
}

export const useLive = () => useContext(LiveCtx);

// ---- data hook: stale-while-revalidate, refetched on ref events -----------------------------------

const memo = new Map<string, unknown>();

export interface Result<T> { data?: T; error?: Error; loading: boolean; reload: () => void }

export function useGet<T>(url: string | null, opts: { live?: boolean } = {}): Result<T> {
  const { tick } = useLive();
  const [n, setN] = useState(0);
  const [state, setState] = useState<{ url: string | null; data?: T; error?: Error }>({ url, data: url ? (memo.get(url) as T | undefined) : undefined });
  const seen = useRef(0);
  const liveTick = opts.live === false ? 0 : tick;
  useEffect(() => {
    if (!url) return;
    const ctl = new AbortController();
    const stale = memo.get(url) as T | undefined;
    setState((s) => ({ url, data: stale ?? (s.url === url ? s.data : undefined), error: undefined }));
    getJson<T>(url, ctl.signal).then(
      (data) => {
        memo.set(url, data);
        setState({ url, data });
      },
      (error) => {
        if (!ctl.signal.aborted) setState((s) => ({ url, data: s.url === url ? s.data : undefined, error }));
      },
    );
    seen.current = liveTick;
    return () => ctl.abort();
  }, [url, liveTick, n]);
  const fresh = state.url === url;
  return { data: fresh ? state.data : url ? (memo.get(url) as T | undefined) : undefined, error: fresh ? state.error : undefined, loading: !!url && (!fresh || (state.data === undefined && !state.error)), reload: () => setN((x) => x + 1) };
}

export const clearApiMemo = () => memo.clear();
