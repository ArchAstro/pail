// -*- mode: Bluespec; -*-
/// Combining (`Repo::commit_combining`): a writer that lost its direct attempts publishes
/// an intent, then whoever combines lands every pending intent in one swap. Each step is
/// one store request (or one wave of them):
///
/// ```text
///   Intending ─intend─▶ Polling ─pollRef, pollList (either order)─▶ landed: Withdrawing (Published)
///                          ▲                                         overlap: Withdrawing (Conflicted)
///                          │                                         else Deciding
///   Deciding ─wait (not the oldest)─▶ Polling          ─giveUp─▶ Withdrawing (Unknown)
///   Deciding ─gather─▶ BatchSwapping (or Polling: nothing to land)
///   BatchSwapping ─answered, won─▶ Cleaning ─cleanup─▶ Done (mine in the batch) or Polling
///                 ─answered, lost─▶ Polling
///                 ─unanswered─▶ BatchRechecking ─batchRecheck─▶ Done (mine landed) or Polling
///   Withdrawing ─withdraw─▶ Done
/// ```
///
/// A round reads the ref and lists the intents, reads each pending one (one withdrawn
/// meanwhile is skipped), builds one commit per intent on the head, oldest first, skipping
/// any whose paths changed since its base, stores them and swaps the ref to the last,
/// adding the batch to the ref's `recent`. Reading the intents and storing the batch are
/// one step: nothing another writer does in between changes the batch (commits are
/// immutable, intents are read already, gc waits for the lease). The oldest pending intent's writer combines;
/// the rest poll, unless the ref stalls (`COMBINER_TAKEOVER`), so here anyone may.
module combine {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"

  /// A round's accumulator: the batch so far and its last commit's tree and ancestors.
  type Building = { tip: CommitId, tree: Tree, ancestors: Set[CommitId], batch: Set[Entry] }

  /// Write the intent: the change and the base it was made on.
  pure def intendCan(s: State, c: str): bool = client(s, c).phase == Intending

  pure def intendNext(s: State, c: str, at: int): State = {
    ...s,
    bucket: intentPut(s.bucket, c, { base: client(s, c).base, delta: client(s, c).delta, at: at }),
    clients: s.clients.set(c, { ...client(s, c), phase: Polling, comb: NO_COMB }),
  }

  /// Polled both: landed (mine is in `recent`), conflicted (a path mine writes changed
  /// since my base), or deciding whether to combine.
  pure def polled(s: State, c: str, cl: Client): Client =
    if (not(cl.comb.polledRef and cl.comb.polledList)) cl
    else if (cl.comb.recent.keys().contains(c))
      { ...cl, phase: Withdrawing, comb: { ...cl.comb, then: Published } }
    else if (overlaps(cl.delta, treeOf(s.bucket, cl.base), treeOf(s.bucket, cl.comb.head.commit)))
      { ...cl, phase: Withdrawing, comb: { ...cl.comb, then: Conflicted } }
    else { ...cl, phase: Deciding }

  /// Read the ref (with its `recent`).
  pure def pollRefCan(s: State, c: str): bool =
    client(s, c).phase == Polling and not(client(s, c).comb.polledRef)

  pure def pollRefNext(s: State, c: str): State =
    withClient(s, c, polled(s, c, {
      ...client(s, c),
      comb: {
        ...client(s, c).comb,
        polledRef: true,
        head: tip(s.bucket),
        recent: recentOf(s.bucket),
        lineage: lineageOf(s.bucket),
      },
    }))

  /// List the branch's intents.
  pure def pollListCan(s: State, c: str): bool =
    client(s, c).phase == Polling and not(client(s, c).comb.polledList)

  pure def pollListNext(s: State, c: str): State =
    withClient(s, c, polled(s, c, {
      ...client(s, c),
      comb: { ...client(s, c).comb, polledList: true, listed: liveKeys(s.bucket.intents) },
    }))

  /// The intents the round would land: listed and not landed, and mine.
  pure def pending(cl: Client, c: str): Set[str] =
    cl.comb.listed.exclude(cl.comb.recent.keys()).union(Set(c))

  /// Whether intent `v` sorts before `w` by id: by time, then rank (bodies outlive
  /// deletes, so any listed intent has a time).
  pure def before(b: Bucket, v: str, w: str): bool =
    body(b.intents, v).at < body(b.intents, w).at
      or (body(b.intents, v).at == body(b.intents, w).at and RANK.get(v) < RANK.get(w))

  /// Whether `c`'s intent is the oldest pending one: its writer combines.
  pure def designated(s: State, c: str): bool =
    pending(client(s, c), c).forall(w => not(before(s.bucket, w, c)))

  /// Not the oldest: poll again, leaving the round to its writer.
  pure def waitCan(s: State, c: str): bool =
    client(s, c).phase == Deciding and not(designated(s, c))

  pure def waitNext(s: State, c: str): State =
    withClient(s, c, { ...client(s, c), phase: Polling, comb: NO_COMB })

  /// The deadline passed (`COMMIT_DEADLINE`): withdraw, then fail.
  pure def giveUpCan(s: State, c: str): bool = client(s, c).phase == Deciding

  pure def giveUpNext(s: State, c: str): State =
    withClient(s, c, { ...client(s, c), phase: Withdrawing, comb: { ...client(s, c).comb, then: Unknown } })

  /// Intents whose read in `gather` may come back empty: pending ones (not mine, which is
  /// in hand) deleted by now. One still live was live at its read too, since an intent's
  /// key is never rewritten.
  pure def vanishable(s: State, c: str): Set[str] =
    pending(client(s, c), c).exclude(Set(c)).filter(w => not(isLive(s.bucket.intents, w)))

  /// The `k`th of `ws` in id order, built onto `a` unless its paths changed since its base.
  pure def buildStep(b: Bucket, ws: Set[str], k: int, a: Building): Building =
    ws.filter(w => ws.filter(v => before(b, v, w)).size() == k).fold(a, (acc, w) =>
      if (overlaps(body(b.intents, w).delta, treeOf(b, body(b.intents, w).base), acc.tree)) acc
      else Set(acc.tip.append(INTENT_NAME.get(w))).fold(acc, (none, id) => {
        tip: id,
        tree: overlay(acc.tree, body(b.intents, w).delta),
        ancestors: acc.ancestors.union(Set(acc.tip)),
        batch: acc.batch.union(Set({
          intent: w,
          commit: id,
          tree: overlay(acc.tree, body(b.intents, w).delta),
          ancestors: acc.ancestors.union(Set(acc.tip)),
        })),
      })
    )

  /// One commit per intent in `ws` on `onto`, oldest first (at most one per client).
  pure def built(b: Bucket, ws: Set[str], onto: CommitId): Building = {
    val start = { tip: onto, tree: treeOf(b, onto), ancestors: ancestorsOf(b, onto), batch: Set() }
    Set(buildStep(b, ws, 0, start)).fold(start, (n0, a0) =>
      Set(buildStep(b, ws, 1, a0)).fold(a0, (n1, a1) =>
        Set(buildStep(b, ws, 2, a1)).fold(a1, (n2, a2) => buildStep(b, ws, 3, a2))
      )
    )
  }

  /// Read the pending intents (`vanished`: those read after they were withdrawn), build the
  /// batch and store every commit of it with its page. An intent on a base gone from the
  /// store fails the round, as reading its tree fails `combine`.
  pure def gatherCan(s: State, c: str, vanished: Set[str]): bool =
    client(s, c).phase == Deciding and vanished.forall(v => vanishable(s, c).contains(v))

  pure def gatherNext(s: State, c: str, vanished: Set[str]): State = {
    val cl = client(s, c)
    val ws = pending(cl, c).exclude(vanished)
    if (ws.exists(w => not(isLive(s.bucket.commits, body(s.bucket.intents, w).base))))
      finished(s, c, { ...cl, phase: Done, outcome: Unknown })
    else
      Set(built(s.bucket, ws, cl.comb.head.commit)).fold(s, (none, r) =>
        if (r.batch == Set()) withClient(s, c, { ...cl, phase: Polling, comb: NO_COMB })
        else {
          val after = {
            ...s,
            bucket: storedBatch(s.bucket, r.batch),
            clients: s.clients.set(c,
              { ...cl, phase: BatchSwapping, comb: { ...cl.comb, batch: r.batch, tip: r.tip } }),
          }
          guarded(after, c, batchSwapNext(after, c, Dropped))
        }
      )
  }

  /// The ref's `recent` with the batch landed.
  pure def withBatch(cl: Client): Recent =
    cl.comb.batch.fold(cl.comb.recent, (r, e) => r.put(e.intent, e.commit))

  /// CAS the ref from the head read to the batch's last commit.
  pure def batchSwapCan(s: State, c: str): bool = client(s, c).phase == BatchSwapping

  pure def batchSwapNext(s: State, c: str, d: Delivery): State = {
    val cl = client(s, c)
    val applies = received(d) and refSwapApplies(s.bucket, cl.comb.head.version)
    {
      ...s,
      bucket:
        if (received(d)) refSwapped(s.bucket, cl.comb.head.version, cl.comb.tip, withBatch(cl))
        else s.bucket,
      history: if (applies) s.history.append(cl.comb.tip) else s.history,
      clients: s.clients.set(c,
        if (not(answered(d))) { ...cl, phase: BatchRechecking }
        else if (applies) { ...cl, phase: Cleaning }
        else { ...cl, phase: Polling, comb: NO_COMB }
      ),
    }
  }

  /// Landed: delete the batch's intents (best effort), then return if mine was in it.
  pure def cleanupCan(s: State, c: str): bool = client(s, c).phase == Cleaning

  pure def cleanupNext(s: State, c: str): State = {
    val cl = client(s, c)
    finished(
      { ...s, bucket: intentsDeleted(s.bucket, cl.comb.batch.map(e => e.intent)) },
      c,
      if (cl.comb.batch.exists(e => e.intent == c))
        {
          ...cl,
          phase: Done,
          outcome: Published,
          known: knownOf(
            withBatch(cl).get(c),
            { commit: cl.comb.tip, version: cl.comb.head.version + 1 },
            withBatch(cl),
            cl.comb.lineage,
          ),
        }
      else { ...cl, phase: Polling, comb: NO_COMB },
    )
  }

  /// After an unanswered swap, read the ref: at the batch's tip, the swap won.
  pure def batchRecheckCan(s: State, c: str): bool = client(s, c).phase == BatchRechecking

  pure def batchRecheckNext(s: State, c: str): State = {
    val cl = client(s, c)
    finished(s, c,
      if (tip(s.bucket).commit == cl.comb.tip and recentOf(s.bucket).keys().contains(c))
        {
          ...cl,
          phase: Done,
          outcome: Published,
          known: knownOf(recentOf(s.bucket).get(c), tip(s.bucket), recentOf(s.bucket), lineageOf(s.bucket)),
        }
      else { ...cl, phase: Polling, comb: NO_COMB }
    )
  }

  /// Delete my intent, then return.
  pure def withdrawCan(s: State, c: str): bool = client(s, c).phase == Withdrawing

  pure def withdrawNext(s: State, c: str): State = {
    val cl = client(s, c)
    finished(
      { ...s, bucket: intentsDeleted(s.bucket, Set(c)) },
      c,
      {
        ...cl,
        phase: Done,
        outcome: cl.comb.then,
        known:
          if (cl.comb.then == Published) knownOf(cl.comb.recent.get(c), cl.comb.head, cl.comb.recent, cl.comb.lineage)
          else cl.known,
      },
    )
  }
}
