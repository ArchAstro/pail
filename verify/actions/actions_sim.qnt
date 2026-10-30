// -*- mode: Bluespec; -*-
/// `actions.qnt`'s protocol under random interleavings, with a ghost remembering what was
/// concluded and accepted, for the properties that need history: at most one result is ever
/// accepted per job, a conclusion never changes, an attempt is frozen, one run is ever
/// created per event key. (The refinement cannot see ghosts, so they stay out of
/// `actions.qnt`'s state.) `npm run simulate:actions` checks `simSafety` here; `npm run
/// mutate:actions` flips each design switch of `actions.qnt` and expects a violation.
module actionsSim {
  import actions.* from "./actions"

  type Conclusion = { concl: Concl, by: str, byseq: int }
  type RunAtt = { rid: int, att: int }

  type Ghost = {
    // every runner result ever accepted for a job
    accepted: JobKey -> Set[{ by: str, byseq: int }],
    // the first conclusion of each job
    done: JobKey -> Conclusion,
    // the first conclusion of each attempt of each run
    ran: RunAtt -> Concl,
    // how many commits created the run of each event key
    creates: str -> int,
  }

  var ghost: Ghost

  pure val GHOST0: Ghost = { accepted: Map(), done: Map(), ran: Map(), creates: EVENTS.mapBy(k => 0) }

  /// What the step `a`, taking `s` to `t`, did to the ghost.
  pure def ghostAfter(g: Ghost, s: State, a: Act, t: State): Ghost = {
    val jobs = t.t1.jobs
    val byRunner = jobs.keys().filter(jk => jobs.get(jk).by != "")
    val fresh = jobs.keys().filter(jk => jobs.get(jk).status == Completed and not(g.done.keys().contains(jk)))
    val runs = t.t1.runs
    val freshRuns = runs.keys().filter(id => runs.get(id).done and not(g.ran.keys().contains({ rid: id, att: runs.get(id).att })))
    val created = match a {
      | AWrite(o) => match s.pend.get(o) {
        | Pend(c) => match c.intent {
          | ITrigger(k) => if (landable(s.t1, c)) Set(k) else Set()
          | _ => Set()
        }
        | NoPend => Set()
      }
      | _ => Set()
    }
    {
      accepted: byRunner.fold(g.accepted, (m, jk) =>
        m.put(jk, (if (m.keys().contains(jk)) m.get(jk) else Set()).union(Set({ by: jobs.get(jk).by, byseq: jobs.get(jk).byseq })))),
      done: fresh.fold(g.done, (m, jk) => m.put(jk, { concl: jobs.get(jk).concl, by: jobs.get(jk).by, byseq: jobs.get(jk).byseq })),
      ran: freshRuns.fold(g.ran, (m, id) => m.put({ rid: id, att: runs.get(id).att }, runs.get(id).concl)),
      creates: g.creates.keys().mapBy(k => g.creates.get(k) + (if (created.contains(k)) 1 else 0)),
    }
  }

  action simInit = all { state' = INITIAL, ghost' = GHOST0 }

  /// A step's kind: a trace picks among the kinds that can happen, then a step of that kind,
  /// so that the rare (a replica's write between a runner's steps) are not drowned by the many
  /// (every intent a replica might read).
  pure def kindOf(a: Act): int = match a {
    | ATick => 0
    | AFire(_) => 1
    | AApprove(_) => 2
    | ARead(_) => 3
    | AWrite(_) => 4
    | ASync => 5
    | AClaimRead(_) => 6
    | AClaimWrite(_) => 7
    | ARefresh(_) => 8
    | AStarted(_) => 9
    | AResult(_) => 10
    | ACrash(_) => 11
  }

  action simStep = {
    val all_ = acts(state)
    nondet kind = all_.map(kindOf).oneOf()
    nondet a = all_.filter(x => kindOf(x) == kind).oneOf()
    all {
      state' = apply(state, a),
      ghost' = ghostAfter(ghost, state, a, apply(state, a)),
    }
  }

  // ---------- properties that need the ghost ----------

  /// 1. At most one runner's result is ever accepted for a job (run, attempt, job).
  val atMostOneAccepted: bool = ghost.accepted.keys().forall(jk => ghost.accepted.get(jk).size() <= 1)

  /// 3. completed and its conclusion never change, nor who concluded it; a rerun is a new
  /// attempt, and its run's earlier attempts keep their conclusion.
  val finality: bool = ghost.done.keys().forall(jk => {
    val j = jobOf(state.t1, jk)
    val d = ghost.done.get(jk)
    j.status == Completed and j.concl == d.concl and j.by == d.by and j.byseq == d.byseq
  })

  val attemptsAreFrozen: bool = ghost.ran.keys().forall(x => {
    val r = runOf(state.t1, x.rid)
    r.att > x.att or (r.att == x.att and r.done and r.concl == ghost.ran.get(x))
  })

  /// 8. Exactly one commit ever created the run of an event key or cron slot.
  val oneCreationPerKey: bool = ghost.creates.keys().forall(k => ghost.creates.get(k) <= 1)

  val simSafety: bool = all { safety, atMostOneAccepted, finality, attemptsAreFrozen, oneCreationPerKey }

  // ---------- witnesses: each is violated by a run that reaches the case ----------

  /// A lost runner's job goes back to the queue and runs again.
  val neverRequeued: bool = not(state.t1.jobs.keys().exists(jk => state.t1.jobs.get(jk).lost > 0 and state.t1.jobs.get(jk).status == InProgress))
  /// A run is cancelled (superseded in its group).
  val neverCancelled: bool = not(state.t1.runs.keys().exists(id => state.t1.runs.get(id).done and state.t1.runs.get(id).concl == Cancelled))
  /// A run has a second attempt.
  val neverRerun: bool = not(state.t1.runs.keys().exists(id => state.t1.runs.get(id).att > 1))
  /// A job behind an environment gate is queued, on an approval.
  val neverApproved: bool = not(state.t1.jobs.keys().exists(jk => gated(jk.job) and (state.t1.jobs.get(jk).status == InProgress or state.t1.jobs.get(jk).status == Queued)))
  /// A job is skipped because a need failed.
  val neverSkipped: bool = not(state.t1.jobs.keys().exists(jk => state.t1.jobs.get(jk).concl == Skipped))
}
