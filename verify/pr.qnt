// -*- mode: Bluespec; -*-
/// The `pail pr` protocol (`extensions/pail-pr`; pail-web's `server/repo.ts` follows it): pull
/// requests, reviews and comments as records on the `_meta` branch, written by clients that
/// each read, decide and then write, while others do the same. `tests/pr_refinement.rs` runs
/// the real plugin against `pr_scenarios.qnt`; `pr_sim.qnt` simulates random interleavings.
///
/// A client is a process of the plugin. `begin` is everything it does before its first
/// write (its reads, then its decision): it ends held at that write (`Wait`), or has
/// refused. `release` lets the write go through: it succeeds, or fails with a conflict
/// (exit 4), and the client reads again and decides again, or finishes. The writes:
///
///   WCreate   `put prs/<n>.json --if-absent`                  (claim an id; n+1 on a conflict)
///   WSave     `put prs/1.json --expect prs/1.json=<blob read>` (edit, close, reopen, ready,
///             the merge's claim, release and record: a compare-and-swap on the record)
///   WReview   `put prs/1/reviews/<who>.json`                   (the reviewer's latest verdict)
///   WComment  `put prs/1/comments/<ms>-<who>.json --if-absent` (ms+1 on a conflict)
///   WLand     `land <head> --stack --onto <base> --expect-tip <commit>`
///
/// One head branch, one base. The head's commits are numbered 1..tip, the base is commit 0;
/// `fileAt` says what each commit's files hold. Merging claims the PR (`Merging`), lands
/// exactly the commit it judged, then records `Merged`. Everything not about concurrency
/// (titles, ids beyond the first PR, force pushes, policy files, `--force`) is left out.
module pr {
  // ---------- the world ----------

  pure val CLIENTS: Set[str] = Set("alice", "alice2", "bob", "carol")

  /// Two clients can be one person (`alice2` is `alice` on another machine).
  pure def whoOf(c: str): str = if (c == "alice2") "alice" else c

  pure val PATHS: Set[str] = Set("a", "b")
  pure val MAX_TIP = 3
  /// Stands for no `--commit`: the head when the command ran.
  pure val HEAD = 99

  // The protocol's design choices. Each `true` is the fix; `false` is what the plugin did
  // before, and what `npm run mutate:pr` shows `safety` (pr_sim.qnt) catching.
  /// Writes to a PR record are compare-and-swap on the record read (else blind overwrite).
  pure val CAS = true
  /// Merging claims the PR before landing (else it judges, lands, then records).
  pure val CLAIM = true
  /// The land is of the commit judged (else of whatever the head is by then).
  pure val GUARD = true
  /// An approval counts for the commit it was given on (else for any).
  pure val PER_COMMIT = true

  /// Commit `k`'s file `p`, line by line (commit 0: the base). `a` changes on every commit;
  /// `b` from the second.
  pure def fileAt(k: int, p: str): List[int] =
    if (p == "a") [k, 10, 11]
    else if (k <= 1) [20, 21, 22]
    else [20, 21, 22, k]

  pure def changed(k: int, p: str): bool = fileAt(k, p) != fileAt(0, p)

  /// The text of line `line` (1-based) of `p` at commit `k`; 99 if it has no such line.
  pure def lineAt(k: int, p: str, line: int): int =
    if (line >= 1 and line <= fileAt(k, p).length()) fileAt(k, p)[line - 1] else 99

  // ---------- the records ----------

  type Side = New | Old

  type AnchorOpt = NoAnchor | Anchored({ path: str, line: int, side: Side, commit: int })
  type CommentId = { ms: int, who: str }
  type ReplyOpt = NoReply | ReplyTo(CommentId)
  /// `body` stands for the text.
  type Comment = { body: int, anchor: AnchorOpt, reply: ReplyOpt }
  type Review = { approve: bool, commit: int }

  type PrState =
    | Open
    | Closed
    | Merging({ by: str, commit: int })
    | Merged({ commit: int })

  /// `title` and `body` stand for text, so that two edits can be told apart.
  type PrRec = { author: str, title: int, body: int, state: PrState, draft: bool }

  // ---------- the clients ----------

  type Spec =
    | Top
    | Line({ path: str, line: int, side: Side, commit: int })
    | Reply(CommentId)

  type Cmd =
    | Create(int)
    | EditTitle(int)
    | EditBody(int)
    | EditDraft(bool)
    | Ready(bool)
    | Close
    | Reopen
    | CmReview({ approve: bool, commit: int })
    | CmComment({ ms: int, body: int, spec: Spec })
    | Merge
    // The rest of a merge, after its claim: give the claim up; record the merge.
    | Release
    | Finish(int)

  /// The write a client is held at.
  type Write =
    | WCreate({ n: int, rec: PrRec })
    | WSave({ expect: PrRec, after: PrRec, cmd: Cmd })
    | WReview(Review)
    | WComment({ id: CommentId, c: Comment })
    | WLand({ commit: int })

  type Phase = Idle | Wait(Write)
  /// `NoRes`: still running (or never run). `Acked`: exited 0. `Refused`: exited non-zero.
  type Res = NoRes | Acked | Refused
  type Client = { phase: Phase, res: Res }

  type State = {
    // `prs/<n>.json`
    prs: int -> PrRec,
    // `prs/1/reviews/<who>.json`
    reviews: str -> Review,
    // `prs/1/comments/<ms>-<who>.json`
    comments: CommentId -> Comment,
    // The commits with a passing required check (`checks/<commit>/test.json`).
    checks: Set[int],
    // The head branch is at commit `tip` (1..tip are the PR's).
    tip: int,
    // The base has commits 1..landed (0: none).
    landed: int,
    clients: str -> Client,
    // What the last `view` showed: the outdated comments, and whether it said mergeable.
    viewed: Set[CommentId],
    viewReady: bool,
  }

  pure val INITIAL: State = {
    prs: Map(),
    reviews: Map(),
    comments: Map(),
    checks: Set(),
    tip: 1,
    landed: 0,
    clients: CLIENTS.mapBy(c => { phase: Idle, res: NoRes }),
    viewed: Set(),
    viewReady: false,
  }

  // ---------- what the plugin decides ----------

  pure val NOREC: PrRec = { author: "", title: 0, body: 0, state: Open, draft: false }

  /// PR 1's record (a placeholder before it exists: the expressions the refinement checks
  /// evaluate every `val`, so `get` must not be asked for a missing key).
  pure def pr1(s: State): PrRec = if (s.prs.keys().contains(1)) s.prs.get(1) else NOREC

  pure def isOpen(r: PrRec): bool = match r.state { | Open => true | _ => false }
  pure def isClosed(r: PrRec): bool = match r.state { | Closed => true | _ => false }
  pure def isMerged(r: PrRec): bool = match r.state { | Merged(_) => true | _ => false }
  pure def isMerging(r: PrRec): bool = match r.state { | Merging(_) => true | _ => false }

  /// The commit a record names as merging or merged (0: none).
  pure def claimed(r: PrRec): int = match r.state {
    | Merging(m) => m.commit
    | Merged(m) => m.commit
    | _ => 0
  }

  /// The non-author reviewers whose approval counts for `tip`.
  pure def approvers(s: State, r: PrRec): Set[str] =
    s.reviews.keys().filter(w =>
      s.reviews.get(w).approve and w != r.author and (not(PER_COMMIT) or s.reviews.get(w).commit == s.tip))

  /// `readiness` finds no problem: open, not a draft, the head's check passing, approved.
  pure def mergeable(s: State, r: PrRec): bool =
    isOpen(r) and not(r.draft) and s.tip >= 1 and s.checks.contains(s.tip) and approvers(s, r).size() >= 1

  /// A commit is one of the PR's: on the head, not yet on the base.
  pure def inRange(s: State, k: int): bool = k > s.landed and k <= s.tip

  /// Whether a line anchor is on a line of the diff: the commit is the PR's, the path changed
  /// by it, the line one of that side's file (new: at the commit; old: at the base).
  pure def anchorOk(s: State, a: { path: str, line: int, side: Side, commit: int }): bool =
    inRange(s, a.commit) and PATHS.contains(a.path) and changed(a.commit, a.path) and a.line >= 1 and
    a.line <= (if (a.side == New) fileAt(a.commit, a.path).length() else fileAt(0, a.path).length())

  /// A line comment is outdated iff the head has moved since its commit and its line, on the
  /// new side, is not what it was there (or its commit left the PR). Not once merging.
  pure def outdated(s: State, cm: Comment): bool =
    not(isMerged(pr1(s))) and not(isMerging(pr1(s))) and match cm.anchor {
      | Anchored(a) =>
        a.side == New and s.tip != a.commit and
        (not(inRange(s, a.commit)) or lineAt(s.tip, a.path, a.line) != lineAt(a.commit, a.path, a.line))
      | NoAnchor => false
    }

  type Dec = Refuse | Skip | Put(PrRec)

  /// A claim is given up (for the merge to try again), or recorded as the merge.
  pure def releaseDec(c: str, r: PrRec): Dec = match r.state {
    | Merging(m) => if (m.by == whoOf(c)) Put(r.with("state", Open)) else Skip
    | _ => Skip
  }

  pure def finishDec(c: str, r: PrRec, h: int): Dec = match r.state {
    | Merged(_) => Skip
    | Merging(m) => if (m.by == whoOf(c)) Put(r.with("state", Merged({ commit: h }))) else Refuse
    | _ => if (CLAIM) Refuse else Put(r.with("state", Merged({ commit: h })))
  }

  /// What a command does to PR 1's record `r`, as it stands when the command reads it.
  pure def decide(s: State, c: str, cmd: Cmd, r: PrRec): Dec = match cmd {
    | EditTitle(t) => if (isMerging(r) or isMerged(r)) Refuse else Put(r.with("title", t))
    | EditBody(b) => if (isMerging(r) or isMerged(r)) Refuse else Put(r.with("body", b))
    | EditDraft(d) => if (isMerging(r) or isMerged(r)) Refuse else Put(r.with("draft", d))
    | Ready(undo) => if (isOpen(r)) Put(r.with("draft", undo)) else Refuse
    | Close => if (isOpen(r)) Put(r.with("state", Closed)) else Refuse
    | Reopen => if (isClosed(r)) Put(r.with("state", Open)) else Refuse
    | Release => releaseDec(c, r)
    | Finish(h) => finishDec(c, r, h)
    | _ => Refuse
  }

  pure def refused: Client = { phase: Idle, res: Refused }
  pure def acked: Client = { phase: Idle, res: Acked }
  pure def waiting(w: Write): Client = { phase: Wait(w), res: NoRes }

  /// The outcome of deciding: held at the CAS write, or done (`skip`: what a no-op means).
  pure def saving(r: PrRec, cmd: Cmd, d: Dec, skip: Client): Client = match d {
    | Refuse => refused
    | Skip => skip
    | Put(nx) => waiting(WSave({ expect: r, after: nx, cmd: cmd }))
  }

  /// A merge reads and judges: held at its claim, or at the land when it makes none.
  pure def mergeStart(s: State, c: str): Client = {
    val r = pr1(s)
    if (not(s.prs.keys().contains(1)) or not(mergeable(s, r))) refused
    else if (CLAIM)
      waiting(WSave({ expect: r, after: r.with("state", Merging({ by: whoOf(c), commit: s.tip })), cmd: Merge }))
    else waiting(WLand({ commit: s.tip }))
  }

  /// The land is over: the commit it landed is on the base; record the merge.
  pure def afterLand(s: State, c: str, h: int): Client = {
    val r = pr1(s)
    saving(r, Finish(h), decide(s, c, Finish(h), r), acked)
  }

  /// The land refused (the head moved): give the claim up.
  pure def afterRefusedLand(s: State, c: str): Client = {
    val r = pr1(s)
    if (CLAIM) saving(r, Release, decide(s, c, Release, r), refused) else refused
  }

  pure def headOr(s: State, k: int): int = if (k == HEAD) s.tip else k

  /// What `cmd` does up to its first write, as client `c`.
  pure def start(s: State, c: str, cmd: Cmd): Client = {
    val has = s.prs.keys().contains(1)
    val r = pr1(s)
    match cmd {
      | Create(t) =>
        // `create` refuses a second open PR for the branch, else claims max id + 1.
        if (s.prs.keys().exists(n => isOpen(s.prs.get(n)))) refused
        else waiting(WCreate({
          n: s.prs.keys().fold(0, (m, k) => if (k > m) k else m) + 1,
          rec: { author: whoOf(c), title: t, body: 0, state: Open, draft: false },
        }))
      | CmReview(v) =>
        // The commit must be the PR's (the default, the head, always is).
        if (not(has) or (v.commit != HEAD and not(inRange(s, v.commit)))) refused
        else waiting(WReview({ approve: v.approve, commit: headOr(s, v.commit) }))
      | CmComment(m) =>
        if (not(has)) refused
        else match m.spec {
          | Top => waiting(WComment({
            id: { ms: m.ms, who: whoOf(c) },
            c: { body: m.body, anchor: NoAnchor, reply: NoReply },
          }))
          | Line(l) =>
            val a = { path: l.path, line: l.line, side: l.side, commit: headOr(s, l.commit) }
            if (anchorOk(s, a)) waiting(WComment({
              id: { ms: m.ms, who: whoOf(c) },
              c: { body: m.body, anchor: Anchored(a), reply: NoReply },
            }))
            else refused
          | Reply(p) =>
            // The parent is on this PR; the reply is on its line.
            if (s.comments.keys().contains(p)) waiting(WComment({
              id: { ms: m.ms, who: whoOf(c) },
              c: { body: m.body, anchor: s.comments.get(p).anchor, reply: ReplyTo(p) },
            }))
            else refused
        }
      | Merge => if (has) mergeStart(s, c) else refused
      | Release => refused
      | Finish(_) => refused
      | _ => if (has) saving(r, cmd, decide(s, c, cmd, r), refused) else refused
    }
  }

  /// A client's write is not as it read: it reads again and decides again.
  pure def conflicted(s: State, c: str, w: Write): Client = match w {
    | WCreate(x) => waiting(WCreate({ n: x.n + 1, rec: x.rec }))
    | WComment(x) => waiting(WComment({ id: { ms: x.id.ms + 1, who: x.id.who }, c: x.c }))
    | WSave(x) =>
      if (x.cmd == Merge) mergeStart(s, c)
      else saving(pr1(s), x.cmd, decide(s, c, x.cmd, pr1(s)),
        if (x.cmd == Release) refused else acked)
    | _ => refused
  }

  pure def setClient(s: State, c: str, cl: Client): State = s.with("clients", s.clients.set(c, cl))

  // ---------- steps ----------

  pure def beginCan(s: State, c: str): bool = s.clients.get(c).phase == Idle

  pure def beginNext(s: State, c: str, cmd: Cmd): State = setClient(s, c, start(s, c, cmd))

  pure def releaseCan(s: State, c: str): bool = s.clients.get(c).phase != Idle

  /// The write goes through, or finds a conflict. A compare-and-swap is on the record's
  /// content, as `put --expect` is on its blob.
  pure def releaseNext(s: State, c: str): State = match s.clients.get(c).phase {
    | Idle => s
    | Wait(w) => match w {
      | WCreate(x) =>
        if (s.prs.keys().contains(x.n)) setClient(s, c, conflicted(s, c, w))
        else setClient(s.with("prs", s.prs.put(x.n, x.rec)), c, acked)
      | WSave(x) =>
        if (not(CAS) or pr1(s) == x.expect) {
          val s2 = s.with("prs", s.prs.set(1, x.after))
          setClient(s2, c, match x.cmd {
            | Merge => waiting(WLand({ commit: claimed(x.after) }))
            | Release => refused
            | _ => acked
          })
        } else setClient(s, c, conflicted(s, c, w))
      | WReview(v) => setClient(s.with("reviews", s.reviews.put(whoOf(c), v)), c, acked)
      | WComment(x) =>
        if (s.comments.keys().contains(x.id)) setClient(s, c, conflicted(s, c, w))
        else setClient(s.with("comments", s.comments.put(x.id, x.c)), c, acked)
      | WLand(x) =>
        if (not(GUARD) or s.tip == x.commit) {
          val h = if (GUARD) x.commit else s.tip
          val s2 = s.with("landed", h)
          setClient(s2, c, afterLand(s2, c, h))
        } else setClient(s, c, afterRefusedLand(s, c))
    }
  }

  pure def pushCan(s: State): bool = s.tip < MAX_TIP and s.landed == 0
  pure def pushNext(s: State): State = s.with("tip", s.tip + 1)

  pure def ciCan(s: State, k: int): bool = k >= 1 and k <= s.tip
  pure def ciNext(s: State, k: int): State = s.with("checks", s.checks.union(Set(k)))

  /// `pail pr view --json`: which comments it marks outdated, and whether it says mergeable.
  pure def viewCan(s: State): bool = s.prs.keys().contains(1)
  pure def viewNext(s: State): State =
    s.with("viewed", s.comments.keys().filter(id => outdated(s, s.comments.get(id))))
     .with("viewReady", mergeable(s, pr1(s)))

  // ---------- actions ----------

  var state: State

  action init = state' = INITIAL

  action begin(c: str, cmd: Cmd): bool = all {
    beginCan(state, c),
    state' = beginNext(state, c, cmd),
  }

  action release(c: str): bool = all {
    releaseCan(state, c),
    state' = releaseNext(state, c),
  }

  action push: bool = all { pushCan(state), state' = pushNext(state) }

  action ci(k: int): bool = all { ciCan(state, k), state' = ciNext(state, k) }

  action view: bool = all { viewCan(state), state' = viewNext(state) }

  // ---------- the commands a random run draws from ----------

  pure val IDS: Set[CommentId] = Set(
    { ms: 1, who: "alice" }, { ms: 2, who: "alice" }, { ms: 1, who: "bob" }, { ms: 1, who: "carol" })

  pure val SPECS: Set[Spec] = Set(
    Top,
    Line({ path: "a", line: 1, side: New, commit: HEAD }),
    Line({ path: "a", line: 2, side: New, commit: 1 }),
    Line({ path: "a", line: 1, side: Old, commit: HEAD }),
    Line({ path: "b", line: 3, side: New, commit: HEAD }),
    Line({ path: "b", line: 1, side: New, commit: 0 }),
    Line({ path: "a", line: 4, side: New, commit: HEAD }),
    Line({ path: "b", line: 4, side: New, commit: HEAD }),
    Line({ path: "b", line: 4, side: Old, commit: HEAD }),
  ).union(IDS.map(id => Reply(id)))

  pure val CMDS: Set[Cmd] = Set(
    Create(1), Create(2),
    EditTitle(1), EditTitle(2), EditBody(1), EditBody(2),
    EditDraft(true), EditDraft(false), Ready(true), Ready(false),
    Close, Reopen, Merge,
    CmReview({ approve: true, commit: HEAD }), CmReview({ approve: false, commit: HEAD }),
    CmReview({ approve: true, commit: 1 }),
  ).union(SPECS.map(sp => CmComment({ ms: 1, body: 1, spec: sp })))
   .union(SPECS.map(sp => CmComment({ ms: 2, body: 2, spec: sp })))

  /// Any interleaving. Releases are likelier than beginnings, so that clients get to finish.
  action step = {
    nondet c = CLIENTS.oneOf()
    nondet cmd = CMDS.oneOf()
    nondet k = 1.to(MAX_TIP).oneOf()
    nondet n = 1.to(8).oneOf()
    any {
      all { n <= 2, begin(c, cmd) },
      all { n >= 3 and n <= 6, release(c) },
      all { n == 7, push },
      all { n == 8, any { ci(k), view } },
    }
  }

  // ---------- properties ----------

  /// Commits are on the base only for a PR that claimed them, and a PR that claimed a commit
  /// is merged when the base has it: a merge never lands what it did not judge, a closed or
  /// draft PR never merges, and a merged record never goes back.
  val landedMatchesRecord: bool =
    if (state.landed == 0) not(state.prs.keys().exists(n => isMerged(state.prs.get(n))))
    else state.prs.keys().contains(1) and claimed(state.prs.get(1)) == state.landed

  /// A client that goes on to land or record the claim, or give it up.
  pure def finishesClaim(cl: Client): bool = match cl.phase {
    | Wait(w) => writeFinishes(w)
    | Idle => false
  }

  pure def writeFinishes(w: Write): bool = match w {
    | WLand(_) => true
    | WSave(x) => cmdFinishes(x.cmd)
    | _ => false
  }

  pure def cmdFinishes(cmd: Cmd): bool = match cmd {
    | Release => true
    | Finish(_) => true
    | _ => false
  }

  /// A claim is held by a client that is still merging.
  val claimHasAHolder: bool =
    state.prs.keys().forall(n => match state.prs.get(n).state {
      | Merging(m) => CLIENTS.exists(c => whoOf(c) == m.by and finishesClaim(state.clients.get(c)))
      | _ => true
    })

  /// What was merged or being merged was ready: a passing check, not a draft.
  val claimedWasReady: bool =
    state.prs.keys().forall(n => {
      val r = state.prs.get(n)
      claimed(r) == 0 or (state.checks.contains(claimed(r)) and not(r.draft) and claimed(r) <= state.tip)
    })

  /// Every line comment is on a line of its commit's diff, and every reply is in a thread
  /// whose parent exists and which it shares the line of. Comments exist only on PR 1.
  val commentsAreAnchored: bool =
    state.comments.keys().forall(id => {
      val cm = state.comments.get(id)
      val anchorOk2 = match cm.anchor {
        | NoAnchor => true
        | Anchored(a) => a.commit >= 1 and a.commit <= state.tip and PATHS.contains(a.path) and
          changed(a.commit, a.path) and a.line >= 1 and
          a.line <= (if (a.side == New) fileAt(a.commit, a.path).length() else fileAt(0, a.path).length())
      }
      val replyOk = match cm.reply {
        | NoReply => true
        | ReplyTo(p) => state.comments.keys().contains(p) and state.comments.get(p).anchor == cm.anchor
      }
      anchorOk2 and replyOk
    })

  /// Reviews are of the PR's commits.
  val reviewsAreOfCommits: bool =
    state.reviews.keys().forall(w => state.reviews.get(w).commit >= 1 and state.reviews.get(w).commit <= state.tip)

  val safety: bool = all {
    landedMatchesRecord,
    claimHasAHolder,
    claimedWasReady,
    commentsAreAnchored,
    reviewsAreOfCommits,
  }
}
