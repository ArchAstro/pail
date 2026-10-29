import { Suspense, useCallback, useEffect, useMemo, useState } from "react";
import { NavLink, Outlet, useLocation, useNavigate, useParams, Link } from "react-router-dom";
import type { AppConfig, PrSummary, RepoOverview } from "../../shared/types";
import { LiveProvider, repoApi, useGet, useLive } from "../api";
import { branchesPath, checksPath, commitsPath, pullsPath, repoPath } from "../lib/routes";
import { makeResolver, RepoContext } from "../repoContext";
import { ErrorNote, Loading } from "./Bits";
import { Icon } from "./Icon";
import { Palette } from "./Palette";
import { TopBar } from "./Shell";

export function RepoLayout() {
  const { repo = "" } = useParams();
  return (
    <LiveProvider repo={repo}>
      <Inner repo={repo} />
    </LiveProvider>
  );
}

function Inner({ repo }: { repo: string }) {
  const overview = useGet<RepoOverview>(repoApi(repo));
  const cfg = useGet<AppConfig>("/api/config", { live: false });
  const prs = useGet<PrSummary[]>(repoApi(repo, "/prs"));
  const { connected, last } = useLive();
  const nav = useNavigate();
  const { pathname } = useLocation();
  const [currentRef, setCurrentRef] = useState("");
  const [palette, setPalette] = useState<null | "files" | "all">(null);
  const [help, setHelp] = useState(false);
  const [toast, setToast] = useState<string | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement;
      if (e.metaKey || e.ctrlKey || e.altKey || el.closest("input, textarea, select, [contenteditable]")) return;
      if (e.key === "t") { e.preventDefault(); setPalette("files"); }
      else if (e.key === "/") { e.preventDefault(); setPalette("all"); }
      else if (e.key === "?") setHelp((h) => !h);
      else if (e.key === "Escape") setHelp(false);
      else if (e.key === "g") {
        // g then c/b/p/k/h: go to commits, branches, pull requests, checks, home
        const once = (ev: KeyboardEvent) => {
          window.removeEventListener("keydown", once, true);
          const dest: Record<string, string> = { c: commitsPath(repo), b: branchesPath(repo), p: pullsPath(repo), k: checksPath(repo), h: repoPath(repo) };
          if (dest[ev.key]) { ev.preventDefault(); ev.stopPropagation(); nav(dest[ev.key]); }
        };
        window.addEventListener("keydown", once, true);
        setTimeout(() => window.removeEventListener("keydown", once, true), 1200);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [repo, nav]);

  useEffect(() => {
    if (!last) return;
    setToast(last.commit ? `${last.branch} moved to ${last.commit.slice(0, 8)}` : `${last.branch} deleted`);
    const t = setTimeout(() => setToast(null), 4000);
    return () => clearTimeout(t);
  }, [last]);

  const set = useCallback((r: string) => setCurrentRef(r), []);
  const ctx = useMemo(
    () => overview.data && { repo, overview: overview.data, actions: cfg.data?.actions ?? false, resolve: makeResolver(overview.data), setCurrentRef: set, currentRef },
    [repo, overview.data, cfg.data, set, currentRef],
  );

  const open = prs.data?.filter((p) => p.state === "open").length;
  return (
    <>
      <TopBar crumb={<><Icon name="repo" size={15} /><Link to={repoPath(repo)}>{repo}</Link></>} onSearch={() => setPalette("all")} live={connected} />
      <nav className="tabs" aria-label="Repository">
        <NavLink end to={repoPath(repo)} className={({ isActive }) => (isActive || /\/(tree|blob)\//.test(pathname) ? "is-on" : "")}><Icon name="file" size={15} />Code</NavLink>
        <NavLink to={commitsPath(repo)}><Icon name="commit" size={15} />Commits</NavLink>
        <NavLink to={branchesPath(repo)}><Icon name="branch" size={15} />Branches</NavLink>
        <NavLink to={pullsPath(repo)}><Icon name="pr" size={15} />Pull requests{open ? <span className="tabs__n">{open}</span> : null}</NavLink>
        <NavLink to={checksPath(repo)}><Icon name="check" size={15} />Checks</NavLink>
      </nav>
      <main id="main" className="page" tabIndex={-1}>
        {overview.error && !ctx ? <ErrorNote error={overview.error} retry={overview.reload} /> : !ctx ? <Loading /> : (
          <RepoContext.Provider value={ctx}>
            <Suspense fallback={<Loading />}><Outlet /></Suspense>
            {palette && <Palette mode={palette} onClose={() => setPalette(null)} />}
          </RepoContext.Provider>
        )}
      </main>
      {help && <Help onClose={() => setHelp(false)} />}
      <div className={`toast${toast ? " is-on" : ""}`} role="status" aria-live="polite">{toast}</div>
      <footer className="foot">
        <button className="linklike" onClick={() => setHelp(true)}>Keyboard shortcuts <kbd>?</kbd></button>
      </footer>
    </>
  );
}

function Help({ onClose }: { onClose: () => void }) {
  const rows: [string, string][] = [["t", "Find a file in this branch"], ["/", "Search files, branches, pull requests"], ["g c", "Go to commits"], ["g b", "Go to branches"], ["g p", "Go to pull requests"], ["g k", "Go to checks"], ["g h", "Go to the repository home"], ["?", "Show or hide this list"]];
  return (
    <div className="scrim" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="palette help" role="dialog" aria-modal="true" aria-label="Keyboard shortcuts">
        <h2>Keyboard shortcuts</h2>
        <dl>
          {rows.map(([k, d]) => (
            <div key={k}><dt>{k.split(" ").map((x) => <kbd key={x}>{x}</kbd>)}</dt><dd>{d}</dd></div>
          ))}
        </dl>
        <button className="btn" onClick={onClose}>Close</button>
      </div>
    </div>
  );
}

