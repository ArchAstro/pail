// -*- mode: Bluespec; -*-
/// Rewriting a branch (`Repo::rewrite_ref`), racing the writers that keep landing on it.
/// Three rewrites, each its record's `op`:
///
/// 1. `rebase` (`pail rebase`) carries out a plan on the tip's commits in memory
///    (`rebase::build`): reword the tip, drop its parent, drop the tip, or swap the two.
/// 2. `undo` (`pail rebase --undo`) reverses the ref's last record: from its new tip back
///    to its old one, each successor mapped back.
/// 3. `reset` (`Repo::force_ref`) moves the branch to the tip's parent, dropping the tip.
///
/// Each step is one store request (or one wave):
///
/// ```text
///   begin* (reads the ref, and the record to undo) ─▶ Leasing ─▶ CheckingLeases
///   beginRebase ─▶ Done (Conflicted)               (a commit of the plan does not apply)
///   CheckingLeases ─▶ RwStoring (a plan made commits) ─rwStore (them)─▶ RwReading
///                  ─▶ RwReading (otherwise: the new tip is stored already)
///   RwReading ─rwRead─▶ RwSwapping                (carried what landed since, wrote the record)
///             ─rwRead─▶ Done (Moved)               (the tip is gone, or an undo's record is not the last)
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

  /// A plan carried out in memory (`rebase::build`): the tip so far, its tree and
  /// ancestors, each rewritten commit's successor, the commits dropped, the ones made, and
  /// whether every commit so far applied.
  type Built = {
    ok: bool,
    tip: CommitId,
    tree: Tree,
    ancestors: Set[CommitId],
    successors: CommitId -> CommitId,
    dropped: Set[CommitId],
    made: Set[Carried],
  }

  /// A commit carried onto the new history, known before it is stored.
  type Carried = { id: CommitId, tree: Tree, ancestors: Set[CommitId] }

  /// The plan so far on `base`, with `dropped` dropped.
  pure def builtOn(b: Bucket, base: CommitId, dropped: Set[CommitId]): Built = {
    ok: true,
    tip: base,
    tree: treeOf(b, base),
    ancestors: ancestorsOf(b, base),
    successors: Map(),
    dropped: dropped,
    made: Set(),
  }

  /// Pick commit `x` onto the plan so far: kept as it is if it still sits there and is not
  /// reworded; otherwise its change from its parent, which must apply, written over the
  /// tree so far; dropped if that changes nothing.
  pure def pick(b: Bucket, c: str, x: CommitId, rewords: bool, a: Built): Built =
    if (not(a.ok)) a
    else if (not(rewords) and parentOf(b, x) == a.tip)
      { ...a, tip: x, tree: treeOf(b, x), ancestors: ancestorsOf(b, x) }
    else Set(parentOf(b, x)).fold(a, (n0, p) =>
      Set(deltaOf(b, x, p)).fold(a, (n1, d) =>
        if (conflicts(d, treeOf(b, p), a.tree)) { ...a, ok: false }
        else if (overlay(a.tree, d) == a.tree) { ...a, dropped: a.dropped.union(Set(x)) }
        else Set(remade(c, x, a.tip)).fold(a, (n2, id) => {
          ...a,
          tip: id,
          tree: overlay(a.tree, d),
          ancestors: a.ancestors.union(Set(a.tip)),
          successors: a.successors.put(x, id),
          made: a.made.union(Set({ id: id, tree: overlay(a.tree, d), ancestors: a.ancestors.union(Set(a.tip)) })),
        })
      )
    )

  /// The group so far with `x`'s change from its parent added, if it applies over the tree
  /// so far with the group's changes before it (`Repo::check_applies` with an overlay).
  pure def meldStep(b: Bucket, tree: Tree, x: CommitId, g: { ok: bool, d: Delta }): { ok: bool, d: Delta } =
    Set(parentOf(b, x)).fold(g, (n0, p) =>
      Set(deltaOf(b, x, p)).fold(g, (n1, d) =>
        if (not(g.ok) or conflicts(d, treeOf(b, p), overlay(tree, g.d))) { ...g, ok: false }
        else { ...g, d: d.keys().fold(g.d, (m, q) => m.put(q, d.get(q))) }
      )
    )

  /// Meld `member` into `lead` onto the plan so far (a squash or fixup group): each one's
  /// change, lead first, must apply; the group is one new commit, both one's successor, or
  /// dropped whole if together they change nothing.
  pure def meld(b: Bucket, c: str, lead: CommitId, member: CommitId, a: Built): Built = {
    val g = meldStep(b, a.tree, member, meldStep(b, a.tree, lead, { ok: true, d: Map() }))
    val group = Set(lead, member)
    if (not(a.ok)) a
    else if (not(g.ok)) { ...a, ok: false }
    else if (overlay(a.tree, g.d) == a.tree) { ...a, dropped: a.dropped.union(group) }
    else Set(meldedId(c, lead, member, a.tip)).fold(a, (n, id) => {
      ...a,
      tip: id,
      tree: overlay(a.tree, g.d),
      ancestors: a.ancestors.union(Set(a.tip)),
      successors: group.fold(a.successors, (m, x) => m.put(x, id)),
      made: a.made.union(Set({ id: id, tree: overlay(a.tree, g.d), ancestors: a.ancestors.union(Set(a.tip)) })),
    })
  }

  /// `plan` carried out by `c` on the history at `t` (its tip `t`, parent `p`, grandparent
  /// `g`): `reword` t; `dropParent` (p dropped, t picked onto g); `dropTip` (p kept, t
  /// dropped); `reorder` (t, then p, onto g); `squash` (t melded into p, onto g);
  /// `squashUnder` (p melded into t, onto g: a fixup moved down past the commit above it).
  pure def planned(b: Bucket, c: str, plan: str, t: CommitId): Built =
    Set(parentOf(b, t)).fold(builtOn(b, t, Set()), (n0, p) =>
      Set(parentOf(b, p)).fold(n0, (n1, g) =>
        if (plan == "reword") pick(b, c, t, true, builtOn(b, p, Set()))
        else if (plan == "dropParent") pick(b, c, t, false, builtOn(b, g, Set(p)))
        else if (plan == "dropTip") pick(b, c, p, false, builtOn(b, g, Set(t)))
        else if (plan == "squash") meld(b, c, p, t, builtOn(b, g, Set()))
        else if (plan == "squashUnder") meld(b, c, t, p, builtOn(b, g, Set()))
        else pick(b, c, p, false, pick(b, c, t, false, builtOn(b, g, Set())))
      )
    )

  pure val PLANS: Set[str] = Set("dropParent", "dropTip", "reorder", "squash", "squashUnder")

  /// Read the ref and carry out `plan` on its tip's commits (the reword needs one, the
  /// others two), unleased, as `pail rebase` builds before taking its lease. A commit that
  /// does not apply fails it there: a conflict, nothing leased or stored.
  pure def beginRebaseCan(s: State, c: str, plan: str): bool = and {
    client(s, c).phase == Idle,
    tip(s.bucket).commit != ROOT,
    plan == "reword" or parentOf(s.bucket, tip(s.bucket).commit) != ROOT,
  }

  pure def beginRebaseNext(s: State, c: str, plan: str): State = {
    val expected = tip(s.bucket).commit
    val built = planned(s.bucket, c, plan, expected)
    if (not(built.ok))
      withClient(s, c, {
        ...IDLE,
        phase: Done,
        outcome: Conflicted,
        rw: { ...NO_RW, op: "rebase", plan: plan, expected: expected },
      })
    else withClient(s, c, {
      ...IDLE,
      phase: Leasing,
      rw: {
        ...NO_RW,
        op: "rebase",
        plan: plan,
        expected: expected,
        new: built.tip,
        successors: built.successors,
        dropped: built.dropped,
        made: built.made.map(m => m.id),
      },
    })
  }

  /// Reword the tip (the root has none to keep).
  pure def beginRewriteCan(s: State, c: str): bool = beginRebaseCan(s, c, "reword")

  pure def beginRewriteNext(s: State, c: str): State = beginRebaseNext(s, c, "reword")

  /// Read the ref, then the record its lineage names (a branch never rewritten has none
  /// to undo): from that record's new tip back to its old one.
  pure def beginUndoCan(s: State, c: str): bool =
    client(s, c).phase == Idle and lineageOf(s.bucket).rewrite != 0

  pure def beginUndoNext(s: State, c: str): State = {
    val r = recordOf(s.bucket, lineageOf(s.bucket).rewrite)
    withClient(s, c, {
      ...IDLE,
      phase: Leasing,
      rw: {
        ...NO_RW,
        op: "undo",
        expected: r.new,
        new: r.old,
        successors: inverse(r.successors),
        since: lineageOf(s.bucket),
      },
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

  /// Store the plan's new commits (and their pages) in one wave, under the lease. The
  /// history the plan was made from cannot have changed: commits are immutable.
  pure def rwStoreCan(s: State, c: str): bool = client(s, c).phase == RwStoring

  pure def rwStoreNext(s: State, c: str): State = {
    val rw = client(s, c).rw
    {
      ...s,
      bucket: storedAll(s.bucket, planned(s.bucket, c, rw.plan, rw.expected).made),
      clients: s.clients.set(c, { ...client(s, c), phase: RwReading }),
    }
  }

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

  /// Read the ref. Moved off the tip the rewrite was made from (it is not under the head),
  /// or, for an undo, rewritten since (its record is no longer the ref's last): fail. Otherwise carry what landed since onto the new tip, storing each; one that does
  /// not apply fails the attempt (those before it stay stored); else write the record.
  pure def rwReadCan(s: State, c: str): bool = client(s, c).phase == RwReading

  pure def rwReadNext(s: State, c: str): State = {
    val rw = client(s, c).rw
    val cur = tip(s.bucket)
    if (or {
      cur.commit != rw.expected and not(ancestorsOf(s.bucket, cur.commit).contains(rw.expected)),
      rw.op == "undo" and lineageOf(s.bucket) != rw.since,
    })
      finished(s, c, { ...client(s, c), phase: Done, outcome: Moved })
    else Set(carried(s.bucket, c, rw, cur.commit)).fold(s, (none, a) =>
      Set(storedAll(s.bucket, a.made)).fold(s, (n, b) =>
        if (not(a.ok)) finished({ ...s, bucket: b }, c, { ...client(s, c), phase: Done, outcome: Conflicted })
        else {
          val after = {
          ...s,
          bucket: recorded(b, {
            op: rw.op,
            old: cur.commit,
            new: a.tip,
            epoch: lineageOf(s.bucket).epoch + 1,
            prev: lineageOf(s.bucket).rewrite,
            by: c,
            successors: a.successors,
            dropped: rw.dropped,
          }),
          clients: s.clients.set(c, {
            ...client(s, c),
            phase: RwSwapping,
            rw: { ...rw, version: cur.version, tip: a.tip, id: nextRecord(s.bucket) },
          }),
          }
          // `rewrite_ref` checks the lease itself and returns the error: unknown outcome.
          guarded(after, c, finished(after, c, { ...client(after, c), phase: Done, outcome: Unknown }))
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
