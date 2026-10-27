// -*- mode: Bluespec; -*-
/// The store's contract as scenarios every pail backend must follow, reply for reply
/// (`tests/store_refinement.rs`). No scenario writes an object a body it had before: there
/// backends differ (see `store.qnt`), and pail does not depend on which way.
module storeScenarios {
  import store.* from "./store"
  import storeSpec.* from "./store_spec"

  /// A create-if-absent creates once; the second keeps the first body.
  /// @conformance
  run createOnceRun = init
    .then(putNew("o/k", "a"))
    .then(putNew("o/k", "b"))
    .then(all {
      assert(state.reply == Created(false)),
      state' = state,
    })
    .then(getOpt("o/k"))
    .then(all {
      assert(state.reply == Found({ body: "a", version: 1 })),
      state' = state,
    })

  /// A swap needs the object's current version; a stale one conflicts.
  /// @conformance
  run swapNeedsTheCurrentVersionRun = init
    .then(casNew("o/k", "a"))
    .then(cas("o/k", 1, "b"))
    .then(cas("o/k", 1, "c"))
    .then(all {
      assert(state.reply == Conflict),
      state' = state,
    })
    .then(getOpt("o/k"))
    .then(all {
      assert(state.reply == Found({ body: "b", version: 2 })),
      state' = state,
    })

  /// A create-only swap conflicts with an object written any other way.
  /// @conformance
  run createOnlySwapConflictsRun = init
    .then(putAny("o/k", "a"))
    .then(casNew("o/k", "b"))
    .then(all {
      assert(state.reply == Conflict),
      state' = state,
    })

  /// Every write gives the object a new version, and a swap against an older one
  /// conflicts.
  /// @conformance
  run everyWriteIsANewVersionRun = init
    .then(putAny("o/k", "a"))
    .then(putAny("o/k", "b"))
    .then(putAny("o/k", "c"))
    .then(getOpt("o/k"))
    .then(all {
      assert(state.reply == Found({ body: "c", version: 3 })),
      state' = state,
    })
    .then(cas("o/k", 1, "d"))
    .then(all {
      assert(state.reply == Conflict),
      state' = state,
    })

  /// A deleted object is gone from reads and lists, a swap against its old version
  /// conflicts, and creating it again gives a new version.
  /// @conformance
  run deleteRun = init
    .then(putAny("o/k", "a"))
    .then(remove("o/k"))
    .then(getOpt("o/k"))
    .then(all {
      assert(state.reply == Missing),
      state' = state,
    })
    .then(listKeys)
    .then(cas("o/k", 1, "b"))
    .then(all {
      assert(state.reply == Conflict),
      state' = state,
    })
    .then(casNew("o/k", "c"))
    .then(all {
      assert(state.reply == Swapped(3)),
      state' = state,
    })

  /// A list shows every live object.
  /// @conformance
  run listRun = init
    .then(putAny("o/a", "1"))
    .then(putNew("o/b", "2"))
    .then(remove("o/missing"))
    .then(listKeys)
    .then(all {
      assert(state.reply == Listed(Set("o/a", "o/b"))),
      state' = state,
    })
}
