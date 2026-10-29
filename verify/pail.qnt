// -*- mode: Bluespec; -*-
/// Pail as a whole: every feature's steps (`publish.qnt`, `combine.qnt`, `rewrite.qnt`,
/// `land.qnt`, `locate.qnt`, `gc.qnt`) as actions on one
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
  import land.* from "./land"
  import locate.* from "./locate"
  import gc.* from "./gc"

  var state: State

  action init = state' = INITIAL

  // ---------- writers ----------

  action begin(c: str, base: CommitId, delta: Delta): bool = all {
    beginCan(state, c, base),
    state' = beginNext(state, c, base, delta),
  }

  action beginKnown(c: str, from: str, delta: Delta): bool = all {
    beginKnownCan(state, c, from),
    state' = beginKnownNext(state, c, from, delta),
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

  action beginRebase(c: str, plan: str): bool = all {
    beginRebaseCan(state, c, plan),
    state' = beginRebaseNext(state, c, plan),
  }

  action beginUndo(c: str): bool = all {
    beginUndoCan(state, c),
    state' = beginUndoNext(state, c),
  }

  action beginReset(c: str): bool = all {
    beginResetCan(state, c),
    state' = beginResetNext(state, c),
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

  // ---------- landing a stack ----------

  action stack(c: str, delta: Delta): bool = all { stackCan(state, c), state' = stackNext(state, c, delta) }

  action beginLand(c: str): bool = all { beginLandCan(state, c), state' = beginLandNext(state, c) }

  action ldRead(c: str): bool = all { ldReadCan(state, c), state' = ldReadNext(state, c) }

  action ldSwap(c: str, d: Delivery): bool = all { ldSwapCan(state, c), state' = ldSwapNext(state, c, d) }

  action ldRecheck(c: str): bool = all { ldRecheckCan(state, c), state' = ldRecheckNext(state, c) }

  // ---------- finding a commit after rewrites ----------

  action locate(c: str): bool = all { locateCan(state, c), state' = locateNext(state, c) }

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

  action gcDeletePage(t: Page): bool = all {
    gcDeletePageCan(state, t),
    state' = gcDeletePageNext(state, t),
  }

  action gcListPacks: bool = all { gcListPacksCan(state), state' = gcListPacksNext(state) }

  action gcDeletePack(k: PackId): bool = all {
    gcDeletePackCan(state, k),
    state' = gcDeletePackNext(state, k),
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
    Map("a" -> "p1"),
    Map("a" -> "p2"),
    Map("b" -> "p3"),
  )

  /// The deltas a repack of the commit `base` may write: some of its big files, each to
  /// the content it has (`relocation`), a nonempty set.
  def relocations(base: CommitId): Set[Delta] = {
    val bigs = PATHS.filter(p => isBig(treeOf(state.bucket, base).get(p)))
    bigs.powerset().exclude(Set(Set())).map(ps => ps.mapBy(p => treeOf(state.bucket, base).get(p)))
  }

  action step = {
    nondet c = CLIENTS.oneOf()
    nondet base = liveKeys(state.bucket.commits).oneOf()
    nondet delta = DELTAS.union(relocations(base)).oneOf()
    nondet d = Set(Answered, Lost, Dropped).oneOf()
    nondet at = Set(1, 2, 3).oneOf()
    nondet plan = PLANS.oneOf()
    any {
      begin(c, base, delta),
      nondet from = CLIENTS.oneOf()
      beginKnown(c, from, delta),
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
      beginRebase(c, plan),
      beginUndo(c),
      beginReset(c),
      rwStore(c),
      rwRead(c),
      rwSwap(c, d),
      rwRecheck(c),
      stack(c, delta),
      beginLand(c),
      ldRead(c),
      ldSwap(c, d),
      ldRecheck(c),
      locate(c),
      gcLease,
      gcCheckLeases,
      gcMark,
      gcListCommits,
      gcListPages,
      gcListPacks,
      gcDone,
      graceElapses,
      // Only enabled while sweeping, and then the doomed set is non-empty.
      nondet id = state.collector.doomedCommits.union(Set(ROOT)).oneOf()
      gcDeleteCommit(id),
      nondet t = state.collector.doomedPages.union(Set(ROOT_PAGE)).oneOf()
      gcDeletePage(t),
      nondet k = state.collector.doomedPacks.union(Set(ROOT)).oneOf()
      gcDeletePack(k),
    }
  }

  /// Writers and rewriters only, nothing crashing or giving up: random runs of `step`
  /// rarely get a writer through two lost races into combining, so `npm run simulate`
  /// runs this too.
  action contendStep = {
    nondet c = CLIENTS.oneOf()
    nondet base = liveKeys(state.bucket.commits).oneOf()
    nondet delta = DELTAS.union(relocations(base)).oneOf()
    nondet d = Set(Answered, Answered, Lost, Dropped).oneOf()
    nondet at = Set(1, 2, 3).oneOf()
    nondet plan = PLANS.oneOf()
    any {
      begin(c, base, delta),
      nondet from = CLIENTS.oneOf()
      beginKnown(c, from, delta),
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
      beginRebase(c, plan),
      beginUndo(c),
      beginReset(c),
      rwStore(c),
      rwRead(c),
      rwSwap(c, d),
      rwRecheck(c),
      stack(c, delta),
      beginLand(c),
      ldRead(c),
      ldSwap(c, d),
      ldRecheck(c),
      locate(c),
    }
  }

  // ---------- properties ----------

  /// Every commit a ref reaches, and its page, is stored: no ref dangles.
  val branchIsComplete: bool = branchIsStored(state.bucket)

  /// Every pack the pages of the commits a ref reaches (or a record reaches) name is
  /// stored: no page points into a pack that is gone.
  val packsAreStored: bool =
    packsStored(state.bucket, refsReach(state.bucket).union(reflogReaches(state.bucket)))

  /// Every big file reads back its content through its page's pack, a relocated file
  /// too: moving a file between packs (a commit writing a path to the content it has)
  /// changes no content.
  val relocateKeepsContent: bool =
    packsHold(state.bucket, refsReach(state.bucket).union(reflogReaches(state.bucket)))

  /// gc sweeping packs has doomed none that a commit a ref reaches names.
  val gcSparesNamedPacks: bool =
    state.collector.phase == GcSweepingPacks
      implies state.collector.doomedPacks.intersect(packsNamedBy(state.bucket, refsReach(state.bucket))) == Set()

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

  /// Whether `c`'s change was taken off the branch on purpose: by an undo or reset, or by
  /// a rebase that dropped its commit.
  def dropped(c: str): bool =
    state.bucket.reflog.keys().exists(id => and {
      droppedBy(state.bucket, recordOf(state.bucket, id)).contains(c),
      or {
        recordOf(state.bucket, id).op != "rebase",
        recordOf(state.bucket, id).dropped.exists(x => author(x) == c),
      },
    })

  /// A client told its commit was published finds it on the branch, now and later: its
  /// own commit, or the one a combining round made of its intent (unless an undo or reset
  /// took it off since).
  val publishedIsOnBranch: bool =
    CLIENTS.forall(c =>
      client(state, c).outcome == Published
        implies (branch(state.bucket).exists(id => author(id) == c) or dropped(c))
    )

  /// Each call publishes a client's change at most once (a move no record names enters
  /// it): once landed, taken off by an undo or reset, it comes back only by a rewrite, or
  /// by landing the stack again.
  val publishedOnce: bool =
    CLIENTS.forall(c =>
      state.history.indices().filter(i => and {
        i > 0,
        not(state.bucket.reflog.keys().exists(id => and {
          recordOf(state.bucket, id).old == state.history[i - 1],
          recordOf(state.bucket, id).new == state.history[i],
        })),
        not(authorsAt(state.bucket, state.history[i - 1]).contains(c)),
        authorsAt(state.bucket, state.history[i]).contains(c),
      }).size() <= (if (client(state, c).ld.calls > 1) client(state, c).ld.calls else 1)
    )

  /// The ref's `recent` names, for each landed intent, a commit made of it, and the
  /// branch has it (that commit, or what a rewrite made of it), unless an undo or reset
  /// took it off since.
  val recentIsTruthful: bool =
    recentOf(state.bucket).keys().forall(w => and {
      author(recentOf(state.bucket).get(w)) == w,
      branch(state.bucket).exists(id => author(id) == w) or dropped(w),
    })

  /// A rebase takes off the branch only the changes of the commits its record says it
  /// dropped; an undo or reset at most one (the tip it drops, or the one the record it
  /// undoes added back): every change landed on top meanwhile is carried.
  val rewritesDropOnlyTheirTarget: bool =
    state.bucket.reflog.keys().forall(id =>
      if (recordOf(state.bucket, id).op == "rebase")
        droppedBy(state.bucket, recordOf(state.bucket, id))
          .forall(w => recordOf(state.bucket, id).dropped.exists(x => author(x) == w))
      else droppedBy(state.bucket, recordOf(state.bucket, id)).size() <= 1
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
  /// branch moved, or that a commit landed since does not apply, so nothing was
  /// rewritten, finds none.
  val rewriteOutcomeIsTruthful: bool =
    CLIENTS.forall(c => and {
      client(state, c).outcome == Rewritten
        implies chain(state.bucket).exists(id => recordOf(state.bucket, id).by == c),
      Set(Moved, Conflicted).contains(client(state, c).outcome)
        implies chain(state.bucket).forall(id => recordOf(state.bucket, id).by != c),
    })

  /// Where a client found the commit it read is on the branch, while the branch has not
  /// been rewritten since.
  val locatedOnBranch: bool =
    CLIENTS.forall(c => client(state, c).located and client(state, c).foundAt == lineageOf(state.bucket)
      implies historyAt(state.bucket, tip(state.bucket).commit).contains(client(state, c).found))

  /// What a client found has every change the commit it read had, but those a rewrite in
  /// the branch's chain took off: rewrites carry history, they do not lose it.
  val locateKeepsChanges: bool =
    CLIENTS.forall(c => client(state, c).located and client(state, c).foundAt == lineageOf(state.bucket)
      implies authorsAt(state.bucket, client(state, c).seen.commit).forall(a => or {
        authorsAt(state.bucket, client(state, c).found).contains(a),
        chain(state.bucket).exists(r => droppedBy(state.bucket, recordOf(state.bucket, r)).contains(a)),
      }))

  /// A known head is true while its version stands: the ref at that version names its
  /// commit, `recent` and lineage, so a swap from it moves the branch from that commit.
  val knownHeadsAreTrue: bool =
    CLIENTS.forall(c => client(state, c).known.has and tip(state.bucket).version == client(state, c).known.head.version
      implies and {
        tip(state.bucket).commit == client(state, c).known.head.commit,
        recentOf(state.bucket) == client(state, c).known.recent,
        lineageOf(state.bucket) == client(state, c).known.lineage,
      })

  /// While gc marks and deletes, no writer is between its first read and its return.
  val gcRunsAlone: bool =
    Set(GcLocked, GcMarked, GcSweepingCommits, GcSweepingPages, GcSweepingPacks).contains(state.collector.phase)
      implies CLIENTS.forall(c => Set(Idle, Stacked, Leasing, CheckingLeases, Done).contains(client(state, c).phase))

  val safety: bool = and {
    gcRunsAlone,
    branchIsComplete,
    packsAreStored,
    relocateKeepsContent,
    gcSparesNamedPacks,
    refOnlyMovesForward,
    publishedIsOnBranch,
    recentIsTruthful,
    conflictedHasNothingOnBranch,
    landsAtMostOnce,
    lineageNamesItsRecord,
    recordsAreChained,
    reflogIsStored,
    rewriteOutcomeIsTruthful,
    rewritesDropOnlyTheirTarget,
    publishedOnce,
    locatedOnBranch,
    locateKeepsChanges,
    knownHeadsAreTrue,
  }
}
