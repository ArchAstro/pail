// -*- mode: Bluespec; -*-
/// Publishing a commit: `Repo::commit` racing other writers on one branch, over a store
/// whose requests can fail in transit. Each step is one store request a client makes:
///
/// ```text
///   begin ─▶ Leasing ─lease─▶ CheckingLeases ─checkLeases─▶ Reading   (gc holds one: Leasing)
///
///            Reading ─readRef─▶ Uploading ─upload─▶ Swapping ─swap(Answered)─▶ Done
///               ▲  │ overlap: Conflicted     │         │  │ lost the race: retry
///               │  ▼      intents pending:   ▼         │  ▼
///               │ Done              Intending ◀── swap(Lost|Dropped) Reading (or Intending)
///               └────────── recheck ◀── Rechecking ◀───┘
/// ```
///
/// Each step is a pair: `xCan(s, ...)`, whether the client can take it, and `x(s, ...)`,
/// the state after. A client holds a shared lease from before its first read to its return
/// (`finished`), so gc, which needs an exclusive one, never runs while it is in flight.
/// `Intending` hands off to combining (`combine.qnt`).
module publish {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"

  /// `commit(base, delta)` is called; its first request will take a lease.
  pure def beginCan(s: State, c: str, base: CommitId): bool =
    client(s, c).phase == Idle and isLive(s.bucket.commits, base)

  pure def beginNext(s: State, c: str, base: CommitId, delta: Delta): State =
    withClient(s, c, {
      ...IDLE,
      base: base,
      delta: delta,
      phase: Leasing,
      seen: { commit: base, version: 0 },
      reuse: reuseOf(s.bucket, base, delta),
    })

  /// `commit` called with the head `from`'s last publish left (`KnownHead`): on that
  /// head, as read, so its first attempt need not read the ref.
  pure def beginKnownCan(s: State, c: str, from: str): bool = and {
    client(s, c).phase == Idle,
    client(s, from).known.has,
    isLive(s.bucket.commits, client(s, from).known.head.commit),
  }

  pure def beginKnownNext(s: State, c: str, from: str, delta: Delta): State = {
    val k = client(s, from).known
    withClient(s, c, {
      ...IDLE,
      base: k.head.commit,
      delta: delta,
      phase: Leasing,
      seen: k.head,
      seenRecent: k.recent,
      seenLineage: k.lineage,
      reuse: reuseOf(s.bucket, k.head.commit, delta),
    })
  }

  /// Write its shared lease (`locks/<c>`).
  pure def leaseCan(s: State, c: str): bool = client(s, c).phase == Leasing

  pure def leaseNext(s: State, c: str): State = {
    ...s,
    bucket: leased(s.bucket, c, Shared, s.now),
    clients: s.clients.set(c, { ...client(s, c), phase: CheckingLeases }),
  }

  /// List the leases. Under gc's exclusive one, drop its own and try again later; gc,
  /// having written its lease before listing, waits for this one to go. Otherwise gc's
  /// lease, if any, came after this listing, so gc will see this lease and wait. A
  /// rewriter goes on to store its new commit (`rewrite.qnt`).
  pure def checkLeasesCan(s: State, c: str): bool = client(s, c).phase == CheckingLeases

  pure def checkLeasesNext(s: State, c: str): State =
    if (leasedByOthers(s.bucket, c, Set(Exclusive), s.now))
      withClient({ ...s, bucket: unleased(s.bucket, c) }, c, { ...client(s, c), phase: Leasing })
    else
      holding(
        withClient(s, c, { ...client(s, c), phase:
          if (client(s, c).ld.calls > 0) LdReading
          // A known head (`beginKnown`): the first attempt swaps from it without reading.
          else if (client(s, c).rw == NO_RW and client(s, c).attempt == 1 and client(s, c).seen.version > 0) Uploading
          else if (client(s, c).rw == NO_RW) Reading
          else if (client(s, c).rw.made != Set()) RwStoring
          else RwReading }),
        c,
      )

  /// Read the ref. A head past the base replays the delta onto it, unless a path the delta
  /// writes changed in between (`check_disjoint`).
  pure def readRefCan(s: State, c: str): bool = client(s, c).phase == Reading

  pure def readRefNext(s: State, c: str): State =
    finished(s, c,
      if (overlaps(
        client(s, c).delta,
        treeOf(s.bucket, client(s, c).base),
        treeOf(s.bucket, tip(s.bucket).commit),
      ))
        { ...client(s, c), phase: Done, outcome: Conflicted, seen: tip(s.bucket), seenRecent: recentOf(s.bucket), seenLineage: lineageOf(s.bucket) }
      else
        { ...client(s, c), phase: Uploading, seen: tip(s.bucket), seenRecent: recentOf(s.bucket), seenLineage: lineageOf(s.bucket) }
    )

  /// Store the commit and its page before any ref can name them, listing the branch's
  /// intents meanwhile. Pending ones (not landed in the ref as read) mean writers are
  /// combining: swapping past them would starve them, so join them instead.
  pure def uploadCan(s: State, c: str): bool = client(s, c).phase == Uploading

  pure def uploadNext(s: State, c: str): State = {
    val after = {
      ...s,
      bucket: uploaded(
        s.bucket,
        made(c, client(s, c)),
        client(s, c).seen.commit,
        client(s, c).delta,
        client(s, c).reuse,
      ),
      clients: s.clients.set(c, {
        ...client(s, c),
        phase:
          if (liveKeys(s.bucket.intents).exclude(client(s, c).seenRecent.keys()) != Set()) Intending
          else Swapping,
      }),
    }
    // Before the swap, `advance_ref` checks the lease: refused, the swap never leaves.
    if (client(after, c).phase == Swapping) guarded(after, c, swapNext(after, c, Dropped)) else after
  }

  /// CAS the ref from the version read to the new commit. Answered: a lost race retries
  /// from the read, and after the last direct attempt hands off to combining. Unanswered
  /// (`Lost`: performed if it applied; `Dropped`: never received): recheck. A swap the lease
  /// check refused (`guarded`) is a `Dropped` one: `commit_opts` takes the error for an
  /// unknown outcome.
  pure def swapCan(s: State, c: str): bool = client(s, c).phase == Swapping

  pure def swapNext(s: State, c: str, d: Delivery): State = finished(
    {
      ...s,
      bucket:
        if (received(d))
          refSwapped(s.bucket, client(s, c).seen.version, made(c, client(s, c)), client(s, c).seenRecent)
        else s.bucket,
      history:
        if (received(d) and refSwapApplies(s.bucket, client(s, c).seen.version))
          s.history.append(made(c, client(s, c)))
        else s.history,
    },
    c,
      if (not(answered(d)))
        { ...client(s, c), phase: Rechecking }
      else if (refSwapApplies(s.bucket, client(s, c).seen.version))
        {
          ...client(s, c),
          phase: Done,
          outcome: Published,
          known: knownOf(
            made(c, client(s, c)),
            { commit: made(c, client(s, c)), version: client(s, c).seen.version + 1 },
            client(s, c).seenRecent,
            client(s, c).seenLineage,
          ),
        }
      else if (client(s, c).attempt < DIRECT_ATTEMPTS)
        { ...client(s, c), phase: Reading, attempt: client(s, c).attempt + 1 }
      else
        { ...client(s, c), phase: Intending },
  )

  /// After an unanswered swap, read the ref to learn whether it won: the commit is on the
  /// branch, as its head or under commits others landed since, or, if the branch was
  /// rewritten since, under the tip the first rewrite replaced (its record's `old`).
  pure def recheckCan(s: State, c: str): bool = client(s, c).phase == Rechecking

  pure def recheckNext(s: State, c: str): State =
    finished(s, c,
      if (landedSince(s.bucket, made(c, client(s, c)), client(s, c).seenLineage))
        {
          ...client(s, c),
          phase: Done,
          outcome: Published,
          known: knownOf(made(c, client(s, c)), tip(s.bucket), recentOf(s.bucket), lineageOf(s.bucket)),
        }
      else if (client(s, c).attempt < DIRECT_ATTEMPTS)
        { ...client(s, c), phase: Reading, attempt: client(s, c).attempt + 1 }
      else
        { ...client(s, c), phase: Done, outcome: Unknown }
    )

  // ---------- failures ----------

  /// The process dies mid-call: it makes no more requests, its refresher stops, and it
  /// leaves its lease behind, which others ignore once it is older than `TTL`.
  pure def crashCan(s: State, c: str): bool = not(Set(Idle, Stacked, Done).contains(client(s, c).phase))

  pure def crashNext(s: State, c: str): State =
    notHolding(withClient(s, c, { ...client(s, c), phase: Done, outcome: Unknown }), c)
}
