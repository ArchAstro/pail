// -*- mode: Bluespec; -*-
/// `pail gc` (`gc::gc`): take the exclusive lease, wait for every writer's lease to go,
/// then mark what the refs reach and, for commits and then pages, list the unreachable
/// objects older than the grace period and delete them one by one (commits, pages, then
/// packs: a pack goes only if no marked page names it). Each step is one phase of requests.
///
/// The lease is what makes this safe: a writer can reuse an object that is already stored
/// without rewriting it (`put_new` of an existing object writes nothing), so an object's
/// age says nothing about whether an in-flight commit needs it. With writers drained,
/// nothing is in flight while gc deletes. The grace period then only spares objects
/// written outside any lease.
///
/// ```text
///   GcIdle ─lease─▶ GcDraining ─checkLeases (none left)─▶ GcLocked ─mark─▶ GcMarked ─listCommits─▶ GcSweepingCommits ─deleteCommit(id)*─┐
///      ▲                                                                       │
///      │      GcSweepingPages ◀─ listPages ◀─────────────────────────────────────────────┘
///      │           │ deletePage(t)*
///      └── done (release) ◀─ deletePack(k)* ◀─ GcSweepingPacks ◀─ listPacks
/// ```
///
/// Time is abstract: `graceElapses` makes every object old.
module gc {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"

  /// Write the exclusive lease (`locks/gc`). Writers that list after this back off.
  pure def gcLeaseCan(s: State): bool = s.collector.phase == GcIdle

  pure def gcLeaseNext(s: State): State = {
    ...s,
    bucket: leased(s.bucket, GC, Exclusive, s.now),
    collector: { ...s.collector, phase: GcDraining },
  }

  /// List the leases: wait while a writer holds one (it listed before gc's lease was
  /// there, so it went ahead). Another collector's lease: back off.
  pure def gcCheckLeasesCan(s: State): bool = s.collector.phase == GcDraining

  pure def gcCheckLeasesNext(s: State): State =
    if (leasedByOthers(s.bucket, GC, Set(Exclusive), s.now))
      notHolding({ ...s, bucket: unleased(s.bucket, GC), collector: GC_IDLE }, GC)
    else if (leasedByOthers(s.bucket, GC, Set(Shared), s.now)) s
    else holding({ ...s, collector: { ...s.collector, phase: GcLocked } }, GC)

  /// Read the refs, then every reflog record, and walk what they reach (commits, then each
  /// commit's root page, then the packs its page names): a rewrite's old history stays,
  /// for successor walks and undo.
  pure def gcMarkCan(s: State): bool = s.collector.phase == GcLocked

  pure def gcMarkNext(s: State): State = {
    ...s,
    collector: {
      ...s.collector,
      phase: GcMarked,
      liveCommits: refsReach(s.bucket).union(reflogReaches(s.bucket)),
      livePages: refsReach(s.bucket).union(reflogReaches(s.bucket)).map(id => pageOf(s.bucket, id)),
      livePacks: packsNamedBy(s.bucket, refsReach(s.bucket).union(reflogReaches(s.bucket))),
    },
  }

  /// Take `now`, then list `commits/`: the unmarked ones not written within the grace
  /// period of `now` are doomed. `now` stays the cutoff for every later listing, so what
  /// was young then is spared even if the grace period elapses mid-run.
  pure def gcListCommitsCan(s: State): bool = s.collector.phase == GcMarked

  pure def gcListCommitsNext(s: State): State = gcGuarded({
    ...s,
    collector: {
      ...s.collector,
      phase: GcSweepingCommits,
      spared: s.bucket.young,
      doomedCommits: liveKeys(s.bucket.commits).filter(id =>
        not(s.collector.liveCommits.contains(id)) and not(s.bucket.young.commits.contains(id))
      ),
    },
  })

  pure def gcDeleteCommitCan(s: State, id: CommitId): bool =
    s.collector.phase == GcSweepingCommits and s.collector.doomedCommits.contains(id)

  pure def gcDeleteCommitNext(s: State, id: CommitId): State = gcGuarded({
    ...s,
    bucket: { ...s.bucket, commits: deleted(s.bucket.commits, id) },
    collector: { ...s.collector, doomedCommits: s.collector.doomedCommits.exclude(Set(id)) },
  })

  /// List `pages/` once every doomed commit is gone.
  pure def gcListPagesCan(s: State): bool =
    s.collector.phase == GcSweepingCommits and s.collector.doomedCommits == Set()

  pure def gcListPagesNext(s: State): State = gcGuarded({
    ...s,
    collector: {
      ...s.collector,
      phase: GcSweepingPages,
      doomedPages: liveKeys(s.bucket.pages).filter(t =>
        not(s.collector.livePages.contains(t))
          and not(s.collector.spared.pages.contains(t))
          and not(s.bucket.young.pages.contains(t))
      ),
    },
  })

  pure def gcDeletePageCan(s: State, t: Page): bool =
    s.collector.phase == GcSweepingPages and s.collector.doomedPages.contains(t)

  pure def gcDeletePageNext(s: State, t: Page): State = gcGuarded({
    ...s,
    bucket: { ...s.bucket, pages: deleted(s.bucket.pages, t) },
    collector: { ...s.collector, doomedPages: s.collector.doomedPages.exclude(Set(t)) },
  })

  /// List `packs/` once every doomed page is gone.
  pure def gcListPacksCan(s: State): bool =
    s.collector.phase == GcSweepingPages and s.collector.doomedPages == Set()

  pure def gcListPacksNext(s: State): State = gcGuarded({
    ...s,
    collector: {
      ...s.collector,
      phase: GcSweepingPacks,
      doomedPacks: liveKeys(s.bucket.packs).filter(k =>
        not(s.collector.livePacks.contains(k))
          and not(s.collector.spared.packs.contains(k))
          and not(s.bucket.young.packs.contains(k))
      ),
    },
  })

  pure def gcDeletePackCan(s: State, k: PackId): bool =
    s.collector.phase == GcSweepingPacks and s.collector.doomedPacks.contains(k)

  pure def gcDeletePackNext(s: State, k: PackId): State = gcGuarded({
    ...s,
    bucket: { ...s.bucket, packs: deleted(s.bucket.packs, k) },
    collector: { ...s.collector, doomedPacks: s.collector.doomedPacks.exclude(Set(k)) },
  })

  pure def gcDoneCan(s: State): bool =
    s.collector.phase == GcSweepingPacks and s.collector.doomedPacks == Set()

  pure def gcDoneNext(s: State): State =
    notHolding({ ...s, bucket: unleased(s.bucket, GC), collector: GC_IDLE }, GC)

  /// Before each delete gc checks its lease (`Lease::check`): once it has gone too long
  /// without refreshing it, to the others it is dead, and what it marked may be stale. It
  /// then deletes nothing more: it returns the error, releasing its lease.
  pure def gcGuarded(s: State): State =
    if (deleting(s.collector))
      guarded(s, GC, {
        ...s,
        collector: {
          ...s.collector,
          phase: GcSweepingPacks,
          doomedCommits: Set(),
          doomedPages: Set(),
          doomedPacks: Set(),
        },
      })
    else s

  /// The grace period passes: every object is now old. It can pass at any moment: the
  /// lease, not the grace period, keeps gc from deleting what a commit needs.
  pure def graceElapsesNext(s: State): State = {
    ...s,
    bucket: { ...s.bucket, young: { pages: Set(), commits: Set(), packs: Set() } },
    // Mid-sweep, what is young now was young at `now` or written after it: either way
    // newer than the cutoff. (Before the first listing there is no cutoff yet.)
    collector:
      if (not(Set(GcSweepingCommits, GcSweepingPages, GcSweepingPacks).contains(s.collector.phase))) s.collector
      else {
        ...s.collector,
        spared: {
          pages: s.collector.spared.pages.union(s.bucket.young.pages),
          commits: s.collector.spared.commits.union(s.bucket.young.commits),
          packs: s.collector.spared.packs.union(s.bucket.young.packs),
        },
      },
  }
}
