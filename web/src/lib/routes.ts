// URLs carry the ref and path in one segment list: /r/tree/<ref>/<path>. Branch names contain
// slashes, so the ref is the longest branch name that prefixes it (or a commit id).

export interface Resolved { ref: string; path: string }

export function resolveRefpath(refpath: string, branches: { name: string }[], fallback: string): Resolved {
  const rp = refpath.replace(/^\/+/, "");
  if (!rp) return { ref: fallback, path: "" };
  const hit = branches
    .filter((b) => rp === b.name || rp.startsWith(`${b.name}/`))
    .sort((a, b) => b.name.length - a.name.length)[0];
  if (hit) return { ref: hit.name, path: rp.slice(hit.name.length).replace(/^\//, "") };
  const [id, ...rest] = rp.split("/");
  if (/^[0-9a-f]{7,64}$/i.test(id)) return { ref: id, path: rest.join("/") };
  return { ref: fallback, path: rp };
}

const enc = (p: string) => p.split("/").map(encodeURIComponent).join("/");

export const repoPath = (repo: string) => `/${encodeURIComponent(repo)}`;
export const treePath = (repo: string, ref: string, path = "") => `${repoPath(repo)}/tree/${enc(ref)}${path ? "/" + enc(path) : ""}`;
export const blobPath = (repo: string, ref: string, path: string) => `${repoPath(repo)}/blob/${enc(ref)}/${enc(path)}`;
export const commitsPath = (repo: string, ref?: string, path?: string) =>
  `${repoPath(repo)}/commits${ref ? "/" + enc(ref) : ""}${path ? "?path=" + encodeURIComponent(path) : ""}`;
export const commitPath = (repo: string, id: string) => `${repoPath(repo)}/commit/${id}`;
export const branchesPath = (repo: string) => `${repoPath(repo)}/branches`;
export const pullsPath = (repo: string) => `${repoPath(repo)}/pulls`;
export const pullPath = (repo: string, n: number) => `${repoPath(repo)}/pull/${n}`;
export const checksPath = (repo: string) => `${repoPath(repo)}/checks`;
export const rawUrl = (repo: string, ref: string, path: string, download = false) =>
  `/api/repos/${encodeURIComponent(repo)}/raw?ref=${encodeURIComponent(ref)}&path=${encodeURIComponent(path)}${download ? "&download=1" : ""}`;
export const comparePath = (repo: string, base?: string, head?: string) => {
  const p = new URLSearchParams();
  if (base) p.set("base", base);
  if (head) p.set("head", head);
  const qs = p.toString();
  return `${repoPath(repo)}/compare${qs ? "?" + qs : ""}`;
};
