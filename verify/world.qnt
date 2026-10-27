// -*- mode: Bluespec; -*-
/// The state every feature acts on: the bucket, each actor's progress, and ghosts that
/// only the properties read. Features (`publish.qnt`, `gc.qnt`) are pure steps over it;
/// `pail.qnt` makes them actions.
module world {
  import store.* from "./store"
  import bucket.* from "./bucket"

  // ---------- writers (`Repo::commit`) ----------

  type Phase = Idle | Leasing | CheckingLeases | Reading | Uploading | Swapping | Rechecking | Done

  /// What `commit` returned: nothing yet, Ok, a ConflictError, another error (its outcome
  /// unknown to the caller), or a hand-off to the combining path.
  type Outcome = Pending | Published | Conflicted | Unknown | Combining

  type Client = {
    base: CommitId,
    delta: Delta,
    phase: Phase,
    // The ref as this client last read it; version 0 until it reads.
    seen: Ref,
    // Which direct attempt (`1..=DIRECT_ATTEMPTS`) the client is on.
    attempt: int,
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

  pure val CLIENTS: Set[str] = Set("alice", "bob", "carol")
  /// The collector's lease holder name.
  pure val GC: str = "gc"
  /// `DIRECT_ATTEMPTS` in repo.rs.
  pure val DIRECT_ATTEMPTS: int = 2

  pure val IDLE: Client = {
    base: ROOT,
    delta: Map(),
    phase: Idle,
    seen: { commit: ROOT, version: 0 },
    attempt: 1,
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

  /// `c` moves on to `cl`. If that is a return, it releases its lease first, except when
  /// it hands off to combining, which goes on writing under it.
  pure def finished(s: State, c: str, cl: Client): State =
    {
      ...s,
      bucket:
        if (cl.phase == Done and cl.outcome != Combining) unleased(s.bucket, c)
        else s.bucket,
      clients: s.clients.set(c, cl),
    }
}
