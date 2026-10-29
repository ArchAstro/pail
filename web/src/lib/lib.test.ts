import { describe, expect, it } from "vitest";
import { parsePatch, splitRows } from "./diff";
import { dayLabel, bytes, duration, timeAgo } from "./format";
import { fuzzyFilter, fuzzyScore } from "./fuzzy";
import { splitHtmlLines, languageFor } from "./highlight";
import { blobPath, resolveRefpath, treePath } from "./routes";
import { parseHash } from "../components/CodeView";

const PATCH = `diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,5 @@ fn main
 keep
-old one
-old two
+new one
+new two
+new three
 tail
diff --git a/logo.png b/logo.png
new file mode 100644
Binary files /dev/null and b/logo.png differ
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
`;

describe("parsePatch", () => {
  const files = parsePatch(PATCH);
  it("splits files, statuses and counts", () => {
    expect(files.map((f) => [f.path, f.status, f.adds, f.dels, f.binary])).toEqual([
      ["src/a.rs", "modified", 3, 2, false],
      ["logo.png", "added", 0, 0, true],
      ["gone.txt", "deleted", 0, 1, false],
    ]);
  });
  it("numbers lines on both sides", () => {
    const l = files[0].hunks[0].lines;
    expect(l[0]).toMatchObject({ kind: "ctx", old: 1, new: 1 });
    expect(l[1]).toMatchObject({ kind: "del", old: 2 });
    expect(l[3]).toMatchObject({ kind: "add", new: 2 });
    expect(l[l.length - 1]).toMatchObject({ kind: "ctx", old: 4, new: 5, text: "tail" });
  });
  it("pairs deletions with additions for the split view", () => {
    const rows = splitRows(files[0].hunks[0].lines);
    expect(rows).toHaveLength(5);
    expect(rows[1].left?.text).toBe("old one");
    expect(rows[1].right?.text).toBe("new one");
    expect(rows[3].left).toBeUndefined();
    expect(rows[3].right?.text).toBe("new three");
  });
  it("returns nothing for empty text", () => expect(parsePatch("")).toEqual([]));
});

describe("resolveRefpath", () => {
  const branches = [{ name: "main" }, { name: "pr/x" }, { name: "pr/x/deep" }];
  it("picks the longest branch that prefixes the path (branch names contain slashes)", () => {
    expect(resolveRefpath("pr/x/deep/src/a.rs", branches, "main")).toEqual({ ref: "pr/x/deep", path: "src/a.rs" });
    expect(resolveRefpath("pr/x/src/a.rs", branches, "main")).toEqual({ ref: "pr/x", path: "src/a.rs" });
    expect(resolveRefpath("main", branches, "main")).toEqual({ ref: "main", path: "" });
  });
  it("accepts commit ids and falls back to the default branch", () => {
    const id = "a".repeat(64);
    expect(resolveRefpath(`${id}/README.md`, branches, "main")).toEqual({ ref: id, path: "README.md" });
    expect(resolveRefpath("", branches, "main")).toEqual({ ref: "main", path: "" });
    expect(resolveRefpath("unknown/file.txt", branches, "main")).toEqual({ ref: "main", path: "unknown/file.txt" });
  });
  it("builds URLs that round-trip", () => {
    expect(blobPath("r", "pr/x", "dir/a b.rs")).toBe("/r/blob/pr/x/dir/a%20b.rs");
    expect(treePath("r", "main")).toBe("/r/tree/main");
    expect(resolveRefpath(decodeURIComponent("pr/x/dir/a%20b.rs"), branches, "main").path).toBe("dir/a b.rs");
  });
});

describe("fuzzy", () => {
  it("matches subsequences and prefers file names", () => {
    expect(fuzzyScore("xyz", "abc")).toBeNull();
    expect(fuzzyScore("cl", "src/claim.rs")!).toBeGreaterThan(fuzzyScore("cl", "src/clientside/x.rs")! - 100);
    const files = ["src/lease.rs", "docs/design.md", "src/claim.rs", "web/queue.ts"];
    expect(fuzzyFilter(files, "claim", (f) => f)[0]).toBe("src/claim.rs");
    expect(fuzzyFilter(files, "wq", (f) => f)).toEqual(["web/queue.ts"]);
    expect(fuzzyFilter(files, "", (f) => f, 2)).toHaveLength(2);
  });
});

describe("format", () => {
  const now = new Date("2026-10-29T12:00:00Z").getTime();
  it("describes time, size and duration", () => {
    expect(timeAgo("2026-10-29T11:59:50Z", now)).toBe("just now");
    expect(timeAgo("2026-10-29T09:00:00Z", now)).toBe("3h ago");
    expect(timeAgo("2026-10-22T12:00:00Z", now)).toBe("1w ago");
    expect(bytes(512)).toBe("512 B");
    expect(bytes(2048)).toBe("2.0 KB");
    expect(duration(3.41)).toBe("3.4s");
    expect(duration(125)).toBe("2m 5s");
    expect(dayLabel("2026-10-29T08:00:00", new Date("2026-10-29T12:00:00"))).toBe("Today");
  });
});

describe("highlight helpers", () => {
  it("keeps spans balanced across lines", () => {
    expect(splitHtmlLines('<span class="c">a\nb</span>\nc')).toEqual(['<span class="c">a</span>', '<span class="c">b</span>', "c"]);
  });
  it("chooses grammars by file name", () => {
    expect(languageFor("src/main.rs")).toBe("rust");
    expect(languageFor("Dockerfile")).toBe("dockerfile");
    expect(languageFor("notes.unknownext")).toBeNull();
  });
  it("parses line marks from the URL hash", () => {
    expect(parseHash("#L12")).toEqual([12, 12]);
    expect(parseHash("#L20-L12")).toEqual([12, 20]);
    expect(parseHash("#other")).toBeNull();
  });
});
