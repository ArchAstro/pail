// -*- mode: Bluespec; -*-
/// Time: the clock, and the refresher that keeps each held lease alive (`lease.rs`). A lease
/// is trusted by its holder while `Lease::check` passes (refreshed within `MAX_AGE`) and by
/// the others while its LastModified is within `TTL` of their clock, so a holder that
/// stalls looks dead to the others before it knows it. The margin between the two is what
/// keeps a stalled holder from swapping a ref or deleting an object after the others
/// moved on:
///
/// ```text
///   t=0           refresh landed (LastModified)
///   t=MAX_AGE     the holder's last chance to pass its check            ┐ LATENCY: the request
///   t=MAX_AGE+D   ... and the last tick its swap or delete can land     ┘ it checked for lands
///   t=TTL-SKEW+1  the first moment another may call the lease dead (its clock ahead by SKEW)
/// ```
///
/// Safe while `LATENCY + SKEW <= MAX_AGE` (README "Lease timing"). A refresher sends every
/// `REFRESH` ticks after its last finished; its put may take any time to land, or never.
module clock {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"

  /// Time can pass, except that a request a passed check allowed lands within `LATENCY`
  /// ticks of the check: the assumption D. A process stalled longer between its check and
  /// its request is what no margin protects against.
  pure def tickCan(s: State): bool = and {
    CLIENTS.forall(c => not(swapping(client(s, c))) or s.now - s.timing.get(c).checked < LATENCY),
    not(deleting(s.collector)) or s.now - s.timing.get(GC).checked < LATENCY,
  }

  /// A holder whose refresh is due sends it: the put is in flight.
  pure def ticked(t: Timing, now: int): Timing =
    if (t.running and not(t.inflight) and now - t.done >= REFRESH) { ...t, inflight: true, sent: now } else t

  pure def tickNext(s: State): State = {
    ...s,
    now: s.now + 1,
    timing: s.timing.keys().mapBy(h => ticked(s.timing.get(h), s.now + 1)),
  }

  /// The refresh put reaches the store (unless `Dropped`), which stamps the lease with the
  /// time it lands. `Answered`: its reply is on its way back. `Lost` or `Dropped`: an error,
  /// the refresher sleeps again and nothing is stamped.
  pure def refreshLandCan(s: State, h: str): bool =
    s.timing.get(h).running and s.timing.get(h).inflight and not(s.timing.get(h).landed)

  pure def refreshLandNext(s: State, h: str, d: Delivery): State = {
    val t = s.timing.get(h)
    {
      ...s,
      bucket:
        if (received(d)) leased(s.bucket, h, body(s.bucket.locks, h).kind, s.now)
        else s.bucket,
      timing: s.timing.set(h,
        if (answered(d)) { ...t, landed: true }
        else { ...t, inflight: false, sent: 0, done: s.now }
      ),
    }
  }

  /// The reply arrives: the lease counts as refreshed from the start of the put (the
  /// instant before sending, which is no later than the put's LastModified), but only if
  /// the holder is still fresh. One that has gone stale stays so: the others may have
  /// ignored its lease before this refresh landed, and a refresh cannot undo that.
  pure def refreshAckCan(s: State, h: str): bool =
    s.timing.get(h).running and s.timing.get(h).landed

  pure def refreshAckNext(s: State, h: str): State = {
    val t = s.timing.get(h)
    {
      ...s,
      timing: s.timing.set(h, {
        ...t,
        refreshed: if (fresh(s, h)) t.sent else t.refreshed,
        done: s.now,
        inflight: false,
        sent: 0,
        landed: false,
      }),
    }
  }
}
