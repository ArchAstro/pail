// -*- mode: Bluespec; -*-
/// Landing a stack (`pail land --stack`, `Repo::land_stack`) on the branch, racing the
/// writers and rewriters there. A client first makes its stack: a branch of its own (named
/// after it) holding one commit on what the branch had then. Each step is one store
/// request (or one wave):
///
/// ```text
///   Idle ─stack (its branch and commit, whole)─▶ Stacked ─beginLand─▶ Leasing ─▶ CheckingLeases
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

  /// Make `c`'s stack: its commit of `delta` on the branch's tip, stored under a lease
  /// taken and released (`write_commit`), and the branch `c` naming it, under another
  /// (`create_branch`).
  /// Nothing else writes either. It waits while gc holds its lease.
  pure def stackCan(s: State, c: str): bool = and {
    client(s, c).phase == Idle,
    not(s.bucket.refs.keys().contains(c)),
    not(leasedByOthers(s.bucket, c, Set(Exclusive), s.now)),
  }

  pure def stackNext(s: State, c: str, delta: Delta): State = {
    val on = tip(s.bucket).commit
    val id = on.append(c)
    val stored = unleased(uploaded(leased(s.bucket, c, Shared, s.now), id, on, delta, Map()), c)
    val b = unleased(leased(stored, c, Shared, s.now), c)
    {
      ...s,
      bucket: { ...b, refs: created(b.refs, c, { commit: id, recent: Map(), lineage: NO_LINEAGE }) },
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
        val after = {
          ...s,
          bucket: uploaded(s.bucket, id, cur.commit, d, Map()),
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
