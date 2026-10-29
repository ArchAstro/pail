import { createContext, useContext, useEffect } from "react";
import type { RepoOverview } from "../shared/types";
import { resolveRefpath, type Resolved } from "./lib/routes";

export interface RepoCtx {
  repo: string;
  overview: RepoOverview;
  actions: boolean;
  resolve: (refpath: string) => Resolved;
  /** The ref the page is showing, for the file finder. */
  setCurrentRef: (ref: string) => void;
  currentRef: string;
}

export const RepoContext = createContext<RepoCtx | null>(null);

export function useRepo(): RepoCtx {
  const c = useContext(RepoContext);
  if (!c) throw new Error("useRepo outside a repository route");
  return c;
}

export function makeResolver(o: RepoOverview) {
  return (refpath: string) => resolveRefpath(refpath, o.branches, o.default);
}

export function useCurrentRef(ref: string) {
  const { setCurrentRef } = useRepo();
  useEffect(() => setCurrentRef(ref), [ref, setCurrentRef]);
}
