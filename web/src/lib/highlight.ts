import type { HLJSApi } from "highlight.js";

// Languages load on demand, so a page pays only for what it shows.
const LOADERS: Record<string, () => Promise<{ default: any }>> = {
  rust: () => import("highlight.js/lib/languages/rust"),
  typescript: () => import("highlight.js/lib/languages/typescript"),
  javascript: () => import("highlight.js/lib/languages/javascript"),
  json: () => import("highlight.js/lib/languages/json"),
  markdown: () => import("highlight.js/lib/languages/markdown"),
  python: () => import("highlight.js/lib/languages/python"),
  go: () => import("highlight.js/lib/languages/go"),
  bash: () => import("highlight.js/lib/languages/bash"),
  yaml: () => import("highlight.js/lib/languages/yaml"),
  toml: () => import("highlight.js/lib/languages/ini"),
  xml: () => import("highlight.js/lib/languages/xml"),
  css: () => import("highlight.js/lib/languages/css"),
  sql: () => import("highlight.js/lib/languages/sql"),
  c: () => import("highlight.js/lib/languages/c"),
  cpp: () => import("highlight.js/lib/languages/cpp"),
  java: () => import("highlight.js/lib/languages/java"),
  ruby: () => import("highlight.js/lib/languages/ruby"),
  dockerfile: () => import("highlight.js/lib/languages/dockerfile"),
  diff: () => import("highlight.js/lib/languages/diff"),
};

const EXT: Record<string, string> = {
  rs: "rust", ts: "typescript", tsx: "typescript", mts: "typescript", js: "javascript", jsx: "javascript", mjs: "javascript", cjs: "javascript",
  json: "json", md: "markdown", markdown: "markdown", py: "python", go: "go", sh: "bash", bash: "bash", zsh: "bash", yml: "yaml", yaml: "yaml",
  toml: "toml", ini: "toml", html: "xml", htm: "xml", xml: "xml", svg: "xml", css: "css", sql: "sql", c: "c", h: "c", cc: "cpp", cpp: "cpp", hpp: "cpp",
  java: "java", rb: "ruby", diff: "diff", patch: "diff",
};

export function languageFor(path: string): string | null {
  const name = path.split("/").pop()!.toLowerCase();
  if (name === "dockerfile") return "dockerfile";
  if (name === "cargo.lock") return "toml";
  const ext = name.includes(".") ? name.split(".").pop()! : "";
  return EXT[ext] ?? null;
}

let core: Promise<HLJSApi> | null = null;
const loaded = new Set<string>();

async function hljs(lang: string): Promise<HLJSApi | null> {
  const load = LOADERS[lang];
  if (!load) return null;
  core ??= import("highlight.js/lib/core").then((m) => m.default);
  const h = await core;
  if (!loaded.has(lang)) {
    h.registerLanguage(lang, (await load()).default);
    loaded.add(lang);
  }
  return h;
}

export function escapeHtml(s: string) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/** Highlighted HTML for each line of `text`; spans never cross lines. Null: no grammar. */
export async function highlightLines(text: string, lang: string | null): Promise<string[] | null> {
  if (!lang) return null;
  const h = await hljs(lang);
  if (!h) return null;
  const html = h.highlight(text, { language: lang, ignoreIllegals: true }).value;
  return splitHtmlLines(html);
}

/** Split highlighted HTML at newlines, closing and reopening spans so each line stands alone. */
export function splitHtmlLines(html: string): string[] {
  const lines: string[] = [];
  const open: string[] = [];
  let cur = "";
  for (const part of html.split(/(<[^>]+>|\n)/)) {
    if (part === "\n") {
      lines.push(cur + "</span>".repeat(open.length));
      cur = open.join("");
    } else if (part.startsWith("</")) {
      open.pop();
      cur += part;
    } else if (part.startsWith("<")) {
      open.push(part);
      cur += part;
    } else cur += part;
  }
  lines.push(cur + "</span>".repeat(open.length));
  return lines;
}

export async function highlightBlock(code: string, lang: string): Promise<string | null> {
  const h = await hljs(lang);
  return h ? h.highlight(code, { language: lang, ignoreIllegals: true }).value : null;
}

export const languageAlias = (l: string) => EXT[l] ?? (LOADERS[l] ? l : null);
