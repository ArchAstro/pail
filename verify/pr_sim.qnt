// -*- mode: Bluespec; -*-
/// `pr.qnt`'s protocol under random interleavings, with a ghost remembering what was
/// acknowledged, for the properties that need history: a merged record never goes back, an
/// acknowledged edit is not lost, the commit landed is the one judged. (The refinement cannot
/// see ghosts, so they stay out of `pr.qnt`'s state.) `npm run simulate:pr` checks `safety`
/// here; `npm run mutate:pr` flips each design switch of `pr.qnt` and expects a violation.
module prSim {
  import pr.* from "./pr"

  type Ghost = {
    // The latest acknowledged value of each edited field, of PR 1.
    title: int,
    body: int,
    draft: bool,
    everMerged: bool,
    // Commits a merge judged ready (by the strict rule: an approval counts for its commit).
    judged: Set[int],  // 0 stands for none
  }

  var ghost: Ghost

  pure val GHOST0: Ghost = { title: 0, body: 0, draft: false, everMerged: false, judged: Set() }

  /// The strict rule, whatever `PER_COMMIT` says.
  pure def strictMergeable(s: State, r: PrRec): bool =
    isOpen(r) and not(r.draft) and s.tip >= 1 and s.checks.contains(s.tip) and
    s.reviews.keys().filter(w =>
      s.reviews.get(w).approve and w != r.author and s.reviews.get(w).commit == s.tip).size() >= 1

  /// The commit a client judged ready by holding at a claim (or, with no claim, at a land)
  /// that it did not hold at before (0: none).
  pure def judgedBy(s: State, c: str, t: State): int = match t.clients.get(c).phase {
    | Wait(w) => judgedAt(s, c, w)
    | Idle => 0
  }

  pure def judgedAt(s: State, c: str, w: Write): int = match w {
    | WSave(x) => if (x.cmd == Merge and strictMergeable(s, pr1(s))) s.tip else 0
    | WLand(_) => if (s.clients.get(c).phase == Idle and strictMergeable(s, pr1(s))) s.tip else 0
    | _ => 0
  }

  /// What a write that went through changed.
  pure def writeEffect(g: Ghost, s: State, c: str, t: State): Ghost = match s.clients.get(c).phase {
    | Wait(w) => writeEffectOf(g, s, w, t)
    | Idle => g
  }

  pure def writeEffectOf(g: Ghost, s: State, w: Write, t: State): Ghost = match w {
    | WCreate(x) =>
      if (x.n == 1 and not(s.prs.keys().contains(1)))
        g.with("title", x.rec.title).with("body", x.rec.body).with("draft", x.rec.draft)
      else g
    | WSave(x) => if (t.prs != s.prs and pr1(t) == x.after) wrote(g, x.cmd, x.after) else g
    | _ => g
  }

  /// What `begin` or `release` of client `c` did to the ghost.
  pure def ghostAfter(g: Ghost, s: State, c: str, t: State): Ghost =
    writeEffect(g.with("judged", g.judged.union(Set(judgedBy(s, c, t)))), s, c, t)

  pure def wrote(g: Ghost, cmd: Cmd, r: PrRec): Ghost = match cmd {
    | EditTitle(t) => g.with("title", t)
    | EditBody(b) => g.with("body", b)
    | EditDraft(d) => g.with("draft", d)
    | Ready(u) => g.with("draft", u)
    | Finish(_) => g.with("everMerged", true)
    | _ => g
  }

  action simInit = all { state' = INITIAL, ghost' = GHOST0 }

  action simBegin(c: str, cmd: Cmd): bool = all {
    beginCan(state, c),
    state' = beginNext(state, c, cmd),
    ghost' = ghostAfter(ghost, state, c, beginNext(state, c, cmd)),
  }

  action simRelease(c: str): bool = all {
    releaseCan(state, c),
    state' = releaseNext(state, c),
    ghost' = ghostAfter(ghost, state, c, releaseNext(state, c)),
  }

  action simPush: bool = all { pushCan(state), state' = pushNext(state), ghost' = ghost }
  action simCi(k: int): bool = all { ciCan(state, k), state' = ciNext(state, k), ghost' = ghost }
  action simView: bool = all { viewCan(state), state' = viewNext(state), ghost' = ghost }

  action simStep = {
    nondet c = CLIENTS.oneOf()
    // Until a PR exists most commands refuse: open one first.
    nondet cmd = (if (state.prs.keys().size() == 0) Set(Create(1), Create(2)) else CMDS).oneOf()
    nondet k = 1.to(MAX_TIP).oneOf()
    nondet n = 1.to(8).oneOf()
    any {
      all { n <= 3, simBegin(c, cmd) },
      all { n >= 4 and n <= 6, simRelease(c) },
      all { n == 7, simPush },
      all { n == 8, any { simCi(k), simView } },
    }
  }

  // ---------- properties that need the ghost ----------

  /// A merged record stays merged (no edit, close or ready written over it).
  val mergedIsTerminal: bool = not(ghost.everMerged) or isMerged(pr1(state))

  /// The last acknowledged edit of each field is what the record holds: no lost update.
  val editsAreKept: bool =
    not(state.prs.keys().contains(1)) or {
      val r = pr1(state)
      r.title == ghost.title and r.body == ghost.body and r.draft == ghost.draft
    }

  /// What the base has was judged ready, by the strict rule: not a commit pushed after the
  /// judgement, not one whose approval was for another.
  val landsWhatWasJudged: bool = state.landed == 0 or ghost.judged.contains(state.landed)

  val simSafety: bool = all { safety, mergedIsTerminal, editsAreKept, landsWhatWasJudged }

  // ---------- witnesses: each is violated by a run that reaches the case ----------

  val neverMerged: bool = not(state.prs.keys().exists(n => isMerged(state.prs.get(n))))
  val neverOutdated: bool = state.viewed.size() == 0
  val neverStale: bool = not(state.prs.keys().contains(1) and state.reviews.keys().exists(w => state.reviews.get(w).approve and state.reviews.get(w).commit < state.tip))
}
