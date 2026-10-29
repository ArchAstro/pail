// Shapes shared by the server's JSON API and the React client.

export type CheckState = "pending" | "success" | "failure";

export interface Person { name: string; email: string }

export interface BranchInfo {
  name: string;
  commit: string;
  isDefault: boolean;
  date?: string;
  subject?: string;
  author?: string;
  /** Rolled-up state of the checks on the tip: none when it has no results. */
  checks?: CheckRollup;
}

export interface CheckRollup { state: CheckState | "none"; passed: number; failed: number; pending: number; total: number }

export interface RepoSummary { name: string; description?: string; store: string }

export interface RepoOverview extends RepoSummary {
  default: string;
  branches: BranchInfo[];
  head: string;
  error?: string;
}

export interface CommitSummary {
  id: string;
  parent: string | null;
  author: Person;
  date: string;
  subject: string;
  body: string;
  maintenance: boolean;
  /** Commits this one rewrites (squashed, rebased), from meta `rewritten.from`. */
  rewrittenFrom: string[];
  /** Branch this commit was landed from, from meta `landed.from`. */
  landedFrom?: string;
  files: number;
  checks?: CheckRollup;
  refs?: string[];
}

export interface LogPage { commits: CommitSummary[]; more: boolean }

export interface CommitDetail { commit: CommitSummary; patch: string; truncated?: boolean; checks: CheckResult[]; sources: { id: string; subject: string }[] }

export interface TreeEntry { name: string; type: "blob" | "tree"; size?: number; mode: string }
export interface TreeListing { commit: string; path: string; entries: TreeEntry[] }

export interface BlobView {
  commit: string;
  path: string;
  size: number;
  binary: boolean;
  image: boolean;
  tooLarge: boolean;
  text?: string;
}

export interface CheckResult {
  name: string;
  state: CheckState;
  required: boolean;
  declared: boolean;
  command?: string;
  log_tail?: string;
  recorded?: boolean;
  runner?: string;
  branch?: string;
  duration_s?: number;
  at?: number;
  [k: string]: unknown;
}

export interface PrRecord {
  id: number;
  title: string;
  body: string;
  head: string;
  base: string;
  author: string;
  state: "open" | "merged" | "closed";
  created?: number;
  merged_commit?: string;
  merged_by?: string;
  merged_at?: number;
}

export interface Review { who: string; verdict: "approve" | "changes"; comment: string; at?: number }

export interface PrSummary extends PrRecord { headCommit: string | null; checks?: CheckRollup; approvals: number; changesRequested: number }

export interface Policy { approvals: number; required_checks: string[] }

export interface PrDetail {
  pr: PrRecord;
  headCommit: string | null;
  baseCommit: string | null;
  reviews: Review[];
  commits: CommitSummary[];
  checks: CheckResult[];
  policy: Policy;
  readiness: { ready: boolean; problems: string[] };
  actions: boolean;
}

export interface PrDiff { patch: string; from: string | null; to: string | null; truncated: boolean }

export interface AppConfig { actions: boolean; repos: RepoSummary[]; version: string }

export interface RefEvent { repo: string; branch: string; commit: string | null; old: string | null; at?: string }
