// -*- mode: Bluespec; -*-
/// Rewriting a branch (`pail rebase` publishing through `Repo::rewrite_ref`), racing the
/// writers that keep landing on it. The rewrite here rewords the tip: one new commit with
/// the tip's change, on the tip's parent. Each step is one store request (or one wave):
///
/// ```text
///   beginRewrite (reads the ref: the tip to reword) ─▶ Leasing ─▶ CheckingLeases ─▶ RwStoring
///   RwStoring ─rwStore (the new commit)─▶ RwReading
///   RwReading ─rwRead─▶ RwSwapping                (carried what landed since, wrote the record)
///             ─rwRead─▶ Done (Moved)               (the tip is gone from the branch)
///   RwSwapping ─answered, won─▶ Done (Rewritten)
///              ─answered, lost─▶ RwReading
///              ─unanswered─▶ RwRechecking ─rwRecheck─▶ Done (Rewritten: my lineage) or RwReading
/// ```
///
/// `rwRead` is `rewrite_ref`'s attempt up to its swap: read the ref; if it moved past the
/// tip, carry each commit landed since onto the new tip, in order; write the record
/// (`reflog/<branch>/<id>`, naming the ref's last record), then the swap bumps the epoch
/// and names it. Nothing another actor does between the read and the record changes them
/// (the commits are immutable, gc waits for the lease). A reword keeps the tip's tree, so
/// every carried commit applies (`check_applies` always passes here).
module rewrite {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"

  /// A commit's parent: its ancestor one shorter than it.
  pure def parentOf(b: Bucket, id: CommitId): CommitId =
    ancestorsOf(b, id).fold(ROOT, (p, a) =>
      if (ancestorsOf(b, a).size() + 1 == ancestorsOf(b, id).size()) a else p
    )

  /// Read the ref: its tip is the commit to reword (the root has none to keep).
  pure def beginRewriteCan(s: State, c: str): bool =
    client(s, c).phase == Idle and tip(s.bucket).commit != ROOT

  pure def beginRewriteNext(s: State, c: str): State = {
    val expected = tip(s.bucket).commit
    withClient(s, c, {
      ...IDLE,
      phase: Leasing,
      rw: {
        ...NO_RW,
        expected: expected,
        new: remade(c, expected, parentOf(s.bucket, expected)),
      },
    })
  }

  /// Store the new commit (and its page: the tip's), under the lease.
  pure def rwStoreCan(s: State, c: str): bool = client(s, c).phase == RwStoring

  pure def rwStoreNext(s: State, c: str): State = {
    val rw = client(s, c).rw
    {
      ...s,
      bucket: stored(s.bucket, rw.new, treeOf(s.bucket, rw.expected), ancestorsOf(s.bucket, rw.expected)),
      clients: s.clients.set(c, { ...client(s, c), phase: RwReading }),
    }
  }

  /// A commit carried onto the new history, known before it is stored.
  type Carried = { id: CommitId, tree: Tree, ancestors: Set[CommitId] }

  /// Carrying commits onto the new history: the tip so far and its ancestors, each
  /// rewritten commit's successor, and the commits made.
  type Carrying = { tip: CommitId, ancestors: Set[CommitId], successors: CommitId -> CommitId, made: Set[Carried] }

  /// Carry the commit `k` past the rewritten tip, if any, onto the tip so far. It keeps
  /// its tree: the reword kept the tree under it.
  pure def carryStep(b: Bucket, c: str, extra: Set[CommitId], depth: int, k: int, a: Carrying): Carrying =
    extra.filter(x => ancestorsOf(b, x).size() == depth + k).fold(a, (acc, x) =>
      Set(remade(c, x, acc.tip)).fold(acc, (none, id) => {
        tip: id,
        ancestors: acc.ancestors.union(Set(acc.tip)),
        successors: acc.successors.put(x, id),
        made: acc.made.union(Set({ id: id, tree: treeOf(b, x), ancestors: acc.ancestors.union(Set(acc.tip)) })),
      })
    )

  /// The new history with every commit landed on the reworded tip since (up to `cur`)
  /// carried, oldest first (at most one per writer, so four).
  pure def carried(b: Bucket, c: str, rw: Rw, cur: CommitId): Carrying = {
    val extra = ancestorsOf(b, cur).union(Set(cur)).exclude(ancestorsOf(b, rw.expected).union(Set(rw.expected)))
    val depth = ancestorsOf(b, rw.expected).size() + 1
    val start = { tip: rw.new, ancestors: ancestorsOf(b, rw.new), successors: Map(rw.expected -> rw.new), made: Set() }
    Set(carryStep(b, c, extra, depth, 0, start)).fold(start, (n0, a0) =>
      Set(carryStep(b, c, extra, depth, 1, a0)).fold(a0, (n1, a1) =>
        Set(carryStep(b, c, extra, depth, 2, a1)).fold(a1, (n2, a2) => carryStep(b, c, extra, depth, 3, a2))
      )
    )
  }

  /// Read the ref. Moved off the reworded tip (it is not under the head): fail. Otherwise
  /// carry what landed since onto the new tip, storing each, and write the record.
  pure def rwReadCan(s: State, c: str): bool = client(s, c).phase == RwReading

  pure def rwReadNext(s: State, c: str): State = {
    val rw = client(s, c).rw
    val cur = tip(s.bucket)
    if (cur.commit != rw.expected and not(ancestorsOf(s.bucket, cur.commit).contains(rw.expected)))
      finished(s, c, { ...client(s, c), phase: Done, outcome: Moved })
    else Set(carried(s.bucket, c, rw, cur.commit)).fold(s, (none, a) => {
      ...s,
      bucket: recorded(
        a.made.fold(s.bucket, (acc, m) => stored(acc, m.id, m.tree, m.ancestors)),
        {
          old: cur.commit,
          new: a.tip,
          epoch: lineageOf(s.bucket).epoch + 1,
          prev: lineageOf(s.bucket).rewrite,
          by: c,
          successors: a.successors,
        },
      ),
      clients: s.clients.set(c, {
        ...client(s, c),
        phase: RwSwapping,
        rw: { ...rw, version: cur.version, tip: a.tip, id: nextRecord(s.bucket) },
      }),
    })
  }

  /// CAS the ref from the version read to the new tip, keeping `recent`, with the epoch
  /// bumped and the record named. Answered: won, or retry from the read. Unanswered: reread.
  pure def rwSwapCan(s: State, c: str): bool = client(s, c).phase == RwSwapping

  pure def rwSwapNext(s: State, c: str, d: Delivery): State = {
    val rw = client(s, c).rw
    val applies = refSwapApplies(s.bucket, rw.version)
    finished(
      {
        ...s,
        bucket:
          if (received(d))
            refRewritten(s.bucket, rw.version, {
              commit: rw.tip,
              recent: recentOf(s.bucket),
              lineage: { epoch: recordOf(s.bucket, rw.id).epoch, rewrite: rw.id },
            })
          else s.bucket,
        history: if (received(d) and applies) s.history.append(rw.tip) else s.history,
      },
      c,
      if (not(answered(d))) { ...client(s, c), phase: RwRechecking }
      else if (applies) { ...client(s, c), phase: Done, outcome: Rewritten }
      else { ...client(s, c), phase: RwReading },
    )
  }

  /// After an unanswered swap, read the ref: it won if its record is in the ref's chain
  /// (the ref names it, or a later rewrite's record leads back to it); otherwise try again.
  pure def rwRecheckCan(s: State, c: str): bool = client(s, c).phase == RwRechecking

  pure def rwRecheckNext(s: State, c: str): State =
    finished(s, c,
      if (chain(s.bucket).contains(client(s, c).rw.id))
        { ...client(s, c), phase: Done, outcome: Rewritten }
      else { ...client(s, c), phase: RwReading }
    )
}
