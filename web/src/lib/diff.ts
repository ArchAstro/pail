export type LineKind = "ctx" | "add" | "del";
export interface DiffLine { kind: LineKind; old?: number; new?: number; text: string }
export interface Hunk { header: string; lines: DiffLine[] }
export interface FileDiff {
  path: string;
  oldPath: string;
  status: "added" | "deleted" | "modified" | "renamed";
  binary: boolean;
  hunks: Hunk[];
  adds: number;
  dels: number;
}

/** Parse the output of `git diff` / `pail show -p` into files, hunks and numbered lines. */
export function parsePatch(patch: string): FileDiff[] {
  const files: FileDiff[] = [];
  let f: FileDiff | null = null;
  let h: Hunk | null = null;
  let o = 0;
  let n = 0;
  for (const raw of patch.split("\n")) {
    if (raw.startsWith("diff --git ")) {
      const m = /^diff --git a\/(.*?) b\/(.*)$/.exec(raw);
      f = { path: m?.[2] ?? raw.slice(11), oldPath: m?.[1] ?? "", status: "modified", binary: false, hunks: [], adds: 0, dels: 0 };
      files.push(f);
      h = null;
      continue;
    }
    if (!f) continue;
    if (!h) {
      if (raw.startsWith("new file mode")) f.status = "added";
      else if (raw.startsWith("deleted file mode")) f.status = "deleted";
      else if (raw.startsWith("rename from ")) { f.status = "renamed"; f.oldPath = raw.slice(12); }
      else if (raw.startsWith("Binary files") || raw.startsWith("GIT binary patch")) f.binary = true;
      else if (raw.startsWith("--- ") || raw.startsWith("+++ ") || raw.startsWith("index ")) continue;
    }
    if (raw.startsWith("@@")) {
      const m = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
      o = Number(m?.[1] ?? 0);
      n = Number(m?.[2] ?? 0);
      h = { header: raw, lines: [] };
      f.hunks.push(h);
      continue;
    }
    if (!h) continue;
    const c = raw[0];
    if (c === "+") { h.lines.push({ kind: "add", new: n++, text: raw.slice(1) }); f.adds++; }
    else if (c === "-") { h.lines.push({ kind: "del", old: o++, text: raw.slice(1) }); f.dels++; }
    else if (c === " ") h.lines.push({ kind: "ctx", old: o++, new: n++, text: raw.slice(1) });
    // "\ No newline at end of file" and trailing blanks are skipped.
  }
  return files;
}

export interface SplitRow { left?: DiffLine; right?: DiffLine }

/** Pair each run of deletions with the run of additions that follows it. */
export function splitRows(lines: DiffLine[]): SplitRow[] {
  const rows: SplitRow[] = [];
  for (let i = 0; i < lines.length; ) {
    const l = lines[i];
    if (l.kind === "ctx") { rows.push({ left: l, right: l }); i++; continue; }
    const dels: DiffLine[] = [];
    const adds: DiffLine[] = [];
    while (i < lines.length && lines[i].kind === "del") dels.push(lines[i++]);
    while (i < lines.length && lines[i].kind === "add") adds.push(lines[i++]);
    for (let k = 0; k < Math.max(dels.length, adds.length); k++) rows.push({ left: dels[k], right: adds[k] });
  }
  return rows;
}
