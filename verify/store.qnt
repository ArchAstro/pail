// -*- mode: Bluespec; -*-
/// An object store as pail uses one (S3, GCS, a local directory), independent of pail.
///
/// A store is a set of named objects. Each write gives an object a new version (an ETag or
/// GCS generation) that is never reused, even after a delete, so a conditional write that
/// matched a version matched exactly that write. Every operation is linearizable: S3 and
/// GCS are strongly consistent for reads, writes and lists.
///
/// The store does not know what objects mean. A model of a client lays out its objects in
/// one or more spaces, each a typed map from key to versioned object (pail's are in
/// `bucket.qnt`), and calls these operators on them.
///
/// A request can fail in transit (`Delivery`): the store may have performed it or not,
/// and the client cannot tell which.
module store {
  /// One object, or the tombstone a delete leaves (so its version is never reused).
  type Versioned[b] = { body: b, version: int, live: bool }

  /// A namespace of objects (`commits/`, `refs/`, ...) with keys of type `k`.
  type Space[k, b] = k -> Versioned[b]

  /// What became of a request: answered; performed but its reply lost; or never received.
  type Delivery = Answered | Lost | Dropped

  /// Whether the store received the request (and so performed it, if it applied).
  pure def received(d: Delivery): bool = d != Dropped

  /// Whether the client learns the result.
  pure def answered(d: Delivery): bool = d == Answered

  // ---------- reads ----------

  pure def isLive(s: Space[k, b], key: k): bool =
    s.keys().contains(key) and s.get(key).live

  /// The object's body. Only meaningful when it `isLive`.
  pure def body(s: Space[k, b], key: k): b = s.get(key).body

  /// The object's current version; 0 if it was never written. A deleted object keeps the
  /// version of its delete.
  pure def versionOf(s: Space[k, b], key: k): int =
    if (s.keys().contains(key)) s.get(key).version else 0

  /// The keys of every live object (a LIST).
  pure def liveKeys(s: Space[k, b]): Set[k] = s.keys().filter(key => s.get(key).live)

  // ---------- writes ----------

  /// An unconditional PUT.
  pure def written(s: Space[k, b], key: k, data: b): Space[k, b] =
    s.put(key, { body: data, version: versionOf(s, key) + 1, live: true })

  /// Whether a create-if-absent (If-None-Match: *) applies.
  pure def createApplies(s: Space[k, b], key: k): bool = not(isLive(s, key))

  /// A create-if-absent: unchanged if the object exists.
  pure def created(s: Space[k, b], key: k, data: b): Space[k, b] =
    if (createApplies(s, key)) written(s, key, data) else s

  /// Whether a write conditional on `version` (If-Match) applies.
  pure def casApplies(s: Space[k, b], key: k, version: int): bool =
    isLive(s, key) and versionOf(s, key) == version

  /// A write conditional on `version`: unchanged if the object moved on.
  pure def swapped(s: Space[k, b], key: k, version: int, data: b): Space[k, b] =
    if (casApplies(s, key, version)) written(s, key, data) else s

  /// A DELETE: the object goes, its version stays used.
  pure def deleted(s: Space[k, b], key: k): Space[k, b] =
    if (isLive(s, key))
      s.put(key, { ...s.get(key), version: versionOf(s, key) + 1, live: false })
    else s
}
