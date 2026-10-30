// -*- mode: Bluespec; -*-
/// Landing a stack (`pail land --stack`, `Repo::land_stack`) on the branch, racing the
/// writers and rewriters there. A client first makes its stack: a branch of its own (named
/// after it) at what the branch has then, and one commit on it. Each step is one store
/// request (or one wave):
///
/// ```text
///   Idle ─beginBranch─▶ Leasing ─▶ CheckingLeases ─▶ Branching ─createBranch─▶ Branched
///                                    └─(lease stale at its check)─▶ Done (Unknown)
///   Branched ─stack (its commit, whole)─▶ Stacked ─beginLand─▶ Leasing ─▶ CheckingLeases
///   Done (a land returned) ─beginLand─▶ Leasing    (again: idempotent)
///   CheckingLeases ─▶ LdReading
///   LdReading ─ldRead─▶ Done (Published)       (the branch has its commit, or a copy of it)
///             ─ldRead─▶ Done (Conflicted)      (a path its commit writes changed on the branch)
///             ─ldRead─▶ LdSwapping             (its commit, or a copy stored on the branch's tip)
///   LdSwapping ─answered, won─▶ Done (Published)
///              ─lost, or unanswered─▶ LdRechecking ─ldRecheck─▶ Done (Published: landed since) or LdReading
/// ```
///
/// `ldRead` is one attempt up to its swap: read the stack, then the branch; find where the
/// stack leaves the branch's history (`Repo::fork_point_at`: the merge base, or one with
/// a history the branch's rewrites replaced, if later); skip the commit if the branch has
/// a copy of it above that; fast-forward when the branch is still there; otherwise copy
/// the commit onto the branch's tip, unless a path it writes changed between.
module land {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"
  import rewrite.* from "./rewrite"

  /// Make `c`'s stack, as `pail switch -c` and `pail commit` do: first its branch, named
  /// after it, at the commit the branch has now (`create_branch`, under a lease of its own
  /// that it checks before the write); then its commit of `delta` on that, which moves the
  /// branch (an uncontended `commit` on a branch only `c` writes, taken whole).
  pure def beginBranchCan(s: State, c: str): bool = and {
    client(s, c).phase == Idle,
    not(s.bucket.refs.keys().contains(c)),
  }

  pure def beginBranchNext(s: State, c: str): State =
    withClient(s, c, {
      ...IDLE,
      phase: Leasing,
      branching: true,
      base: tip(s.bucket).commit,
      seen: { commit: tip(s.bucket).commit, version: 0 },
    })

  /// The write of `refs/<c>`, which `Lease::check` let go (`checkLeases` is where it is
  /// asked: the lease listing is the last request before it). A write that does not reach
  /// the store, or whose answer is lost, fails the call; the branch exists in the second
  /// case. Either way the lease is released.
  pure def createBranchCan(s: State, c: str): bool = client(s, c).phase == Branching

  pure def createBranchNext(s: State, c: str, d: Delivery): State = {
    val cl = client(s, c)
    val b =
      if (received(d))
        { ...s.bucket, refs: created(s.bucket.refs, c, { commit: cl.base, recent: Map(), lineage: NO_LINEAGE }) }
      else s.bucket
    {
      ...s,
      bucket: unleased(b, c),
      timing: s.timing.set(c, NO_TIMING),
      clients: s.clients.set(c,
        if (answered(d)) { ...cl, phase: Branched, branching: false }
        else { ...cl, phase: Done, outcome: Unknown, branching: false }
      ),
    }
  }

  /// Commit `c`'s `delta` on its branch (stored under a lease taken and released, with the
  /// branch moved to it). Nothing else writes either. It waits while gc holds its lease.
  pure def stackCan(s: State, c: str): bool = and {
    client(s, c).phase == Branched,
    not(leasedByOthers(s.bucket, c, Set(Exclusive), s.now)),
  }

  pure def stackNext(s: State, c: str, delta: Delta): State = {
    val on = client(s, c).base
    val id = on.append(c)
    val stored = unleased(uploaded(leased(s.bucket, c, Shared, s.now), id, on, delta, Map()), c)
    {
      ...s,
      bucket: { ...stored, refs: swapped(stored.refs, c, versionOf(stored.refs, c), { commit: id, recent: Map(), lineage: NO_LINEAGE }) },
      clients: s.clients.set(c, { ...client(s, c), phase: Stacked }),
    }
  }

  /// Start `land_stack`: it leases first. A stack landed already may be landed once more
  /// (landing is idempotent).
  pure def beginLandCan(s: State, c: str): bool = or {
    client(s, c).phase == Stacked,
    client(s, c).phase == Done and client(s, c).ld.calls == 1,
  }

  pure def beginLandNext(s: State, c: str): State =
    withClient(s, c, {
      ...client(s, c),
      phase: Leasing,
      outcome: Pending,
      ld: { ...NO_LD, calls: client(s, c).ld.calls + 1 },
    })

  /// The tip of `c`'s stack.
  pure def stackOf(s: State, c: str): CommitId = body(s.bucket.refs, c).commit

  /// The latest commit both histories have.
  pure def mergeBase(b: Bucket, x: CommitId, y: CommitId): CommitId =
    historyAt(b, x).filter(m => historyAt(b, y).contains(m)).fold(ROOT, (best, m) =>
      if (ancestorsOf(b, m).size() > ancestorsOf(b, best).size()) m else best
    )

  /// Where the stack at `t` leaves the branch's history: the merge base with the branch, or
  /// with the old tip of one of its rewrites, whichever is latest.
  pure def forkPoint(b: Bucket, t: CommitId): CommitId =
    chain(b).fold(mergeBase(b, t, tip(b).commit), (best, r) =>
      Set(mergeBase(b, t, recordOf(b, r).old)).fold(best, (n, m) =>
        if (ancestorsOf(b, m).size() > ancestorsOf(b, best).size()) m else best
      )
    )

  pure def ldReadCan(s: State, c: str): bool = client(s, c).phase == LdReading

  pure def ldReadNext(s: State, c: str): State = {
    val t = stackOf(s, c)
    val cur = tip(s.bucket)
    val base = forkPoint(s.bucket, t)
    val above = branch(s.bucket).exclude(historyAt(s.bucket, base))
    val seen = { ...client(s, c).ld, version: cur.version, lineage: lineageOf(s.bucket) }
    // The stack's one commit is below the fork point, or the branch has a copy above it.
    if (base == t or above.exists(x => authorsOf(x).contains(c)))
      finished(s, c, { ...client(s, c), phase: Done, outcome: Published, ld: seen })
    else if (base == cur.commit) {
      val after = withClient(s, c, { ...client(s, c), phase: LdSwapping, ld: { ...seen, tip: t } })
      guarded(after, c, ldSwapNext(after, c, Dropped))
    }
    else Set(deltaOf(s.bucket, t, parentOf(s.bucket, t))).fold(s, (n, d) =>
      if (d.keys().exists(p => treeOf(s.bucket, base).get(p) != treeOf(s.bucket, cur.commit).get(p)))
        finished(s, c, { ...client(s, c), phase: Done, outcome: Conflicted, ld: seen })
      else Set(remade(c, t, cur.commit)).fold(s, (n2, id) => {
        val tree = overlay(treeOf(s.bucket, cur.commit), d)
        val after = {
          ...s,
          // The copy's entries are the stack commit's: it points at the packs that commit
          // does, and stores none.
          bucket: storedCopy(
            s.bucket, id, tree, historyAt(s.bucket, cur.commit),
            copyLocs(s.bucket, tree, d, t, body(s.bucket.commits, cur.commit).locs),
          ),
          clients: s.clients.set(c, { ...client(s, c), phase: LdSwapping, ld: { ...seen, tip: id } }),
        }
        guarded(after, c, ldSwapNext(after, c, Dropped))
      })
    )
  }

  /// CAS the branch from the version read to the new tip, keeping its `recent` and
  /// lineage. Won: landed. Lost or unanswered: read the branch again.
  pure def ldSwapCan(s: State, c: str): bool = client(s, c).phase == LdSwapping

  pure def ldSwapNext(s: State, c: str, d: Delivery): State = {
    val ld = client(s, c).ld
    val applies = refSwapApplies(s.bucket, ld.version)
    finished(
      {
        ...s,
        bucket: if (received(d)) refSwapped(s.bucket, ld.version, ld.tip, recentOf(s.bucket)) else s.bucket,
        history: if (received(d) and applies) s.history.append(ld.tip) else s.history,
      },
      c,
      if (answered(d) and applies) { ...client(s, c), phase: Done, outcome: Published }
      else { ...client(s, c), phase: LdRechecking },
    )
  }

  /// After a swap that lost or went unanswered, read the branch: landed if the new tip is
  /// on it, or under the tip the first rewrite since replaced (as a writer's recheck);
  /// otherwise try again.
  pure def ldRecheckCan(s: State, c: str): bool = client(s, c).phase == LdRechecking

  pure def ldRecheckNext(s: State, c: str): State =
    finished(s, c,
      if (landedSince(s.bucket, client(s, c).ld.tip, client(s, c).ld.lineage))
        { ...client(s, c), phase: Done, outcome: Published }
      else { ...client(s, c), phase: LdReading }
    )
}
