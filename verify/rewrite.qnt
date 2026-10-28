// -*- mode: Bluespec; -*-
/// Rewriting a branch (`Repo::rewrite_ref`), racing the writers that keep landing on it.
/// Three rewrites, each its record's `op`:
///
/// 1. `rebase` (`pail rebase`) rewords the tip: one new commit with the tip's change, on
///    the tip's parent.
/// 2. `undo` (`pail rebase --undo`) reverses the ref's last record: from its new tip back
///    to its old one, each successor mapped back.
/// 3. `reset` (`Repo::force_ref`) moves the branch to the tip's parent, dropping the tip.
///
/// Each step is one store request (or one wave):
///
/// ```text
///   begin* (reads the ref, and the record to undo) ─▶ Leasing ─▶ CheckingLeases
///   CheckingLeases ─▶ RwStoring (rebase) ─rwStore (the new commit)─▶ RwReading
///                  ─▶ RwReading (undo, reset: the new tip is stored already)
///   RwReading ─rwRead─▶ RwSwapping                (carried what landed since, wrote the record)
///             ─rwRead─▶ Done (Moved)               (the tip is gone from the branch)
///             ─rwRead─▶ Done (Conflicted)          (a commit landed since does not apply)
///   RwSwapping ─answered, won─▶ Done (Rewritten)
///              ─answered, lost─▶ RwReading
///              ─unanswered─▶ RwRechecking ─rwRecheck─▶ Done (Rewritten: my lineage) or RwReading
/// ```
///
/// `rwRead` is `rewrite_ref`'s attempt up to its swap: read the ref; if it moved past the
/// tip, carry each commit landed since onto the new tip, in order, each checked to apply
/// and stored; write the record (`reflog/<branch>/<id>`, naming the ref's last record),
/// then the swap bumps the epoch and names it. Nothing another actor does between the read
/// and the record changes them (the commits are immutable, gc waits for the lease).
module rewrite {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"

  /// A commit's parent: its ancestor one shorter than it.
  pure def parentOf(b: Bucket, id: CommitId): CommitId =
    ancestorsOf(b, id).fold(ROOT, (p, a) =>
      if (ancestorsOf(b, a).size() + 1 == ancestorsOf(b, id).size()) a else p
    )

  /// Each rewritten commit's successor in `m`, mapped back.
  pure def inverse(m: CommitId -> CommitId): CommitId -> CommitId =
    m.keys().fold(Map(), (acc, k) => acc.put(m.get(k), k))

  /// Read the ref: its tip is the commit to reword (the root has none to keep).
  pure def beginRewriteCan(s: State, c: str): bool =
    client(s, c).phase == Idle and tip(s.bucket).commit != ROOT

  pure def beginRewriteNext(s: State, c: str): State = {
    val expected = tip(s.bucket).commit
    val new = remade(c, expected, parentOf(s.bucket, expected))
    withClient(s, c, {
      ...IDLE,
      phase: Leasing,
      rw: { ...NO_RW, op: "rebase", expected: expected, new: new, successors: Map(expected -> new) },
    })
  }

  /// Read the ref, then the record its lineage names (a branch never rewritten has none
  /// to undo): from that record's new tip back to its old one.
  pure def beginUndoCan(s: State, c: str): bool =
    client(s, c).phase == Idle and lineageOf(s.bucket).rewrite != 0

  pure def beginUndoNext(s: State, c: str): State = {
    val r = recordOf(s.bucket, lineageOf(s.bucket).rewrite)
    withClient(s, c, {
      ...IDLE,
      phase: Leasing,
      rw: { ...NO_RW, op: "undo", expected: r.new, new: r.old, successors: inverse(r.successors) },
    })
  }

  /// Read the ref: its tip is the commit to drop.
  pure def beginResetCan(s: State, c: str): bool = beginRewriteCan(s, c)

  pure def beginResetNext(s: State, c: str): State = {
    val expected = tip(s.bucket).commit
    withClient(s, c, {
      ...IDLE,
      phase: Leasing,
      rw: { ...NO_RW, op: "reset", expected: expected, new: parentOf(s.bucket, expected) },
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

  /// Carrying commits onto the new history: the tip so far, its tree and ancestors, each
  /// rewritten commit's successor, the commits made, and whether each carried so far
  /// applied (the first that does not ends the attempt).
  type Carrying = {
    tip: CommitId,
    tree: Tree,
    ancestors: Set[CommitId],
    successors: CommitId -> CommitId,
    made: Set[Carried],
    ok: bool,
  }

  /// Carry the commit `k` past the rewritten tip, if any, onto the tip so far: its change
  /// from its parent, written over the tip's tree, if it applies there.
  pure def carryStep(b: Bucket, c: str, extra: Set[CommitId], depth: int, k: int, a: Carrying): Carrying =
    extra.filter(x => ancestorsOf(b, x).size() == depth + k).fold(a, (acc, x) =>
      Set(parentOf(b, x)).fold(acc, (n0, p) =>
        Set(deltaOf(b, x, p)).fold(acc, (n1, d) =>
          if (not(acc.ok)) acc
          else if (conflicts(d, treeOf(b, p), acc.tree)) { ...acc, ok: false }
          else Set(remade(c, x, acc.tip)).fold(acc, (n2, id) => {
            tip: id,
            tree: overlay(acc.tree, d),
            ancestors: acc.ancestors.union(Set(acc.tip)),
            successors: acc.successors.put(x, id),
            made: acc.made.union(Set({ id: id, tree: overlay(acc.tree, d), ancestors: acc.ancestors.union(Set(acc.tip)) })),
            ok: true,
          })
        )
      )
    )

  /// The new history with every commit landed on the reworded tip since (up to `cur`)
  /// carried, oldest first (at most one per writer, so four).
  pure def carried(b: Bucket, c: str, rw: Rw, cur: CommitId): Carrying = {
    val extra = ancestorsOf(b, cur).union(Set(cur)).exclude(ancestorsOf(b, rw.expected).union(Set(rw.expected)))
    val depth = ancestorsOf(b, rw.expected).size() + 1
    val start = {
      tip: rw.new,
      tree: treeOf(b, rw.new),
      ancestors: ancestorsOf(b, rw.new),
      successors: rw.successors,
      made: Set(),
      ok: true,
    }
    Set(carryStep(b, c, extra, depth, 0, start)).fold(start, (n0, a0) =>
      Set(carryStep(b, c, extra, depth, 1, a0)).fold(a0, (n1, a1) =>
        Set(carryStep(b, c, extra, depth, 2, a1)).fold(a1, (n2, a2) => carryStep(b, c, extra, depth, 3, a2))
      )
    )
  }

  /// Read the ref. Moved off the tip the rewrite was made from (it is not under the head):
  /// fail. Otherwise carry what landed since onto the new tip, storing each; one that does
  /// not apply fails the attempt (those before it stay stored); else write the record.
  pure def rwReadCan(s: State, c: str): bool = client(s, c).phase == RwReading

  pure def rwReadNext(s: State, c: str): State = {
    val rw = client(s, c).rw
    val cur = tip(s.bucket)
    if (cur.commit != rw.expected and not(ancestorsOf(s.bucket, cur.commit).contains(rw.expected)))
      finished(s, c, { ...client(s, c), phase: Done, outcome: Moved })
    else Set(carried(s.bucket, c, rw, cur.commit)).fold(s, (none, a) =>
      Set(a.made.fold(s.bucket, (acc, m) => stored(acc, m.id, m.tree, m.ancestors))).fold(s, (n, b) =>
        if (not(a.ok)) finished({ ...s, bucket: b }, c, { ...client(s, c), phase: Done, outcome: Conflicted })
        else {
          ...s,
          bucket: recorded(b, {
            op: rw.op,
            old: cur.commit,
            new: a.tip,
            epoch: lineageOf(s.bucket).epoch + 1,
            prev: lineageOf(s.bucket).rewrite,
            by: c,
            successors: a.successors,
          }),
          clients: s.clients.set(c, {
            ...client(s, c),
            phase: RwSwapping,
            rw: { ...rw, version: cur.version, tip: a.tip, id: nextRecord(s.bucket) },
          }),
        }
      )
    )
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
