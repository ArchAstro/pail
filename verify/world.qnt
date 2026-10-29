// -*- mode: Bluespec; -*-
/// The state every feature acts on: the bucket, each actor's progress, and ghosts that
/// only the properties read. Features (`publish.qnt`, `combine.qnt`, `rewrite.qnt`,
/// `gc.qnt`) are pure steps over it;
/// `pail.qnt` makes them actions.
module world {
  import store.* from "./store"
  import bucket.* from "./bucket"

  // ---------- writers (`Repo::commit`) ----------

  /// Where a writer is: the direct path (`Leasing` .. `Rechecking`), then, having lost it,
  /// combining (`Intending` .. `Withdrawing`, `Repo::commit_combining`). A client
  /// rewriting the branch instead (`rewrite.qnt`) leases as a writer does, then goes
  /// `RwStoring` .. `RwRechecking`; one landing a stack (`land.qnt`) first makes it
  /// (`Stacked`), then leases and goes `LdReading` .. `LdRechecking`.
  type Phase =
    | Idle
    | Leasing
    | CheckingLeases
    | Reading
    | Uploading
    | Swapping
    | Rechecking
    | Intending
    | Polling
    | Deciding
    | BatchSwapping
    | BatchRechecking
    | Cleaning
    | Withdrawing
    | RwStoring
    | RwReading
    | RwSwapping
    | RwRechecking
    | Stacked
    | LdReading
    | LdSwapping
    | LdRechecking
    | Done

  /// What `commit` returned: nothing yet, Ok, a ConflictError, or another error (its
  /// outcome unknown to the caller). What `rewrite_ref` returned: Ok (`Rewritten`), a
  /// MovedError (`Moved`), a ConflictError (a commit it carried does not apply), or
  /// another error (`Unknown`).
  type Outcome = Pending | Published | Conflicted | Unknown | Rewritten | Moved

  /// A rewrite in progress: what it is (its record's `op`: `rebase` carries out a `plan`
  /// on the tip's commits, `undo` reverses the ref's last record, `reset` drops the tip),
  /// the tip it was made from, the commit replacing it, the successors it starts from, the
  /// commits it drops and the ones it makes (stored before the first attempt) and, for an
  /// undo, the lineage that named its record (another rewrite since fails it); then, as its last
  /// attempt read the ref, the version its swap expects, the tip it moves to (with any
  /// commits landed since carried onto it) and the record it wrote.
  type Rw = {
    op: str,
    plan: str,
    expected: CommitId,
    new: CommitId,
    successors: CommitId -> CommitId,
    dropped: Set[CommitId],
    made: Set[CommitId],
    since: Lineage,
    version: int,
    tip: CommitId,
    id: int,
  }

  /// A combining writer's round: what its poll read (the ref and the intents listed, in
  /// either order), the batch it built, and what it returns once its intent is withdrawn.
  type Comb = {
    polledRef: bool,
    head: Ref,
    recent: Recent,
    polledList: bool,
    listed: Set[str],
    batch: Set[Entry],
    // The batch's last commit: what its swap moves the ref to.
    tip: CommitId,
    then: Outcome,
    // The lineage in the ref as polled.
    lineage: Lineage,
  }

  /// A head a published commit leaves its caller (`pail commit` keeps it in the working
  /// copy for the next commit's `KnownHead`): the ref's version, `recent` and lineage as
  /// the commit's publish left them, kept only when the commit is the ref's tip then.
  type Known = { has: bool, head: Ref, recent: Recent, lineage: Lineage }

  /// A landing in progress: how many land calls the client has begun, what its last
  /// attempt read of the branch (version, lineage) and the tip its swap moves it to.
  type Ld = { calls: int, tip: CommitId, version: int, lineage: Lineage }

  type Client = {
    base: CommitId,
    delta: Delta,
    phase: Phase,
    // The ref as this client last read it; version 0 until it reads.
    seen: Ref,
    // The landed intents in that read, which a direct swap writes back unchanged.
    seenRecent: Recent,
    // The lineage in that read.
    seenLineage: Lineage,
    // Which direct attempt (`1..=DIRECT_ATTEMPTS`) the client is on.
    attempt: int,
    comb: Comb,
    rw: Rw,
    ld: Ld,
    outcome: Outcome,
    // Where its read commit is in the branch's history at lineage `foundAt`, once it asked
    // (`locate.qnt`).
    located: bool,
    found: CommitId,
    foundAt: Lineage,
    // The head its last publish left it (`Known`), if any.
    known: Known,
  }

  // ---------- the collector (`pail gc`) ----------

  /// Idle; holding its exclusive lease, waiting for writers' leases to go; holding it alone;
  /// marked what the refs reach; deleting unreachable old commits, then pages.
  type GcPhase =
    | GcIdle
    | GcDraining
    | GcLocked
    | GcMarked
    | GcSweepingCommits
    | GcSweepingPages

  type Collector = {
    phase: GcPhase,
    // What the refs reached when it marked.
    liveCommits: Set[CommitId],
    livePages: Set[Tree],
    // Listed as unreachable and old, not yet deleted.
    doomedCommits: Set[CommitId],
    doomedPages: Set[Tree],
    // Young when it took `now` (at its first listing): its cutoff for every listing.
    spared: Young,
  }

  // ---------- time ----------

  /// What a lease holder (a client, or gc as `GC`) knows of its lease's age, by its own
  /// clock, and the refresher that keeps it alive (`Lease`'s `refreshed`, its background
  /// task). Times are ticks (`bucket.qnt`); .
  ///
  /// ```text
  ///   held ──tick: REFRESH since `done`──▶ sent ──refreshLand──▶ landed ──refreshAck──▶ stamped
  ///                                          │ Lost / Dropped                        (refreshed = sent)
  ///                                          └──▶ done = now, nothing stamped
  /// ```
  type Timing = {
    // Holding a lease: its refresher runs.
    running: bool,
    // The start of the latest refresh the holder knows landed (or of the lease's first
    // write): what `Lease::check` measures from.
    refreshed: int,
    // When its refresher last finished a request: the next goes REFRESH later.
    done: int,
    // A refresh is in flight (sent, its request not yet answered), and when it was sent
    // (0 when none is).
    inflight: bool,
    sent: int,
    // That refresh reached the store (LastModified is then its landing time), its reply
    // not yet back.
    landed: bool,
    // When the last check that allowed a swap or delete passed.
    checked: int,
  }

  type State = {
    bucket: Bucket,
    clients: str -> Client,
    collector: Collector,
    // The time, in ticks.
    now: int,
    timing: str -> Timing,
    // Ghost: every commit the ref has named, oldest first. Not stored by pail.
    history: List[CommitId],
  }

  pure val CLIENTS: Set[str] = Set("alice", "bob", "carol", "dave")
  /// The collector's lease holder name.
  pure val GC: str = "gc"
  /// `DIRECT_ATTEMPTS` in repo.rs.
  pure val DIRECT_ATTEMPTS: int = 2
  /// Breaks ties between intents created at the same time (their ids' random suffixes).
  pure val RANK: str -> int = Map("alice" -> 0, "bob" -> 1, "carol" -> 2, "dave" -> 3)
  /// Names the commit a combining round builds for a writer's intent: not the writer's own
  /// commit on the same parent, since it records the intent (`pail.intent`).
  pure val INTENT_NAME: str -> str =
    Map("alice" -> "alice+", "bob" -> "bob+", "carol" -> "carol+", "dave" -> "dave+")

  /// Marks a commit a client's rewrite made (rewording one, or carrying one onto the new
  /// tip): it follows the name of the commit's writer or intent, so `author` still finds it.
  pure val MARK: str -> str =
    Map("alice" -> "~alice", "bob" -> "~bob", "carol" -> "~carol", "dave" -> "~dave")

  pure val NO_RW: Rw = {
    op: "",
    plan: "",
    expected: ROOT,
    new: ROOT,
    successors: Map(),
    dropped: Set(),
    made: Set(),
    since: NO_LINEAGE,
    version: 0,
    tip: ROOT,
    id: 0,
  }

  pure val NO_LD: Ld = { calls: 0, tip: ROOT, version: 0, lineage: NO_LINEAGE }

  pure val NO_COMB: Comb = {
    polledRef: false,
    head: { commit: ROOT, version: 0 },
    recent: Map(),
    polledList: false,
    listed: Set(),
    batch: Set(),
    tip: ROOT,
    then: Pending,
    lineage: NO_LINEAGE,
  }

  pure val NO_KNOWN: Known = { has: false, head: { commit: ROOT, version: 0 }, recent: Map(), lineage: NO_LINEAGE }

  /// What a publish of commit `id` leaves its caller when the ref, as the publish last
  /// had it, is at `at` with `recent` and `lineage`: a known head only if `id` is the tip
  /// (`Worktree::commit`: a version belongs to the ref's commit, not to one under it).
  pure def knownOf(id: CommitId, at: Ref, recent: Recent, lineage: Lineage): Known =
    if (id == at.commit) { has: true, head: at, recent: recent, lineage: lineage } else NO_KNOWN

  pure val IDLE: Client = {
    base: ROOT,
    delta: Map(),
    phase: Idle,
    seen: { commit: ROOT, version: 0 },
    seenRecent: Map(),
    seenLineage: NO_LINEAGE,
    attempt: 1,
    comb: NO_COMB,
    rw: NO_RW,
    ld: NO_LD,
    outcome: Pending,
    located: false,
    found: ROOT,
    foundAt: NO_LINEAGE,
    known: NO_KNOWN,
  }

  pure val GC_IDLE: Collector = {
    phase: GcIdle,
    liveCommits: Set(),
    livePages: Set(),
    doomedCommits: Set(),
    doomedPages: Set(),
    spared: { pages: Set(), commits: Set() },
  }

  pure val NO_TIMING: Timing = { running: false, refreshed: 0, done: 0, inflight: false, sent: 0, landed: false, checked: 0 }

  pure val INITIAL: State = {
    bucket: ROOT_BUCKET,
    clients: CLIENTS.mapBy(c => IDLE),
    collector: GC_IDLE,
    now: 0,
    timing: CLIENTS.union(Set(GC)).mapBy(h => NO_TIMING),
    history: [ROOT],
  }

  /// The commit `c` builds on what it read.
  pure def made(c: str, cl: Client): CommitId = cl.seen.commit.append(c)

  pure def client(s: State, c: str): Client = s.clients.get(c)

  /// `c` moves on to `cl`. Arriving at `Leasing`, it takes the instant before its put
  /// (`Lease::write`), which its lease will count from.
  pure def withClient(s: State, c: str, cl: Client): State =
    {
      ...s,
      clients: s.clients.set(c, cl),
      timing:
        if (cl.phase == Leasing and client(s, c).phase != Leasing)
          s.timing.set(c, { ...s.timing.get(c), sent: s.now })
        else s.timing,
    }

  /// `c` moves on to `cl`. If that is a return, it releases its lease first.
  pure def finished(s: State, c: str, cl: Client): State =
    {
      ...s,
      bucket: if (cl.phase == Done) unleased(s.bucket, c) else s.bucket,
      clients: s.clients.set(c, cl),
      timing: if (cl.phase == Done) s.timing.set(c, NO_TIMING) else s.timing,
    }

  // ---------- leases held, checked ----------

  /// `h` has listed the leases and found none in its way (`Lease::held`): the lease is its,
  /// its refresher starts, and its age is counted from the start of the put that wrote it,
  /// taken before the put (`Lease::write`) and so no later than its LastModified. (gc starts
  /// with its put: the instant is its LastModified.)
  pure def holding(s: State, h: str): State =
    {
      ...s,
      timing: s.timing.set(h, {
        ...NO_TIMING,
        running: true,
        refreshed: if (h == GC) leasedAt(s.bucket, h) else s.timing.get(h).sent,
        done: s.now,
      }),
    }

  /// `h`'s lease is gone (released, or its holder died): its refresher stops.
  pure def notHolding(s: State, h: str): State = { ...s, timing: s.timing.set(h, NO_TIMING) }

  /// `Lease::check`: `h`'s lease was refreshed within `MAX_AGE` of now.
  pure def fresh(s: State, h: str): bool = s.now - s.timing.get(h).refreshed <= MAX_AGE

  /// `h` is about to send a request guarded by `Lease::check`: if the check passes, the
  /// request is on its way, and may take up to `LATENCY` ticks to land (`tickCan`); if it
  /// fails, `refused` is where `h` goes: as when the request never reached the store.
  pure def guarded(s: State, h: str, refused: State): State =
    if (fresh(s, h)) { ...s, timing: s.timing.set(h, { ...s.timing.get(h), checked: s.now }) }
    else refused

  /// A client waiting at a ref swap, which a passed check let go ahead.
  pure def swapping(cl: Client): bool =
    Set(Swapping, BatchSwapping, RwSwapping, LdSwapping).contains(cl.phase)

  /// gc about to delete an object a passed check let it delete.
  pure def deleting(col: Collector): bool =
    or {
      col.phase == GcSweepingCommits and col.doomedCommits != Set(),
      col.phase == GcSweepingPages and col.doomedPages != Set(),
    }

  /// The name of the writer or intent whose change commit `id` makes (a rewrite's mark
  /// after it skipped).
  pure def tagOf(id: CommitId): str =
    if (MARK.keys().exists(c => MARK.get(c) == id[length(id) - 1])) id[length(id) - 2]
    else id[length(id) - 1]

  /// The tags of the commits melded into `id` (a squash or fixup), oldest first: the
  /// `[t, "&"]` pairs just before its own tag, as in `[.., t, "&", lead, mark]` (at most
  /// four; one per writer).
  pure def meldedTags(id: CommitId): List[str] = {
    val at = if (MARK.keys().exists(c => MARK.get(c) == id[length(id) - 1])) length(id) - 2 else length(id) - 1
    val a1 = at >= 2 and id[at - 1] == "&"
    val a2 = a1 and at >= 4 and id[at - 3] == "&"
    val a3 = a2 and at >= 6 and id[at - 5] == "&"
    val a4 = a3 and at >= 8 and id[at - 7] == "&"
    val l4 = if (a4) List(id[at - 8]) else List()
    val l3 = if (a3) l4.append(id[at - 6]) else l4
    val l2 = if (a2) l3.append(id[at - 4]) else l3
    if (a1) l2.append(id[at - 2]) else l2
  }

  /// `onto` with `tags` appended, each followed by `"&"`.
  pure def withMelded(onto: CommitId, tags: List[str]): CommitId = {
    val o1 = if (length(tags) > 0) onto.append(tags[0]).append("&") else onto
    val o2 = if (length(tags) > 1) o1.append(tags[1]).append("&") else o1
    val o3 = if (length(tags) > 2) o2.append(tags[2]).append("&") else o2
    if (length(tags) > 3) o3.append(tags[3]).append("&") else o3
  }

  /// The commit a rewrite by `c` makes of `id`'s change on top of `onto` (keeping what was
  /// melded into it).
  pure def remade(c: str, id: CommitId, onto: CommitId): CommitId =
    withMelded(onto, meldedTags(id)).append(tagOf(id)).append(MARK.get(c))

  /// The commit a rewrite by `c` makes on `onto` of `lead` with `member` melded into it
  /// (a squash or fixup group of two).
  pure def meldedId(c: str, lead: CommitId, member: CommitId, onto: CommitId): CommitId =
    withMelded(withMelded(onto, meldedTags(member)).append(tagOf(member)).append("&"), meldedTags(lead))
      .append(tagOf(lead))
      .append(MARK.get(c))

  /// The writer a tag names: itself, or the writer of the intent it names.
  pure def writerOf(tag: str): str =
    CLIENTS.fold(tag, (a, c) => if (INTENT_NAME.get(c) == tag) c else a)

  /// Who wrote commit `id`'s change: its writer, directly or through its intent, however
  /// often it was rewritten since.
  pure def author(id: CommitId): str = writerOf(tagOf(id))

  /// Whose changes commit `id` makes: its author and those of the commits melded into it.
  pure def authorsOf(id: CommitId): Set[str] =
    Set(author(id)).union(meldedTags(id).indices().map(i => writerOf(meldedTags(id)[i])))

  /// The writers whose changes the history at `id` has.
  pure def authorsAt(b: Bucket, id: CommitId): Set[str] =
    ancestorsOf(b, id).union(Set(id)).exclude(Set(ROOT)).fold(Set(), (acc, x) => acc.union(authorsOf(x)))

  /// The writers whose changes record `r` took off the branch.
  pure def droppedBy(b: Bucket, r: Rewrite): Set[str] =
    authorsAt(b, r.old).exclude(authorsAt(b, r.new))
}
