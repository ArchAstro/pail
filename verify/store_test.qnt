// -*- mode: Bluespec; -*-
/// The store's contract, exercised on one space of integers (plain Quint tests).
module storeTest {
  import store.* from "./store"

  var objects: Space[str, int]

  action init = objects' = Map()

  run createThenConflictRun = init
    .then(objects' = created(objects, "k", 1))
    .then(all {
      assert(not(createApplies(objects, "k"))),
      assert(created(objects, "k", 2) == objects),
      objects' = objects,
    })

  run swapNeedsTheCurrentVersionRun = init
    .then(objects' = created(objects, "k", 1))
    .then(objects' = swapped(objects, "k", 1, 2))
    .then(all {
      assert(body(objects, "k") == 2),
      assert(versionOf(objects, "k") == 2),
      assert(swapped(objects, "k", 1, 3) == objects),
      objects' = objects,
    })

  /// After a delete and a re-create, a swap against the first version must not apply (no
  /// ABA): versions are never reused.
  run versionsAreNeverReusedRun = init
    .then(objects' = created(objects, "k", 1))
    .then(objects' = deleted(objects, "k"))
    .then(objects' = created(objects, "k", 1))
    .then(all {
      assert(versionOf(objects, "k") == 3),
      assert(not(casApplies(objects, "k", 1))),
      assert(liveKeys(objects) == Set("k")),
      objects' = objects,
    })

  run deleteHidesTheObjectRun = init
    .then(objects' = created(objects, "k", 1))
    .then(objects' = deleted(objects, "k"))
    .then(all {
      assert(not(isLive(objects, "k"))),
      assert(liveKeys(objects) == Set()),
      assert(not(casApplies(objects, "k", 2))),
      objects' = objects,
    })
}
