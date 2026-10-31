// -*- mode: Bluespec; -*-
/// pail Actions (designed, not yet built): a workflow system over a bucket. Orchestrator
/// replicas own all run and job state on the `_actions` and `_checks` branches (Tier 1);
/// runners only touch plain objects under `<repo>.actions/` (Tier 2: queue, leases, progress,
/// results, cancel markers). The orchestrator is the one bridge: it reads what runners
/// reported, checks it against the job file, and commits the transition. `actions_sim.qnt`
/// simulates random interleavings; `actions_scenarios.qnt` has the runs the orchestrator and
/// runner will be refined against.
///
/// A replica's tick is `read`, then `write`: `read` observes Tier 1 and Tier 2 and decides
/// one intent, which becomes a commit held at its write (`Pend`); `write` lets it land, or
/// finds a conflict (the replica reads again). Between the two, anything may have happened,
/// so a replica always writes from a stale read. The intents (one per commit; the real
/// orchestrator batches several in one commit, which only means more of them land together):
///
///   ITrigger   a run from an event key (push, PR synchronize, a cron slot): `runs/<id>`,
///              `claims/<key>` (the key), its jobs and `concurrency/<group>`, one commit
///   IPromote   a pending job whose `needs` all succeeded is queued, or waits for its
///              environment's approval; one whose need failed is skipped
///   IStart     a queued job whose lease a runner holds is in_progress (names holder, seq)
///   IAccept    a runner's result becomes the job's conclusion, if fenced
///   ISweep     a job whose lease expired, with no result: requeue, or "runner lost"
///   IFinish    all jobs of the attempt are completed: the run is
///   IRerun     a new attempt: successes copied, the rest fresh; the old attempt is frozen
///   ICheck     the `_checks` projection of a completed job
///
/// Tier 1 guards: a write of a record the replica read is `put --expect` (CAS: the record
/// is as read); a write of a record it did not find is `--if-absent`. Each guard can be off
/// below. A runner claims a job by creating its lease (generation 0 matches one writer), or
/// takes over an expired one by generation match; it refreshes, reports a step started,
/// writes a create-only result, sees a cancel marker, or crashes. Time is `now` and `TTL`.
///
/// Small on purpose: one workflow, jobs `build`, `test` and `deploy` (both need `build`;
/// `deploy` is gated by environment `prod`), runs from three event keys (two pushes in one
/// concurrency group with cancel-in-progress, one cron slot), at most two attempts.
/// Matrices, expressions, artifacts, logs, retention and the leader lease (correctness
/// never depends on it: every write is guarded) are left out.
module actions {
  // ---------- the world ----------

  pure val RUNNERS: Set[str] = Set("r1", "r2", "r3")
  pure val REPLICAS: Set[str] = Set("o1", "o2")
  pure val APPROVERS: Set[str] = Set("alice", "bob")
  pure val EVENTS: Set[str] = Set("push1", "push2", "cron1")
  pure val JOBS: Set[str] = Set("build", "test", "deploy")
  pure val MAX_RUNS = 3
  pure val MAX_ATT = 2
  /// A lease lives `TTL` ticks past its last refresh.
  pure val TTL = 2
  pure val MAX_T = 6

  // The protocol's design choices. Each `true` is the fix; `false` is the bug it prevents,
  // and what `npm run mutate:actions` shows `safety` (and `simSafety`) catching.
  /// Writes of records the replica read are compare-and-swap (else blind overwrite).
  pure val CAS = true
  /// Writes of records it did not find are `--if-absent` (else blind overwrite): run ids,
  /// event and slot claims, the concurrency file.
  pure val IF_ABSENT = true
  /// A commit expects every record it read, not only those it writes (the concurrency file
  /// on a rerun that keeps the holder; the run on a sweep; the group's previous holder; the
  /// running jobs of a run it cancels).
  pure val GUARD_READS = true
  /// A runner claims a lease by create or generation match (else a blind put).
  pure val LEASE_CREATE_ONLY = true
  /// Intake accepts a result only if the job file names its runner and seq (else any
  /// result of an in_progress job).
  pure val FENCE = true
  /// ... and that is all it checks: the job file's holder and seq, not also the live lease's
  /// (the plan's "lease seq matches": a lease taken over after the job started would
  /// strand the job).
  pure val FENCE_ON_JOB = true
  /// A result is created at a path with the lease seq (else one per job: a late result of
  /// a lost runner blocks the next holder's).
  pure val RESULT_PER_LEASE = true
  /// The sweep exists: an expired lease with no result requeues or concludes the job.
  pure val SWEEP = true
  /// A job is queued only when its `needs` succeeded; one whose need failed is skipped.
  pure val NEEDS_CHECK = true
  /// An approval counts only from someone other than the run's actor.
  pure val APPROVAL_EXCLUDES_ACTOR = true
  /// A new run cancels the group's previous holder (cancel-in-progress).
  pure val CANCEL_IN_PROGRESS = true
  /// A rerun is a new attempt (else it resets the attempt's jobs in place).
  pure val RERUN_NEW_ATTEMPT = true
  /// `_checks` is projected from the accepted job (else from the runners' raw results).
  pure val CHECK_FROM_JOB = true

  /// The orchestrator also ticks every so often, whatever it was told (the events model's
  /// safety poll, `verify/events/events.qnt`), and when it starts (the cursor resume). Else
  /// it ticks only on events, and a lease that expires, which writes nothing and so announces
  /// nothing, strands its job.
  pure val SAFETY_POLL = true

  pure def needsOf(j: str): Set[str] = if (j == "build") Set() else Set("build")
  /// The job has `environment: prod`.
  pure def gated(j: str): bool = j == "deploy"
  /// The concurrency group of a run ("": none).
  pure def groupOf(k: str): str = if (k == "cron1") "" else "main"
  pure def shaOf(k: str): int = if (k == "push1") 1 else 2
  pure def actorOf(k: str): str = if (k == "push1") "alice" else if (k == "push2") "bob" else "cron"

  // ---------- Tier 1: `_actions` and `_checks` ----------

  type JobKey = { rid: int, att: int, job: str }
  type Status = Pending | Queued | Waiting | InProgress | Completed
  type Concl = NoConcl | Success | Failure | Cancelled | Skipped

  /// `holder`, `seq`: the lease it is (or was last) in progress under. `by`, `byseq`: the
  /// runner and seq whose result concluded it ("": none, as for skipped, lost, copied).
  type Job = {
    status: Status, concl: Concl, holder: str, seq: int, lost: int,
    by: str, byseq: int, copied: bool,
  }
  /// `runs/<id>.json`; `key` is the event key or cron slot it was created for.
  type Run = { key: str, att: int, done: bool, concl: Concl, cancel: bool }
  /// `_checks:<sha>/ci.<job>.json`
  type CheckKey = { sha: int, job: str }
  type Check = { rid: int, att: int, state: Concl }
  /// `runs/<id>/<att>/approvals/prod/<who>.json` (create-only)
  type Approval = { rid: int, att: int, who: str }

  type T1 = {
    runs: int -> Run,
    // `claims/<key>`: the run created for an event key or cron slot
    claims: str -> int,
    jobs: JobKey -> Job,
    // `concurrency/<group>`: the newest run
    groups: str -> int,
    checks: CheckKey -> Check,
    approvals: Set[Approval],
  }

  // ---------- Tier 2: `<repo>.actions/` ----------

  /// `holder` "" and `seq` 0: no lease. `seq` grows with every claim.
  type Lease = { holder: str, seq: int, exp: int }
  /// A result's path: the job and (with `RESULT_PER_LEASE`) the lease seq.
  type ResKey = { jk: JobKey, seq: int }
  type Result = { runner: str, seq: int, concl: Concl }

  type T2 = {
    queue: Set[JobKey],
    leases: JobKey -> Lease,
    results: ResKey -> Result,
    cancels: Set[JobKey],
    // progress: a step started under this lease seq
    started: Set[ResKey],
  }

  // ---------- orchestrator replicas, runners ----------

  type Intent =
    | ITrigger(str)
    | IPromote(JobKey)
    | IStart(JobKey)
    | IAccept(ResKey)
    | ISweep(JobKey)
    | IFinish(int)
    | IRerun(int)
    | ICheck(JobKey)

  /// What a decision writes, and the records it read without writing them.
  type Delta = { w: T1, rdRuns: Set[int], rdJobs: Set[JobKey], rdGroups: Set[str] }
  type Dec = Refuse | Do(Delta)
  /// A commit held at its write: what Tier 1 was when it read, and what it writes.
  type Commit = { intent: Intent, snap: T1, d: Delta }
  type PendO = NoPend | Pend(Commit)

  type RPhase =
    | RIdle
    // read the queue entry and the lease (`seen`); held at the lease write
    | RReading({ jk: JobKey, seen: Lease })
    | RHolding({ jk: JobKey, seq: int, started: bool })
  type Runner = { alive: bool, ph: RPhase }

  type State = {
    now: int,
    // the events that happened (a push, a PR synchronize, a cron slot came due)
    occurred: Set[str],
    t1: T1,
    t2: T2,
    runners: str -> Runner,
    pend: str -> PendO,
    // the replicas with a wake-up pending: an event announced a write, the safety poll came
    // due, or the replica started. A replica ticks (`read`) only when woken.
    woken: Set[str],
  }

  pure val EMPTY1: T1 = { runs: Map(), claims: Map(), jobs: Map(), groups: Map(), checks: Map(), approvals: Set() }

  pure val INITIAL: State = {
    now: 0,
    occurred: Set(),
    t1: EMPTY1,
    t2: { queue: Set(), leases: Map(), results: Map(), cancels: Set(), started: Set() },
    runners: RUNNERS.mapBy(r => { alive: true, ph: RIdle }),
    pend: REPLICAS.mapBy(o => NoPend),
    woken: REPLICAS,
  }

  // ---------- reading ----------

  pure val NOJOB: Job = { status: Pending, concl: NoConcl, holder: "", seq: 0, lost: 0, by: "", byseq: 0, copied: false }
  pure val NORUN: Run = { key: "", att: 0, done: false, concl: NoConcl, cancel: false }
  pure val NOLEASE: Lease = { holder: "", seq: 0, exp: 0 }

  pure def jobOf(v: T1, jk: JobKey): Job = if (v.jobs.keys().contains(jk)) v.jobs.get(jk) else NOJOB
  pure def runOf(v: T1, id: int): Run = if (v.runs.keys().contains(id)) v.runs.get(id) else NORUN
  pure def leaseOf(t: T2, jk: JobKey): Lease = if (t.leases.keys().contains(jk)) t.leases.get(jk) else NOLEASE
  pure def groupHolder(v: T1, g: str): int = if (v.groups.keys().contains(g)) v.groups.get(g) else 0

  pure def needOf(jk: JobKey, n: str): JobKey = { rid: jk.rid, att: jk.att, job: n }
  pure def concluded(v: T1, jk: JobKey): bool = v.jobs.keys().contains(jk) and v.jobs.get(jk).status == Completed
  pure def succeeded(v: T1, jk: JobKey): bool = concluded(v, jk) and v.jobs.get(jk).concl == Success
  pure def needsMet(v: T1, jk: JobKey): bool = needsOf(jk.job).forall(n => succeeded(v, needOf(jk, n)))
  pure def needFailed(v: T1, jk: JobKey): bool =
    needsOf(jk.job).exists(n => concluded(v, needOf(jk, n)) and v.jobs.get(needOf(jk, n)).concl != Success)

  pure def isActive(st: Status): bool = match st {
    | Pending => true
    | Queued => true
    | Waiting => true
    | _ => false
  }

  /// An approval for the job's run and attempt; `strict`: by someone other than the actor.
  pure def approved(v: T1, jk: JobKey, strict: bool): bool =
    v.approvals.exists(a => a.rid == jk.rid and a.att == jk.att and (not(strict) or a.who != actorOf(runOf(v, jk.rid).key)))

  /// A result's path: its seq is the lease's, or 0.
  pure def resSeq(seq: int): int = if (RESULT_PER_LEASE) seq else 0

  pure def liveFor(s: State, jk: JobKey, j: Job): bool = {
    val l = leaseOf(s.t2, jk)
    l.holder == j.holder and l.seq == j.seq and l.exp > s.now
  }

  // ---------- what the orchestrator decides ----------

  pure def overlay(a: k -> v, b: k -> v): k -> v = b.keys().fold(a, (m, x) => m.put(x, b.get(x)))

  pure def merge(a: T1, b: T1): T1 = {
    runs: overlay(a.runs, b.runs),
    claims: overlay(a.claims, b.claims),
    jobs: overlay(a.jobs, b.jobs),
    groups: overlay(a.groups, b.groups),
    checks: overlay(a.checks, b.checks),
    approvals: Set(),
  }

  pure def jobDelta(jk: JobKey, j: Job): T1 = { ...EMPTY1, jobs: Map(jk -> j) }
  pure def runDelta(id: int, r: Run): T1 = { ...EMPTY1, runs: Map(id -> r) }
  pure def doing(w: T1): Dec = Do({ w: w, rdRuns: Set(), rdJobs: Set(), rdGroups: Set() })

  /// A job's first state: queued at once if nothing holds it back, else pending.
  pure def freshJob(j: str): Job = {
    ...NOJOB,
    status: if (needsOf(j).size() == 0 and not(gated(j))) Queued else Pending,
  }

  pure def freshJobs(id: int, att: int): JobKey -> Job =
    JOBS.map(j => { rid: id, att: att, job: j }).mapBy(jk => freshJob(jk.job))

  /// Making run `id` the newest of its group: the previous holder, if it is still going, is
  /// cancelled (its jobs not yet started are completed cancelled; the markers for the running
  /// ones are made by `sync`). Writes `concurrency/<group>` when the holder changes.
  pure def claimGroup(v: T1, id: int, key: str): Delta = {
    val g = groupOf(key)
    val prev = groupHolder(v, g)
    val pr = runOf(v, prev)
    val cancelling = CANCEL_IN_PROGRESS and prev != 0 and prev != id and v.runs.keys().contains(prev) and not(pr.done)
    val doomed = v.jobs.keys().filter(jk => cancelling and jk.rid == prev and jk.att == pr.att and isActive(v.jobs.get(jk).status))
    val w: T1 = {
      ...EMPTY1,
      runs: if (cancelling) Map(prev -> { ...pr, cancel: true }) else Map(),
      jobs: doomed.mapBy(jk => { ...v.jobs.get(jk), status: Completed, concl: Cancelled }),
      groups: if (prev == id) Map() else Map(g -> id),
    }
    // it read the previous holder (done, or not: a rerun of it that lands in between must be
    // seen), and every job of the run it cancels (a running one decides the marker, and a
    // sweep or a start of one that lands between must not be overwritten by a cancelled run's
    // job going back to the queue)
    val read = v.jobs.keys().filter(jk => cancelling and jk.rid == prev and jk.att == pr.att)
    { w: w, rdRuns: if (prev != 0 and prev != id) Set(prev) else Set(), rdJobs: read, rdGroups: Set(g) }
  }

  pure def withBase(d: Delta, base: T1): Delta = { ...d, w: merge(d.w, base) }

  pure def maxId(v: T1): int = v.runs.keys().fold(0, (m, k) => if (k > m) k else m)

  pure def decideTrigger(s: State, k: str): Dec = {
    val v = s.t1
    val id = maxId(v) + 1
    if (not(s.occurred.contains(k)) or v.claims.keys().contains(k) or id > MAX_RUNS) Refuse
    else {
      val base: T1 = {
        ...EMPTY1,
        runs: Map(id -> { key: k, att: 1, done: false, concl: NoConcl, cancel: false }),
        claims: Map(k -> id),
        jobs: freshJobs(id, 1),
      }
      if (groupOf(k) == "") doing(base) else Do(withBase(claimGroup(v, id, k), base))
    }
  }

  pure def decidePromote(s: State, jk: JobKey): Dec = {
    val v = s.t1
    val j = jobOf(v, jk)
    val r = runOf(v, jk.rid)
    val target = if (gated(jk.job) and not(approved(v, jk, APPROVAL_EXCLUDES_ACTOR))) Waiting else Queued
    if (not(v.jobs.keys().contains(jk)) or r.att != jk.att or not(j.status == Pending or j.status == Waiting)) Refuse
    else if (NEEDS_CHECK and j.status == Pending and needFailed(v, jk))
      doing(jobDelta(jk, { ...j, status: Completed, concl: Skipped }))
    else if (NEEDS_CHECK and not(needsMet(v, jk))) Refuse
    else if (j.status == Waiting and target == Waiting) Refuse
    else doing(jobDelta(jk, { ...j, status: target }))
  }

  pure def decideStart(s: State, jk: JobKey): Dec = {
    val j = jobOf(s.t1, jk)
    val l = leaseOf(s.t2, jk)
    if (j.status == Queued and l.seq > 0 and l.exp > s.now)
      doing(jobDelta(jk, { ...j, status: InProgress, holder: l.holder, seq: l.seq }))
    else Refuse
  }

  pure def decideAccept(s: State, rk: ResKey): Dec = {
    val j = jobOf(s.t1, rk.jk)
    if (not(s.t2.results.keys().contains(rk)) or j.status != InProgress) Refuse
    else {
      val res = s.t2.results.get(rk)
      val fenced = res.runner == j.holder and res.seq == j.seq and (FENCE_ON_JOB or leaseOf(s.t2, rk.jk).seq == res.seq)
      if (FENCE and not(fenced)) Refuse
      else doing(jobDelta(rk.jk, { ...j, status: Completed, concl: res.concl, by: res.runner, byseq: res.seq }))
    }
  }

  pure def decideSweep(s: State, jk: JobKey): Dec = {
    val j = jobOf(s.t1, jk)
    val r = runOf(s.t1, jk.rid)
    val seen: ResKey = { jk: jk, seq: resSeq(j.seq) }
    if (not(SWEEP) or j.status != InProgress or liveFor(s, jk, j) or s.t2.results.keys().contains(seen)) Refuse
    else {
      val nj =
        if (r.cancel) { ...j, status: Completed, concl: Cancelled }
        else if (s.t2.started.contains({ jk: jk, seq: j.seq })) { ...j, status: Completed, concl: Failure, lost: j.lost + 1 }
        else { ...j, status: Queued, holder: "", lost: j.lost + 1 }
      Do({ w: jobDelta(jk, nj), rdRuns: Set(jk.rid), rdJobs: Set(), rdGroups: Set() })
    }
  }

  pure def decideFinish(s: State, id: int): Dec = {
    val v = s.t1
    val r = runOf(v, id)
    val jks = JOBS.map(j => { rid: id, att: r.att, job: j })
    if (not(v.runs.keys().contains(id)) or r.done or not(jks.forall(jk => concluded(v, jk)))) Refuse
    else {
      val concl =
        if (r.cancel) Cancelled
        else if (jks.exists(jk => v.jobs.get(jk).concl == Failure)) Failure
        else Success
      doing(runDelta(id, { ...r, done: true, concl: concl }))
    }
  }

  pure def decideRerun(s: State, id: int): Dec = {
    val v = s.t1
    val r = runOf(v, id)
    val att = if (RERUN_NEW_ATTEMPT) r.att + 1 else r.att
    if (not(v.runs.keys().contains(id)) or not(r.done) or r.concl == Success or att > MAX_ATT) Refuse
    else {
      // successes are copied (frozen), the rest start over
      val jobs = JOBS.map(j => { rid: id, att: att, job: j }).mapBy(jk => {
        val old = jobOf(v, { rid: id, att: r.att, job: jk.job })
        if (old.status == Completed and old.concl == Success) { ...old, copied: true } else freshJob(jk.job)
      })
      val base = { ...runDelta(id, { ...r, att: att, done: false, concl: NoConcl, cancel: false }), jobs: jobs }
      Do(withBase(claimGroup(v, id, r.key), base))
    }
  }

  pure def decideCheck(s: State, jk: JobKey): Dec = {
    val v = s.t1
    val j = jobOf(v, jk)
    val r = runOf(v, jk.rid)
    if (not(v.jobs.keys().contains(jk)) or j.status != Completed or jk.att != r.att) Refuse
    else {
      val raw = s.t2.results.keys().exists(rk => rk.jk == jk and s.t2.results.get(rk).concl == Success)
      val state = if (CHECK_FROM_JOB or not(raw)) j.concl else Success
      val ck: CheckKey = { sha: shaOf(r.key), job: jk.job }
      val ch: Check = { rid: jk.rid, att: jk.att, state: state }
      if (v.checks.keys().contains(ck) and v.checks.get(ck) == ch) Refuse
      else Do({ w: { ...EMPTY1, checks: Map(ck -> ch) }, rdRuns: Set(jk.rid), rdJobs: Set(), rdGroups: Set() })
    }
  }

  pure def decide(s: State, i: Intent): Dec = match i {
    | ITrigger(k) => decideTrigger(s, k)
    | IPromote(jk) => decidePromote(s, jk)
    | IStart(jk) => decideStart(s, jk)
    | IAccept(rk) => decideAccept(s, rk)
    | ISweep(jk) => decideSweep(s, jk)
    | IFinish(id) => decideFinish(s, id)
    | IRerun(id) => decideRerun(s, id)
    | ICheck(jk) => decideCheck(s, jk)
  }

  pure def deciding(s: State, i: Intent): bool = match decide(s, i) {
    | Refuse => false
    | Do(_) => true
  }

  // ---------- committing ----------

  /// The guard of a write: a record read (`snap`) must still be as read (`CAS`); one that was
  /// absent must still be (`IF_ABSENT`). `rd`: records read but not written.
  pure def guardOk(cur: k -> v, snap: k -> v, wr: Set[k], rd: Set[k]): bool =
    wr.union(if (GUARD_READS) rd else Set()).forall(x =>
      if (snap.keys().contains(x)) (not(CAS) or (cur.keys().contains(x) and cur.get(x) == snap.get(x)))
      else (not(IF_ABSENT) or not(cur.keys().contains(x))))

  pure def landable(cur: T1, c: Commit): bool = all {
    guardOk(cur.runs, c.snap.runs, c.d.w.runs.keys(), c.d.rdRuns),
    guardOk(cur.claims, c.snap.claims, c.d.w.claims.keys(), Set()),
    guardOk(cur.jobs, c.snap.jobs, c.d.w.jobs.keys(), c.d.rdJobs),
    guardOk(cur.groups, c.snap.groups, c.d.w.groups.keys(), c.d.rdGroups),
    guardOk(cur.checks, c.snap.checks, c.d.w.checks.keys(), Set()),
  }

  pure def landed(cur: T1, c: Commit): T1 = { ...merge(cur, c.d.w), approvals: cur.approvals }

  // ---------- steps ----------

  pure def isPend(p: PendO): bool = match p {
    | Pend(_) => true
    | NoPend => false
  }

  // ---------- wake-ups: the events model's contract ----------
  //
  // `pail events --branch '*' --prefix actions/ --cursor ...` runs beside each replica and
  // wakes it. Its contract (`events.qnt`): a notification is only a hint; one may be lost,
  // repeated or reordered (a wake is a flag, so repeating and reordering change nothing, and
  // `lose` drops it); the safety poll re-lists everything every so often and the start-up
  // diff against the cursor emits what changed while the process was down, so a wake lost
  // only delays a tick (`wake`). What announces nothing, the passing of time, wakes no one.

  /// A write to a branch or an object of the area was announced to every replica.
  pure def announced(s: State): State = s.with("woken", REPLICAS)

  pure def wakeCan(s: State, o: str): bool = SAFETY_POLL and not(s.woken.contains(o))
  pure def wakeNext(s: State, o: str): State = s.with("woken", s.woken.union(Set(o)))

  pure def loseCan(s: State, o: str): bool = s.woken.contains(o)
  pure def loseNext(s: State, o: str): State = s.with("woken", s.woken.exclude(Set(o)))

  /// Some replica will tick: it is woken, mid-commit (its write is a ref event and a retry),
  /// or the safety poll will come.
  pure def willTick(s: State): bool = SAFETY_POLL or s.woken.size() > 0 or REPLICAS.exists(o => isPend(s.pend.get(o)))

  pure def readCan(s: State, o: str, i: Intent): bool =
    not(isPend(s.pend.get(o))) and s.woken.contains(o) and deciding(s, i)

  /// A tick takes the wake-up it answers.
  pure def readNext(s: State, o: str, i: Intent): State = match decide(s, i) {
    | Refuse => s
    | Do(d) => s.with("pend", s.pend.set(o, Pend({ intent: i, snap: s.t1, d: d })))
      .with("woken", s.woken.exclude(Set(o)))
  }

  pure def writeCan(s: State, o: str): bool = isPend(s.pend.get(o))

  /// The commit lands, or finds a conflict; either way the replica is done with it.
  pure def writeNext(s: State, o: str): State = match s.pend.get(o) {
    | NoPend => s
    | Pend(c) =>
      // a conflict sends the replica back to read again; a landed commit is announced to all
      val s2 = s.with("pend", s.pend.set(o, NoPend)).with("woken", s.woken.union(Set(o)))
      if (landable(s.t1, c)) announced(s2.with("t1", landed(s.t1, c))) else s2
  }

  /// The orchestrator keeps the queue to the queued jobs, and marks the running jobs of
  /// cancelled runs. (Both are idempotent Tier 2 writes, and may lag.)
  pure def syncNext(s: State): State = {
    val v = s.t1
    val t = s.with("t2", {
      ...s.t2,
      queue: v.jobs.keys().filter(jk => v.jobs.get(jk).status == Queued),
      cancels: s.t2.cancels.union(v.jobs.keys().filter(jk => v.jobs.get(jk).status == InProgress and runOf(v, jk.rid).cancel)),
    })
    if (t.t2 == s.t2) s else announced(t)
  }

  pure def tickCan(s: State): bool = s.now < MAX_T
  pure def tickNext(s: State): State = s.with("now", s.now + 1)

  pure def setRunner(s: State, r: str, ph: RPhase): State =
    s.with("runners", s.runners.set(r, { alive: s.runners.get(r).alive, ph: ph }))

  pure def setLease(s: State, jk: JobKey, l: Lease): State =
    s.with("t2", { ...s.t2, leases: s.t2.leases.put(jk, l) })

  /// A runner reads a queue entry and the lease, free or expired.
  pure def claimReadCan(s: State, r: str, jk: JobKey): bool = all {
    s.runners.get(r).alive,
    s.runners.get(r).ph == RIdle,
    s.t2.queue.contains(jk),
    leaseOf(s.t2, jk).exp <= s.now,
  }

  pure def claimReadNext(s: State, r: str, jk: JobKey): State =
    setRunner(s, r, RReading({ jk: jk, seen: leaseOf(s.t2, jk) }))

  pure def readingOf(s: State, r: str): bool = match s.runners.get(r).ph {
    | RReading(_) => s.runners.get(r).alive
    | _ => false
  }

  /// The lease write: create (generation 0) or take over (the generation read), one winner.
  pure def claimWriteNext(s: State, r: str): State = match s.runners.get(r).ph {
    | RReading(x) =>
      if (not(LEASE_CREATE_ONLY) or leaseOf(s.t2, x.jk) == x.seen)
        announced(setRunner(setLease(s, x.jk, { holder: r, seq: x.seen.seq + 1, exp: s.now + TTL }), r,
          RHolding({ jk: x.jk, seq: x.seen.seq + 1, started: false })))
      else setRunner(s, r, RIdle)
    | _ => s
  }

  pure def holdingOf(s: State, r: str): bool = match s.runners.get(r).ph {
    | RHolding(_) => s.runners.get(r).alive
    | _ => false
  }

  /// The refresh is a generation match: it fails once another holds the lease, and the runner
  /// stops. One whose lease merely expired still succeeds, if nobody took it over.
  pure def refreshNext(s: State, r: str): State = match s.runners.get(r).ph {
    | RHolding(x) =>
      val l = leaseOf(s.t2, x.jk)
      if (l.holder == r and l.seq == x.seq) announced(setLease(s, x.jk, { ...l, exp: s.now + TTL }))
      else setRunner(s, r, RIdle)
    | _ => s
  }

  pure def startedCan(s: State, r: str): bool = match s.runners.get(r).ph {
    | RHolding(x) => s.runners.get(r).alive and not(x.started)
    | _ => false
  }

  pure def startedNext(s: State, r: str): State = match s.runners.get(r).ph {
    | RHolding(x) =>
      announced(setRunner(s, r, RHolding({ ...x, started: true }))
        .with("t2", { ...s.t2, started: s.t2.started.union(Set({ jk: x.jk, seq: x.seq })) }))
    | _ => s
  }

  /// Finished its steps, or (`Cancelled`) saw the cancel marker: the create-only result.
  pure def resultCan(s: State, r: str, c: Concl): bool = match s.runners.get(r).ph {
    | RHolding(x) => s.runners.get(r).alive and (
      if (c == Cancelled) s.t2.cancels.contains(x.jk) else x.started and (c == Success or c == Failure))
    | _ => false
  }

  pure def resultNext(s: State, r: str, c: Concl): State = match s.runners.get(r).ph {
    | RHolding(x) =>
      val rk: ResKey = { jk: x.jk, seq: resSeq(x.seq) }
      if (s.t2.results.keys().contains(rk)) setRunner(s, r, RIdle)
      else announced(setRunner(s, r, RIdle).with("t2", { ...s.t2, results: s.t2.results.put(rk, { runner: r, seq: x.seq, concl: c }) }))
    | _ => s
  }

  pure def crashNext(s: State, r: str): State =
    s.with("runners", s.runners.set(r, { alive: false, ph: RIdle }))

  pure def approveCan(s: State, a: Approval): bool =
    s.t1.runs.keys().contains(a.rid) and a.att <= s.t1.runs.get(a.rid).att and not(s.t1.approvals.contains(a))

  pure def approveNext(s: State, a: Approval): State =
    announced(s.with("t1", { ...s.t1, approvals: s.t1.approvals.union(Set(a)) }))

  // ---------- every step, as data (so that the simulation can wrap each) ----------

  type Act =
    | ATick
    | AFire(str)
    | AApprove(Approval)
    | ARead({ o: str, i: Intent })
    | AWrite(str)
    | ASync
    | AClaimRead({ r: str, jk: JobKey })
    | AClaimWrite(str)
    | ARefresh(str)
    | AStarted(str)
    | AResult({ r: str, c: Concl })
    | ACrash(str)
    | AWake(str)
    | ALose(str)

  pure def intentsOf(s: State): Set[Intent] = {
    val js = s.t1.jobs.keys()
    EVENTS.map(k => ITrigger(k))
      .union(js.map(jk => IPromote(jk)))
      .union(js.map(jk => IStart(jk)))
      .union(js.map(jk => ISweep(jk)))
      .union(js.map(jk => ICheck(jk)))
      .union(s.t2.results.keys().map(rk => IAccept(rk)))
      .union(s.t1.runs.keys().map(id => IFinish(id)))
      .union(s.t1.runs.keys().map(id => IRerun(id)))
      .filter(i => deciding(s, i))
  }

  pure def unions(xs: Set[Set[a]]): Set[a] = xs.fold(Set(), (acc, x) => acc.union(x))

  /// Every step that may happen in `s`.
  pure def acts(s: State): Set[Act] = {
    val live = RUNNERS.filter(r => s.runners.get(r).alive)
    val approvals = unions(s.t1.runs.keys().map(id => unions(1.to(MAX_ATT).map(a => APPROVERS.map(w => { rid: id, att: a, who: w })))))
      .filter(a => approveCan(s, a))
    val intents = intentsOf(s)
    Set(ATick).filter(a => tickCan(s))
      .union(EVENTS.filter(k => not(s.occurred.contains(k))).map(k => AFire(k)))
      .union(approvals.map(a => AApprove(a)))
      .union(unions(REPLICAS.filter(o => not(isPend(s.pend.get(o)))).map(o => intents.map(i => ARead({ o: o, i: i })))))
      .union(REPLICAS.filter(o => writeCan(s, o)).map(o => AWrite(o)))
      .union(Set(ASync))
      .union(unions(live.map(r => s.t2.queue.filter(jk => claimReadCan(s, r, jk)).map(jk => AClaimRead({ r: r, jk: jk })))))
      .union(live.filter(r => readingOf(s, r)).map(r => AClaimWrite(r)))
      .union(live.filter(r => holdingOf(s, r)).map(r => ARefresh(r)))
      .union(live.filter(r => startedCan(s, r)).map(r => AStarted(r)))
      .union(unions(live.map(r => Set(Success, Failure, Cancelled).filter(c => resultCan(s, r, c)).map(c => AResult({ r: r, c: c })))))
      .union(live.map(r => ACrash(r)))
      .union(REPLICAS.filter(o => wakeCan(s, o)).map(o => AWake(o)))
      .union(REPLICAS.filter(o => loseCan(s, o)).map(o => ALose(o)))
  }

  pure def apply(s: State, a: Act): State = match a {
    | ATick => tickNext(s)
    | AFire(k) => announced(s.with("occurred", s.occurred.union(Set(k))))
    | AApprove(x) => approveNext(s, x)
    | ARead(x) => readNext(s, x.o, x.i)
    | AWrite(o) => writeNext(s, o)
    | ASync => syncNext(s)
    | AClaimRead(x) => claimReadNext(s, x.r, x.jk)
    | AClaimWrite(r) => claimWriteNext(s, r)
    | ARefresh(r) => refreshNext(s, r)
    | AStarted(r) => startedNext(s, r)
    | AResult(x) => resultNext(s, x.r, x.c)
    | ACrash(r) => crashNext(s, r)
    | AWake(o) => wakeNext(s, o)
    | ALose(o) => loseNext(s, o)
  }

  // ---------- actions ----------

  var state: State

  action init = state' = INITIAL

  action tick: bool = all { tickCan(state), state' = tickNext(state) }
  action fire(k: str): bool = all { not(state.occurred.contains(k)), state' = apply(state, AFire(k)) }
  action approve(who: str, id: int, att: int): bool = all {
    approveCan(state, { rid: id, att: att, who: who }),
    state' = approveNext(state, { rid: id, att: att, who: who }),
  }
  action read(o: str, i: Intent): bool = all { readCan(state, o, i), state' = readNext(state, o, i) }
  action write(o: str): bool = all { writeCan(state, o), state' = writeNext(state, o) }
  action sync: bool = state' = syncNext(state)
  action claimRead(r: str, jk: JobKey): bool = all {
    claimReadCan(state, r, jk),
    state' = claimReadNext(state, r, jk),
  }
  action claimWrite(r: str): bool = all { readingOf(state, r), state' = claimWriteNext(state, r) }
  action refresh(r: str): bool = all { holdingOf(state, r), state' = refreshNext(state, r) }
  action started(r: str): bool = all { startedCan(state, r), state' = startedNext(state, r) }
  action result(r: str, c: Concl): bool = all { resultCan(state, r, c), state' = resultNext(state, r, c) }
  action crash(r: str): bool = all { state.runners.get(r).alive, state' = crashNext(state, r) }
  action wake(o: str): bool = all { wakeCan(state, o), state' = wakeNext(state, o) }
  action lose(o: str): bool = all { loseCan(state, o), state' = loseNext(state, o) }

  /// Any interleaving: one enabled step, uniformly.
  action step = {
    nondet a = acts(state).oneOf()
    state' = apply(state, a)
  }

  // ---------- properties ----------

  /// 2. No job stays in_progress once its lease has expired: it is live (its holder's lease
  /// has not), or the orchestrator can accept its result or sweep it, and some replica will
  /// tick to do it (`willTick`: woken, mid-commit, or the safety poll will come).
  val noStranded: bool = state.t1.jobs.keys().forall(jk => {
    val j = state.t1.jobs.get(jk)
    j.status != InProgress or liveFor(state, jk, j) or (willTick(state) and (
      state.t2.results.keys().exists(rk => rk.jk == jk and deciding(state, IAccept(rk))) or deciding(state, ISweep(jk))))
  })

  /// 4. A job queued, waiting, started or concluded by a runner has all its needs succeeded;
  /// a skipped one has a need that did not.
  val ordering: bool = state.t1.jobs.keys().forall(jk => {
    val j = state.t1.jobs.get(jk)
    val ran = j.status != Pending and (j.status != Completed or j.concl == Success or j.concl == Failure)
    (not(ran) or needsMet(state.t1, jk)) and (j.concl != Skipped or needFailed(state.t1, jk))
  })

  /// 5. A `_checks` success names a job completed success by an accepted result (or copied
  /// from an attempt that had one), of a run at that sha.
  val checksEarned: bool = state.t1.checks.keys().forall(ck => {
    val c = state.t1.checks.get(ck)
    val jk: JobKey = { rid: c.rid, att: c.att, job: ck.job }
    c.state != Success or (
      state.t1.jobs.keys().contains(jk) and succeeded(state.t1, jk) and
      (state.t1.jobs.get(jk).by != "" or state.t1.jobs.get(jk).copied) and
      shaOf(runOf(state.t1, c.rid).key) == ck.sha)
  })

  /// 6. At most one run per concurrency group is active and not cancelled.
  val oneActivePerGroup: bool = Set("main").forall(g =>
    state.t1.runs.keys().filter(id => groupOf(state.t1.runs.get(id).key) == g and not(state.t1.runs.get(id).done) and not(state.t1.runs.get(id).cancel)).size() <= 1)

  /// A cancelled run has nothing left to start.
  val cancelledIsIdle: bool = state.t1.runs.keys().forall(id => {
    val r = state.t1.runs.get(id)
    not(r.cancel) or not(state.t1.jobs.keys().exists(jk => jk.rid == id and jk.att == r.att and isActive(state.t1.jobs.get(jk).status)))
  })

  /// 7. A job with an environment reaches queued (and on) only with an approval from
  /// someone other than the actor, whatever `APPROVAL_EXCLUDES_ACTOR` says.
  val gatesHold: bool = state.t1.jobs.keys().forall(jk => {
    val j = state.t1.jobs.get(jk)
    val ran = j.status == Queued or j.status == InProgress or (j.status == Completed and (j.concl == Success or j.concl == Failure) and not(j.copied))
    not(gated(jk.job)) or not(ran) or approved(state.t1, jk, true)
  })

  /// 8. One run per event key or cron slot: its claim names a run of that key, and every run
  /// is the one its key claims.
  val oneRunPerKey: bool =
    state.t1.claims.keys().forall(k => state.t1.runs.keys().contains(state.t1.claims.get(k)) and state.t1.runs.get(state.t1.claims.get(k)).key == k) and
    state.t1.runs.keys().forall(id => state.t1.claims.keys().contains(state.t1.runs.get(id).key) and state.t1.claims.get(state.t1.runs.get(id).key) == id)

  /// 1, in state terms: a conclusion by a runner is by the holder and seq the job started under.
  val acceptedWasHolder: bool = state.t1.jobs.keys().forall(jk => {
    val j = state.t1.jobs.get(jk)
    j.by == "" or (j.by == j.holder and j.byseq == j.seq)
  })

  /// A runner holds a lease alone: no two runners hold one job under one seq.
  val leaseExclusive: bool = RUNNERS.forall(a => RUNNERS.forall(b => a == b or match state.runners.get(a).ph {
    | RHolding(x) => match state.runners.get(b).ph {
      | RHolding(y) => x.jk != y.jk or x.seq != y.seq
      | _ => true
    }
    | _ => true
  }))

  val safety: bool = all {
    noStranded,
    ordering,
    checksEarned,
    oneActivePerGroup,
    cancelledIsIdle,
    gatesHold,
    oneRunPerKey,
    acceptedWasHolder,
    leaseExclusive,
  }
}
