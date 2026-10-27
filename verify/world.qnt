// -*- mode: Bluespec; -*-
/// The state every feature acts on: the bucket, each actor's progress, and ghosts that
/// only the properties read. Features (`publish.qnt`, `gc.qnt`) are pure steps over it;
/// `pail.qnt` makes them actions.
module world {
  import store.* from "./store"
  import bucket.* from "./bucket"

  // ---------- writers (`Repo::commit`) ----------

  /// Where a writer is: the direct path (`Leasing` .. `Rechecking`), then, having lost it,
  /// combining (`Intending` .. `Withdrawing`, `Repo::commit_combining`).
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
    | Done

  /// What `commit` returned: nothing yet, Ok, a ConflictError, or another error (its
  /// outcome unknown to the caller).
  type Outcome = Pending | Published | Conflicted | Unknown

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

  type Client = {
    base: CommitId,
    delta: Delta,
    phase: Phase,
    // The ref as this client last read it; version 0 until it reads.
    seen: Ref,
    // The landed intents in that read, which a direct swap writes back unchanged.
    seenRecent: Recent,
    // Which direct attempt (`1..=DIRECT_ATTEMPTS`) the client is on.
    attempt: int,
    comb: Comb,
    outcome: Outcome,
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
    attempt: 1,
    comb: NO_COMB,
    outcome: Pending,
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

  /// Who wrote commit `id`'s last step: its writer, directly or through its intent.
  pure def author(id: CommitId): str = {
    val last = id[length(id) - 1]
    CLIENTS.fold(last, (a, c) => if (INTENT_NAME.get(c) == last) c else a)
  }
}
