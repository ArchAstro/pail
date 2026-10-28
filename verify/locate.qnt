// -*- mode: Bluespec; -*-
/// Finding a commit in a rewritten history (`Repo::successor`): a working copy built on
/// a commit it read at some lineage asks where that commit is now (`pail sync`, `pail
/// commit` after a rewrite). Each rewrite since, oldest first, maps it: a rewritten
/// commit to its successor, a dropped one to its nearest kept ancestor's, one the new
/// history still has to itself.
///
/// ```text
///   Done (a writer that read the ref) ─locate─▶ Done (found: where its read commit is now)
/// ```
module locate {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"

  /// Where a commit of `x`'s history (itself included) is after record `r`: its successor,
  /// itself when `r`'s new history has it as it is, none when dropped.
  pure def placesOf(b: Bucket, r: Rewrite, x: CommitId): Set[CommitId] =
    historyAt(b, x).fold(Set(), (acc, h) =>
      if (r.successors.keys().contains(h)) acc.union(Set(r.successors.get(h)))
      else if (not(r.dropped.contains(h)) and historyAt(b, r.new).contains(h)) acc.union(Set(h))
      else acc
    )

  /// Where `x` is after record `r` (`map_through`): the latest of its history's places, as
  /// a reorder can move an older commit above a newer one.
  pure def mapThrough(b: Bucket, r: Rewrite, x: CommitId): CommitId =
    placesOf(b, r, x).fold(ROOT, (best, h) => if (ancestorsOf(b, h).size() > ancestorsOf(b, best).size()) h else best)

  /// Whether walking back to lineage `from` stops at record `id` (`rewrites_between`):
  /// none, `from`'s own, or one no later than its epoch.
  pure def stops(b: Bucket, from: Lineage, id: int): bool =
    id == 0 or id == from.rewrite or recordOf(b, id).epoch <= from.epoch

  /// `x` through record `id` if `walked`, else as it is.
  pure def through(b: Bucket, walked: bool, id: int, x: CommitId): CommitId =
    if (walked) mapThrough(b, recordOf(b, id), x) else x

  /// Where `x`, from the history at lineage `since`, is in the history at `now`
  /// (`Repo::successor`): through each record between, oldest first (four at most).
  pure def successorOf(b: Bucket, x: CommitId, since: Lineage, now: Lineage): CommitId = {
    val r0 = now.rewrite
    val r1 = prevOf(b, r0)
    val r2 = prevOf(b, r1)
    val r3 = prevOf(b, r2)
    val w0 = not(stops(b, since, r0))
    val w1 = w0 and not(stops(b, since, r1))
    val w2 = w1 and not(stops(b, since, r2))
    val w3 = w2 and not(stops(b, since, r3))
    through(b, w0, r0, through(b, w1, r1, through(b, w2, r2, through(b, w3, r3, x))))
  }

  /// Ask where the commit `c` last read is now.
  /// Once per lineage: asking again before the branch is rewritten finds the same.
  pure def locateCan(s: State, c: str): bool = and {
    client(s, c).phase == Done,
    client(s, c).seen.version > 0,
    not(client(s, c).located and client(s, c).foundAt == lineageOf(s.bucket)),
  }

  pure def locateNext(s: State, c: str): State =
    withClient(s, c, {
      ...client(s, c),
      located: true,
      found: successorOf(s.bucket, client(s, c).seen.commit, client(s, c).seenLineage, lineageOf(s.bucket)),
      foundAt: lineageOf(s.bucket),
    })
}
