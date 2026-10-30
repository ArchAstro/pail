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
  /** `merging`: claimed by a merge in progress; edit, close, ready and reopen refuse it. */
  state: "open" | "merging" | "merged" | "closed";
  /** Not mergeable until marked ready. */
  draft?: boolean;
  created?: number;
  updated?: number;
  closed_at?: number;
  closed_by?: string;
  /** Who is merging (slug of their name) and the head commit they judged. */
  merging?: { by: string; commit: string };
  merged_commit?: string;
  merged_by?: string;
  merged_at?: number;
}

/**
 * `who` is the name to show; `byAuthor` is true when the PR's author wrote it (it does not count).
 * `commit` is the head it was given on; `stale` is true once the head has moved on (it then
 * counts for nothing).
 */
export interface Review { who: string; verdict: "approve" | "changes"; comment: string; at?: number; byAuthor: boolean; commit?: string | null; stale: boolean }

export type Side = "new" | "old";

/**
 * `_meta:prs/<n>/comments/<ms>-<who>.json`. A line comment has `path`, `line` (1-based), `side`
 * and `commit` (the PR head the line was read at); a reply has `in_reply_to` (a comment's `id`)
 * and its parent's anchor. `thread` is the `id` of the comment that opened the thread;
 * `outdated` is derived when served.
 */
export interface PrComment {
  id: string; who: string; body: string; at?: number;
  in_reply_to?: string; path?: string; line?: number; side?: Side; commit?: string;
  thread: string; outdated: boolean;
}

/** What the comment route takes: a plain comment, a line comment (`path` and `line`), or a reply. */
export interface NewComment { body: string; path?: string; line?: number; side?: Side; commit?: string; in_reply_to?: string }

export interface PrSummary extends PrRecord { headCommit: string | null; checks?: CheckRollup; approvals: number; changesRequested: number; comments: number }

export interface Policy { approvals: number; required_checks: string[] }

export interface PrDetail {
  pr: PrRecord;
  headCommit: string | null;
  baseCommit: string | null;
  reviews: Review[];
  comments: PrComment[];
  commits: CommitSummary[];
  checks: CheckResult[];
  policy: Policy;
  readiness: { ready: boolean; problems: string[] };
  actions: boolean;
}

/** base...head: what a pull request from head into base would contain. */
export interface Compare {
  base: string;
  head: string;
  baseCommit: string;
  headCommit: string;
  commits: CommitSummary[];
  patch: string;
  truncated: boolean;
  /** An open pull request for the same head and base. */
  existing: number | null;
  /** The first commit's subject and body: the defaults for a new pull request. */
  title: string;
  body: string;
}

export interface NewPr { title: string; body?: string; head: string; base: string; draft?: boolean }

export interface PrDiff { patch: string; from: string | null; to: string | null; truncated: boolean }

export interface AppConfig { actions: boolean; repos: RepoSummary[]; version: string }

export interface RefEvent { repo: string; branch: string; commit: string | null; old: string | null; at?: string }
