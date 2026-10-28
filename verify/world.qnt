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
  }

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

  type State = {
    bucket: Bucket,
    clients: str -> Client,
    collector: Collector,
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
  }

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
  }

  pure val GC_IDLE: Collector = {
    phase: GcIdle,
    liveCommits: Set(),
    livePages: Set(),
    doomedCommits: Set(),
    doomedPages: Set(),
    spared: { pages: Set(), commits: Set() },
  }

  pure val INITIAL: State = {
    bucket: ROOT_BUCKET,
    clients: CLIENTS.mapBy(c => IDLE),
    collector: GC_IDLE,
    history: [ROOT],
  }

  /// The commit `c` builds on what it read.
  pure def made(c: str, cl: Client): CommitId = cl.seen.commit.append(c)

  pure def client(s: State, c: str): Client = s.clients.get(c)

  pure def withClient(s: State, c: str, cl: Client): State =
    { ...s, clients: s.clients.set(c, cl) }

  /// `c` moves on to `cl`. If that is a return, it releases its lease first.
  pure def finished(s: State, c: str, cl: Client): State =
    {
      ...s,
      bucket: if (cl.phase == Done) unleased(s.bucket, c) else s.bucket,
      clients: s.clients.set(c, cl),
    }

  /// The name of the writer or intent whose change commit `id` makes (a rewrite's mark
  /// after it skipped).
  pure def tagOf(id: CommitId): str =
    if (MARK.keys().exists(c => MARK.get(c) == id[length(id) - 1])) id[length(id) - 2]
    else id[length(id) - 1]

  /// The commit a rewrite by `c` makes of `id`'s change on top of `onto`.
  pure def remade(c: str, id: CommitId, onto: CommitId): CommitId =
    onto.append(tagOf(id)).append(MARK.get(c))

  /// Who wrote commit `id`'s change: its writer, directly or through its intent, however
  /// often it was rewritten since.
  pure def author(id: CommitId): str = {
    val last = tagOf(id)
    CLIENTS.fold(last, (a, c) => if (INTENT_NAME.get(c) == last) c else a)
  }

  /// The writers whose changes the history at `id` has.
  pure def authorsAt(b: Bucket, id: CommitId): Set[str] =
    ancestorsOf(b, id).union(Set(id)).exclude(Set(ROOT)).map(x => author(x))

  /// The writers whose changes record `r` took off the branch.
  pure def droppedBy(b: Bucket, r: Rewrite): Set[str] =
    authorsAt(b, r.old).exclude(authorsAt(b, r.new))
}
