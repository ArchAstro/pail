/** A byte-budgeted LRU plus request coalescing: concurrent callers of one key share one read. */
export class Cache {
  private map = new Map<string, { v: unknown; size: number }>();
  private inflight = new Map<string, Promise<unknown>>();
  private bytes = 0;
  hits = 0;
  misses = 0;

  constructor(private maxBytes = 128 * 1024 * 1024) {}

  /** Immutable entries (named by commit hash): cached until evicted. */
  async get<T>(key: string, load: () => Promise<T>, sizeOf: (v: T) => number = approxSize): Promise<T> {
    const hit = this.map.get(key);
    if (hit) {
      this.map.delete(key);
      this.map.set(key, hit);
      this.hits++;
      return hit.v as T;
    }
    const pending = this.inflight.get(key);
    if (pending) return pending as Promise<T>;
    this.misses++;
    const p = load()
      .then((v) => {
        this.put(key, v, sizeOf(v));
        return v;
      })
      .finally(() => this.inflight.delete(key));
    this.inflight.set(key, p);
    return p;
  }

  /** Mutable entries (refs): reused for `ttlMs`, and dropped by `invalidate`. */
  async ttl<T>(key: string, ttlMs: number, load: () => Promise<T>): Promise<T> {
    const hit = this.map.get(key) as { v: T; size: number; at?: number } | undefined;
    if (hit && hit.at !== undefined && Date.now() - hit.at < ttlMs) return hit.v;
    if (hit) this.drop(key);
    const pending = this.inflight.get(key);
    if (pending) return pending as Promise<T>;
    const p = load()
      .then((v) => {
        this.put(key, v, 256, Date.now());
        return v;
      })
      .finally(() => this.inflight.delete(key));
    this.inflight.set(key, p);
    return p;
  }

  invalidate(prefix: string) {
    for (const k of [...this.map.keys()]) if (k.startsWith(prefix)) this.drop(k);
    for (const k of [...this.inflight.keys()]) if (k.startsWith(prefix)) this.inflight.delete(k);
  }

  stats() {
    return { entries: this.map.size, bytes: this.bytes, hits: this.hits, misses: this.misses };
  }

  private drop(key: string) {
    const e = this.map.get(key);
    if (e) {
      this.bytes -= e.size;
      this.map.delete(key);
    }
  }

  private put(key: string, v: unknown, size: number, at?: number) {
    this.drop(key);
    this.map.set(key, Object.assign({ v, size }, at === undefined ? {} : { at }));
    this.bytes += size;
    while (this.bytes > this.maxBytes && this.map.size > 1) {
      const oldest = this.map.keys().next().value as string;
      this.drop(oldest);
    }
  }
}

export function approxSize(v: unknown): number {
  if (v === null || v === undefined) return 16;
  if (Buffer.isBuffer(v)) return v.length + 64;
  if (typeof v === "string") return v.length * 2 + 64;
  try {
    return JSON.stringify(v).length * 2 + 64;
  } catch {
    return 1024;
  }
}
