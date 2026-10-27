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
    })

  /// Write its shared lease (`locks/<c>`).
  pure def leaseCan(s: State, c: str): bool = client(s, c).phase == Leasing

  pure def leaseNext(s: State, c: str): State = {
    ...s,
    bucket: leased(s.bucket, c, Shared),
    clients: s.clients.set(c, { ...client(s, c), phase: CheckingLeases }),
  }

  /// List the leases. Under gc's exclusive one, drop its own and try again later; gc,
  /// having written its lease before listing, waits for this one to go. Otherwise gc's
  /// lease, if any, came after this listing, so gc will see this lease and wait.
  pure def checkLeasesCan(s: State, c: str): bool = client(s, c).phase == CheckingLeases

  pure def checkLeasesNext(s: State, c: str): State =
    if (leasedByOthers(s.bucket, c, Set(Exclusive)))
      { ...s, bucket: unleased(s.bucket, c), clients: s.clients.set(c, { ...client(s, c), phase: Leasing }) }
    else
      withClient(s, c, { ...client(s, c), phase: Reading })

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
        { ...client(s, c), phase: Done, outcome: Conflicted, seen: tip(s.bucket), seenRecent: recentOf(s.bucket) }
      else
        { ...client(s, c), phase: Uploading, seen: tip(s.bucket), seenRecent: recentOf(s.bucket) }
    )

  /// Store the commit and its page before any ref can name them, listing the branch's
  /// intents meanwhile. Pending ones (not landed in the ref as read) mean writers are
  /// combining: swapping past them would starve them, so join them instead.
  pure def uploadCan(s: State, c: str): bool = client(s, c).phase == Uploading

  pure def uploadNext(s: State, c: str): State = {
    ...s,
    bucket: uploaded(
      s.bucket,
      made(c, client(s, c)),
      client(s, c).seen.commit,
      client(s, c).delta,
    ),
    clients: s.clients.set(c, {
      ...client(s, c),
      phase:
        if (liveKeys(s.bucket.intents).exclude(client(s, c).seenRecent.keys()) != Set()) Intending
        else Swapping,
    }),
  }

  /// CAS the ref from the version read to the new commit. Answered: a lost race retries
  /// from the read, and after the last direct attempt hands off to combining. Unanswered
  /// (`Lost`: performed if it applied; `Dropped`: never received): recheck.
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
        { ...client(s, c), phase: Done, outcome: Published }
      else if (client(s, c).attempt < DIRECT_ATTEMPTS)
        { ...client(s, c), phase: Reading, attempt: client(s, c).attempt + 1 }
      else
        { ...client(s, c), phase: Intending },
  )

  /// After an unanswered swap, read the ref to learn whether it won: the commit is on the
  /// branch, as its head or under commits others landed since.
  pure def recheckCan(s: State, c: str): bool = client(s, c).phase == Rechecking

  pure def recheckNext(s: State, c: str): State =
    finished(s, c,
      if (onBranch(s.bucket, made(c, client(s, c))))
        { ...client(s, c), phase: Done, outcome: Published }
      else if (client(s, c).attempt < DIRECT_ATTEMPTS)
        { ...client(s, c), phase: Reading, attempt: client(s, c).attempt + 1 }
      else
        { ...client(s, c), phase: Done, outcome: Unknown }
    )

  // ---------- failures ----------

  /// The process dies mid-call: it makes no more requests and leaves its lease behind.
  pure def crashCan(s: State, c: str): bool = not(Set(Idle, Done).contains(client(s, c).phase))

  pure def crashNext(s: State, c: str): State =
    withClient(s, c, { ...client(s, c), phase: Done, outcome: Unknown })

  /// A dead writer's lease expires (it stopped refreshing it). A live writer's never does:
  /// one that has not refreshed its lease within its lifetime makes no further ref
  /// request (`Lease::check`), so to the bucket it is dead.
  pure def leaseExpiresCan(s: State, c: str): bool =
    isLive(s.bucket.locks, c) and client(s, c).phase == Done

  pure def leaseExpiresNext(s: State, c: str): State = { ...s, bucket: unleased(s.bucket, c) }
}
