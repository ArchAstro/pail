/** Subsequence match with a score favouring the file name and consecutive runs; null: no match. */
export function fuzzyScore(query: string, text: string): number | null {
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  if (!q) return 0;
  const base = t.lastIndexOf("/") + 1;
  let score = 0;
  let ti = 0;
  let prev = -2;
  for (const ch of q) {
    const at = t.indexOf(ch, ti);
    if (at < 0) return null;
    score += at === prev + 1 ? 6 : 1;
    if (at === 0 || "/._-".includes(t[at - 1])) score += 4;
    if (at >= base) score += 2;
    prev = at;
    ti = at + 1;
  }
  return score - text.length * 0.02;
}

export function fuzzyFilter<T>(items: T[], query: string, key: (t: T) => string, limit = 50): T[] {
  if (!query) return items.slice(0, limit);
  const scored: [number, T][] = [];
  for (const it of items) {
    const s = fuzzyScore(query, key(it));
    if (s !== null) scored.push([s, it]);
  }
  return scored.sort((a, b) => b[0] - a[0]).slice(0, limit).map((x) => x[1]);
}
