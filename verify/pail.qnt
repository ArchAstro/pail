// -*- mode: Bluespec; -*-
/// Pail as a whole: every feature's steps (`publish.qnt`, `gc.qnt`) as actions on one
/// state, a `step` that interleaves them freely, and the properties that must hold in
/// every reachable state. `npm run simulate` checks them; `scenarios.qnt` holds runs the
/// real code is checked against.
module pail {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"
  import publish.* from "./publish"
  import gc.* from "./gc"

  var state: State

  action init = state' = INITIAL

  // ---------- writers ----------

  action begin(c: str, base: CommitId, delta: Delta): bool = all {
    beginCan(state, c, base),
    state' = beginNext(state, c, base, delta),
  }

  action lease(c: str): bool = all { leaseCan(state, c), state' = leaseNext(state, c) }

  action checkLeases(c: str): bool = all {
    checkLeasesCan(state, c),
    state' = checkLeasesNext(state, c),
  }

  action crash(c: str): bool = all { crashCan(state, c), state' = crashNext(state, c) }

  action leaseExpires(c: str): bool = all {
    leaseExpiresCan(state, c),
    state' = leaseExpiresNext(state, c),
  }

  action readRef(c: str): bool = all {
    readRefCan(state, c),
    state' = readRefNext(state, c),
  }

  action upload(c: str): bool = all {
    uploadCan(state, c),
    state' = uploadNext(state, c),
  }

  action swap(c: str, d: Delivery): bool = all {
    swapCan(state, c),
    state' = swapNext(state, c, d),
  }

  action recheck(c: str): bool = all {
    recheckCan(state, c),
    state' = recheckNext(state, c),
  }

  // ---------- the collector ----------

  action gcLease: bool = all { gcLeaseCan(state), state' = gcLeaseNext(state) }

  action gcCheckLeases: bool = all {
    gcCheckLeasesCan(state),
    state' = gcCheckLeasesNext(state),
  }

  action gcMark: bool = all { gcMarkCan(state), state' = gcMarkNext(state) }

  action gcListCommits: bool = all {
    gcListCommitsCan(state),
    state' = gcListCommitsNext(state),
  }

  action gcDeleteCommit(id: CommitId): bool = all {
    gcDeleteCommitCan(state, id),
    state' = gcDeleteCommitNext(state, id),
  }

  action gcListPages: bool = all { gcListPagesCan(state), state' = gcListPagesNext(state) }

  action gcDeletePage(t: Tree): bool = all {
    gcDeletePageCan(state, t),
    state' = gcDeletePageNext(state, t),
  }

  action gcDone: bool = all { gcDoneCan(state), state' = gcDoneNext(state) }

  action graceElapses: bool = state' = graceElapsesNext(state)

  // ---------- any interleaving ----------

  /// Deltas a simulated writer may write. None writes a path's root content, so a landed
  /// commit always changes what it writes; `a = 1` and `a = 2` let a later writer recreate
  /// a tree an earlier, lost commit had.
  pure val DELTAS: Set[Delta] = Set(
    Map("a" -> "1"),
    Map("a" -> "2"),
    Map("b" -> "1"),
  )

  action step = {
    nondet c = CLIENTS.oneOf()
    nondet base = liveKeys(state.bucket.commits).oneOf()
    nondet delta = DELTAS.oneOf()
    nondet d = Set(Answered, Lost, Dropped).oneOf()
    any {
      begin(c, base, delta),
      lease(c),
      checkLeases(c),
      crash(c),
      leaseExpires(c),
      readRef(c),
      upload(c),
      swap(c, d),
      recheck(c),
      gcLease,
      gcCheckLeases,
      gcMark,
      gcListCommits,
      gcListPages,
      gcDone,
      graceElapses,
      // Only enabled while sweeping, and then the doomed set is non-empty.
      nondet id = state.collector.doomedCommits.union(Set(ROOT)).oneOf()
      gcDeleteCommit(id),
      nondet t = state.collector.doomedPages.union(Set(ROOT_TREE)).oneOf()
      gcDeletePage(t),
    }
  }

  // ---------- properties ----------

  pure def author(id: CommitId): str = id[length(id) - 1]

  /// Every commit the branch reaches, and its page, is stored: no ref dangles.
  val branchIsComplete: bool = branchIsStored(state.bucket)

  /// Every move of the ref keeps what it named before: nothing published is overwritten.
  val refOnlyMovesForward: bool =
    state.history.indices().forall(i =>
      i == 0 or ancestorsOf(state.bucket, state.history[i]).contains(state.history[i - 1])
    )

  /// A client told its commit was published finds it on the branch, now and later.
  val publishedIsOnBranch: bool =
    CLIENTS.forall(c =>
      client(state, c).outcome == Published
        implies onBranch(state.bucket, made(c, client(state, c)))
    )

  /// A client told it conflicted has nothing on the branch: it can retry without
  /// publishing its change twice.
  val conflictedHasNothingOnBranch: bool =
    CLIENTS.forall(c =>
      client(state, c).outcome == Conflicted
        implies branch(state.bucket).forall(id => author(id) != c)
    )

  /// One call to `commit` lands at most one commit.
  val landsAtMostOnce: bool =
    CLIENTS.forall(c => branch(state.bucket).filter(id => author(id) == c).size() <= 1)

  /// While gc marks and deletes, no writer is between its first read and its return.
  val gcRunsAlone: bool =
    Set(GcLocked, GcMarked, GcSweepingCommits, GcSweepingPages).contains(state.collector.phase)
      implies CLIENTS.forall(c => and {
        not(Set(Reading, Uploading, Swapping, Rechecking).contains(client(state, c).phase)),
        not(client(state, c).outcome == Combining),
      })

  val safety: bool = and {
    gcRunsAlone,
    branchIsComplete,
    refOnlyMovesForward,
    publishedIsOnBranch,
    conflictedHasNothingOnBranch,
    landsAtMostOnce,
  }
}
