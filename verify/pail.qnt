// -*- mode: Bluespec; -*-
/// Pail as a whole: every feature's steps (`publish.qnt`, `combine.qnt`, `rewrite.qnt`,
/// `gc.qnt`) as actions on one
/// state, a `step` that interleaves them freely, and the properties that must hold in
/// every reachable state. `npm run simulate` checks them; `scenarios.qnt` holds runs the
/// real code is checked against.
module pail {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"
  import publish.* from "./publish"
  import combine.* from "./combine"
  import rewrite.* from "./rewrite"
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

  // ---------- combining ----------

  action intend(c: str, at: int): bool = all { intendCan(state, c), state' = intendNext(state, c, at) }

  action pollRef(c: str): bool = all { pollRefCan(state, c), state' = pollRefNext(state, c) }

  action pollList(c: str): bool = all { pollListCan(state, c), state' = pollListNext(state, c) }

  action wait(c: str): bool = all { waitCan(state, c), state' = waitNext(state, c) }

  action giveUp(c: str): bool = all { giveUpCan(state, c), state' = giveUpNext(state, c) }

  action gather(c: str, vanished: Set[str]): bool = all {
    gatherCan(state, c, vanished),
    state' = gatherNext(state, c, vanished),
  }

  action batchSwap(c: str, d: Delivery): bool = all {
    batchSwapCan(state, c),
    state' = batchSwapNext(state, c, d),
  }

  action cleanup(c: str): bool = all { cleanupCan(state, c), state' = cleanupNext(state, c) }

  action batchRecheck(c: str): bool = all {
    batchRecheckCan(state, c),
    state' = batchRecheckNext(state, c),
  }

  action withdraw(c: str): bool = all { withdrawCan(state, c), state' = withdrawNext(state, c) }

  // ---------- rewriting ----------

  action beginRewrite(c: str): bool = all {
    beginRewriteCan(state, c),
    state' = beginRewriteNext(state, c),
  }

  action rwStore(c: str): bool = all { rwStoreCan(state, c), state' = rwStoreNext(state, c) }

  action rwRead(c: str): bool = all { rwReadCan(state, c), state' = rwReadNext(state, c) }

  action rwSwap(c: str, d: Delivery): bool = all {
    rwSwapCan(state, c),
    state' = rwSwapNext(state, c, d),
  }

  action rwRecheck(c: str): bool = all {
    rwRecheckCan(state, c),
    state' = rwRecheckNext(state, c),
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
    nondet at = Set(1, 2, 3).oneOf()
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
      intend(c, at),
      pollRef(c),
      pollList(c),
      wait(c),
      giveUp(c),
      nondet vanished = vanishable(state, c).powerset().oneOf()
      gather(c, vanished),
      batchSwap(c, d),
      cleanup(c),
      batchRecheck(c),
      withdraw(c),
      beginRewrite(c),
      rwStore(c),
      rwRead(c),
      rwSwap(c, d),
      rwRecheck(c),
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

  /// Writers and rewriters only, nothing crashing or giving up: random runs of `step`
  /// rarely get a writer through two lost races into combining, so `npm run simulate`
  /// runs this too.
  action contendStep = {
    nondet c = CLIENTS.oneOf()
    nondet base = liveKeys(state.bucket.commits).oneOf()
    nondet delta = DELTAS.oneOf()
    nondet d = Set(Answered, Answered, Lost, Dropped).oneOf()
    nondet at = Set(1, 2, 3).oneOf()
    any {
      begin(c, base, delta),
      lease(c),
      checkLeases(c),
      readRef(c),
      upload(c),
      swap(c, d),
      recheck(c),
      intend(c, at),
      pollRef(c),
      pollList(c),
      wait(c),
      nondet vanished = vanishable(state, c).powerset().oneOf()
      gather(c, vanished),
      batchSwap(c, d),
      cleanup(c),
      batchRecheck(c),
      withdraw(c),
      beginRewrite(c),
      rwStore(c),
      rwRead(c),
      rwSwap(c, d),
      rwRecheck(c),
    }
  }

  // ---------- properties ----------

  /// Every commit the branch reaches, and its page, is stored: no ref dangles.
  val branchIsComplete: bool = branchIsStored(state.bucket)

  /// Every move of the ref keeps what it named before, unless a rewrite's record names
  /// the move: nothing published is overwritten unaccounted for.
  val refOnlyMovesForward: bool =
    state.history.indices().forall(i => or {
      i == 0,
      ancestorsOf(state.bucket, state.history[i]).contains(state.history[i - 1]),
      state.bucket.reflog.keys().exists(id => and {
        recordOf(state.bucket, id).old == state.history[i - 1],
        recordOf(state.bucket, id).new == state.history[i],
      }),
    })

  /// A client told its commit was published finds it on the branch, now and later: its
  /// own commit, or the one a combining round made of its intent.
  val publishedIsOnBranch: bool =
    CLIENTS.forall(c =>
      client(state, c).outcome == Published
        implies branch(state.bucket).exists(id => author(id) == c)
    )

  /// The ref's `recent` names, for each landed intent, a commit made of it, and the
  /// branch has it (that commit, or what a rewrite made of it).
  val recentIsTruthful: bool =
    recentOf(state.bucket).keys().forall(w => and {
      author(recentOf(state.bucket).get(w)) == w,
      branch(state.bucket).exists(id => author(id) == w),
    })

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

  /// The ref's lineage names the last rewrite: its record, of the same epoch, whose new
  /// tip the branch still has. No rewrite yet: epoch 0.
  val lineageNamesItsRecord: bool = {
    val l = lineageOf(state.bucket)
    if (l.rewrite == 0) l.epoch == 0
    else and {
      isLive(state.bucket.reflog, l.rewrite),
      recordOf(state.bucket, l.rewrite).epoch == l.epoch,
      onBranch(state.bucket, recordOf(state.bucket, l.rewrite).new),
    }
  }

  /// Each record's epoch is one past the record it names: epochs count rewrites.
  val recordsAreChained: bool =
    state.bucket.reflog.keys().forall(id =>
      if (recordOf(state.bucket, id).prev == 0) recordOf(state.bucket, id).epoch == 1
      else recordOf(state.bucket, recordOf(state.bucket, id).prev).epoch + 1 == recordOf(state.bucket, id).epoch
    )

  /// Every commit a record reaches stays stored, with its page: gc keeps rewritten
  /// history, so successors can be walked and a rewrite undone.
  val reflogIsStored: bool = allStored(state.bucket, reflogReaches(state.bucket))

  /// A client told its rewrite landed finds its record in the ref's chain; one told the
  /// branch moved, so nothing was rewritten, finds none.
  val rewriteOutcomeIsTruthful: bool =
    CLIENTS.forall(c => and {
      client(state, c).outcome == Rewritten
        implies chain(state.bucket).exists(id => recordOf(state.bucket, id).by == c),
      client(state, c).outcome == Moved
        implies chain(state.bucket).forall(id => recordOf(state.bucket, id).by != c),
    })

  /// While gc marks and deletes, no writer is between its first read and its return.
  val gcRunsAlone: bool =
    Set(GcLocked, GcMarked, GcSweepingCommits, GcSweepingPages).contains(state.collector.phase)
      implies CLIENTS.forall(c => Set(Idle, Leasing, CheckingLeases, Done).contains(client(state, c).phase))

  val safety: bool = and {
    gcRunsAlone,
    branchIsComplete,
    refOnlyMovesForward,
    publishedIsOnBranch,
    recentIsTruthful,
    conflictedHasNothingOnBranch,
    landsAtMostOnce,
    lineageNamesItsRecord,
    recordsAreChained,
    reflogIsStored,
    rewriteOutcomeIsTruthful,
  }
}
