// -*- mode: Bluespec; -*-
/// Publishing a commit: `Repo::commit` racing other writers on one branch, over a store
/// whose requests can fail in transit.
///
/// Each action is one store request a client makes, so any interleaving of clients is a
/// behaviour of the model. A client's steps follow `commit_opts`:
///
/// ```text
///   begin ─▶ Reading ─readRef─▶ Uploading ─upload─▶ Swapping ─swap(Answered)─▶ Done
///               ▲  │ overlap: Conflicted               │  │ lost the race: retry
///               │  ▼                                   │  ▼
///               │ Done                 swap(Lost|Dropped) Reading (or Combining)
///               └────────── recheck ◀── Rechecking ◀───┘
/// ```
///
/// Out of scope here: combining under contention (a client that loses every direct attempt
/// stops as `Combining`), rewrites, hooks and GC.
module publish {
  import store.* from "./store"
  import bucket.* from "./bucket"

  type Phase = Idle | Reading | Uploading | Swapping | Rechecking | Done

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

  type State = {
    bucket: Bucket,
    clients: str -> Client,
    // Ghost: every commit the ref has named, oldest first. Not stored by pail.
    history: List[CommitId],
  }

  pure val CLIENTS: Set[str] = Set("alice", "bob")
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

  /// The commit `c` builds on what it read. Its hash would be the same on every attempt
  /// that read the same head.
  pure def made(c: str, cl: Client): CommitId = { by: c, gen: cl.seen.commit.gen + 1 }

  var state: State

  action init = all {
    state' = {
      bucket: ROOT_BUCKET,
      clients: CLIENTS.mapBy(c => IDLE),
      history: [ROOT],
    },
  }

  /// `commit(base, delta)` is called; its first request will read the ref.
  action begin(c: str, base: CommitId, delta: Delta): bool = all {
    state.clients.get(c).phase == Idle,
    isLive(state.bucket.commits, base),
    state' = {
      ...state,
      clients: state.clients.set(c, {
        ...IDLE,
        base: base,
        delta: delta,
        phase: Reading,
        seen: { commit: base, version: 0 },
      }),
    },
  }

  /// Read the ref. A head past the base replays the delta onto it, unless a path the delta
  /// writes changed in between (`check_disjoint`).
  action readRef(c: str): bool = all {
    state.clients.get(c).phase == Reading,
    state' = {
      ...state,
      clients: state.clients.set(c,
        if (overlaps(
          state.clients.get(c).delta,
          treeOf(state.bucket, state.clients.get(c).base),
          treeOf(state.bucket, tip(state.bucket).commit),
        ))
          { ...state.clients.get(c), phase: Done, outcome: Conflicted, seen: tip(state.bucket) }
        else
          { ...state.clients.get(c), phase: Uploading, seen: tip(state.bucket) }
      ),
    },
  }

  /// Store the commit object (and its pages) before any ref can name it.
  action upload(c: str): bool = all {
    state.clients.get(c).phase == Uploading,
    state' = {
      ...state,
      bucket: uploaded(
        state.bucket,
        made(c, state.clients.get(c)),
        state.clients.get(c).seen.commit,
        state.clients.get(c).delta,
      ),
      clients: state.clients.set(c, { ...state.clients.get(c), phase: Swapping }),
    },
  }

  /// CAS the ref from the version read to the new commit. Answered: a lost race retries
  /// from the read, and after the last direct attempt hands off to combining. Unanswered
  /// (`Lost`: performed if it applied; `Dropped`: never received): recheck.
  action swap(c: str, d: Delivery): bool = all {
    state.clients.get(c).phase == Swapping,
    state' = {
      bucket:
        if (received(d))
          refSwapped(state.bucket, state.clients.get(c).seen.version, made(c, state.clients.get(c)))
        else state.bucket,
      clients: state.clients.set(c,
        if (not(answered(d)))
          { ...state.clients.get(c), phase: Rechecking }
        else if (refSwapApplies(state.bucket, state.clients.get(c).seen.version))
          { ...state.clients.get(c), phase: Done, outcome: Published }
        else if (state.clients.get(c).attempt < DIRECT_ATTEMPTS)
          { ...state.clients.get(c), phase: Reading, attempt: state.clients.get(c).attempt + 1 }
        else
          { ...state.clients.get(c), phase: Done, outcome: Combining }
      ),
      history:
        if (received(d) and refSwapApplies(state.bucket, state.clients.get(c).seen.version))
          state.history.append(made(c, state.clients.get(c)))
        else state.history,
    },
  }

  /// After an unanswered swap, read the ref to learn whether it won: the commit is on the
  /// branch, as its head or under commits others landed since.
  action recheck(c: str): bool = all {
    state.clients.get(c).phase == Rechecking,
    state' = {
      ...state,
      clients: state.clients.set(c,
        if (onBranch(state.bucket, made(c, state.clients.get(c))))
          { ...state.clients.get(c), phase: Done, outcome: Published }
        else if (state.clients.get(c).attempt < DIRECT_ATTEMPTS)
          { ...state.clients.get(c), phase: Reading, attempt: state.clients.get(c).attempt + 1 }
        else
          { ...state.clients.get(c), phase: Done, outcome: Unknown }
      ),
    },
  }

  /// Deltas a simulated client may write. None writes a path's root content, so a landed
  /// commit always changes what it writes.
  pure val DELTAS: Set[Delta] = Set(
    Map("a" -> "1"),
    Map("b" -> "1"),
    Map("a" -> "2", "b" -> "2"),
  )

  action step = {
    nondet c = CLIENTS.oneOf()
    nondet base = liveKeys(state.bucket.commits).oneOf()
    nondet delta = DELTAS.oneOf()
    nondet d = Set(Answered, Lost, Dropped).oneOf()
    any {
      begin(c, base, delta),
      readRef(c),
      upload(c),
      swap(c, d),
      recheck(c),
    }
  }

  // Properties. Each holds in every reachable state.

  /// The ref never names a commit that is not stored.
  val refNamesStoredCommit: bool = isLive(state.bucket.commits, tip(state.bucket).commit)

  /// Every move of the ref keeps what it named before: nothing published is overwritten.
  val refOnlyMovesForward: bool =
    state.history.indices().forall(i =>
      i == 0 or ancestorsOf(state.bucket, state.history[i]).contains(state.history[i - 1])
    )

  /// A client told its commit was published finds it on the branch, now and later.
  val publishedIsOnBranch: bool =
    CLIENTS.forall(c =>
      state.clients.get(c).outcome == Published
        implies onBranch(state.bucket, made(c, state.clients.get(c)))
    )

  /// A client told it conflicted has nothing on the branch: it can retry without
  /// publishing its change twice.
  val conflictedHasNothingOnBranch: bool =
    CLIENTS.forall(c =>
      state.clients.get(c).outcome == Conflicted
        implies branch(state.bucket).forall(id => id.by != c)
    )

  /// One call to `commit` lands at most one commit.
  val landsAtMostOnce: bool =
    CLIENTS.forall(c => branch(state.bucket).filter(id => id.by == c).size() <= 1)

  val safety: bool = and {
    refNamesStoredCommit,
    refOnlyMovesForward,
    publishedIsOnBranch,
    conflictedHasNothingOnBranch,
    landsAtMostOnce,
  }
}
