import { languageAlias } from "./highlight";
import { rawUrl, blobPath, treePath } from "./routes";

export interface MdContext { repo: string; ref: string; dir: string }

function resolvePath(dir: string, rel: string): string {
  const parts = (rel.startsWith("/") ? [] : dir.split("/").filter(Boolean));
  for (const seg of rel.split("/")) {
    if (seg === "..") parts.pop();
    else if (seg && seg !== ".") parts.push(seg);
  }
  return parts.join("/");
}

const isRelative = (u: string) => u !== "" && !/^([a-z][a-z0-9+.-]*:|#|\/\/)/i.test(u);

/** Markdown to sanitized HTML. Relative links and images point back into the repository. */
export async function renderMarkdown(text: string, ctx: MdContext): Promise<string> {
  const [{ marked }, { default: DOMPurify }] = await Promise.all([import("marked"), import("dompurify")]);
  const raw = marked.parse(text, { gfm: true, async: false }) as string;
  const clean = DOMPurify.sanitize(raw, { USE_PROFILES: { html: true }, FORBID_TAGS: ["style", "form", "input"], FORBID_ATTR: ["style"] });
  const tpl = document.createElement("template");
  tpl.innerHTML = clean;
  tpl.content.querySelectorAll("a[href]").forEach((a) => {
    const href = a.getAttribute("href")!;
    if (isRelative(href)) {
      const [p, hash] = href.split("#");
      const target = resolvePath(ctx.dir, decodeURIComponent(p));
      a.setAttribute("href", (/\.[a-z0-9]+$/i.test(target) ? blobPath(ctx.repo, ctx.ref, target) : treePath(ctx.repo, ctx.ref, target)) + (hash ? "#" + hash : ""));
      a.setAttribute("data-internal", "");
    } else if (/^https?:/i.test(href)) {
      a.setAttribute("target", "_blank");
      a.setAttribute("rel", "noopener noreferrer");
    }
  });
  tpl.content.querySelectorAll("img[src]").forEach((img) => {
    const src = img.getAttribute("src")!;
    if (isRelative(src)) img.setAttribute("src", rawUrl(ctx.repo, ctx.ref, resolvePath(ctx.dir, decodeURIComponent(src))));
    img.setAttribute("loading", "lazy");
  });
  tpl.content.querySelectorAll("h1,h2,h3,h4").forEach((h) => {
    h.id ||= (h.textContent ?? "").toLowerCase().trim().replace(/[^\w\s-]/g, "").replace(/\s+/g, "-");
  });
  return tpl.innerHTML;
}

export function codeLanguage(el: Element): string | null {
  const cls = [...el.classList].find((c) => c.startsWith("language-"));
  return cls ? languageAlias(cls.slice(9)) : null;
}
