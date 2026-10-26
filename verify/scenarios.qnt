// -*- mode: Bluespec; -*-
/// Interleavings the real `Repo::commit` must follow, request by request
/// (`tests/refinement.rs` runs each against a store that holds every request until the
/// scenario releases it).
module scenarios {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import publish.* from "./publish"

  pure val ALICE_A: Delta = Map("a" -> "1")
  pure val BOB_B: Delta = Map("b" -> "1")
  pure val BOB_A: Delta = Map("a" -> "2")

  pure val ALICE_1: CommitId = { by: "alice", gen: 1 }
  pure val ALICE_2: CommitId = { by: "alice", gen: 2 }
  pure val BOB_1: CommitId = { by: "bob", gen: 1 }

  /// Both read the root; bob lands first. Alice's CAS loses, she re-reads, finds bob's
  /// change disjoint from hers and lands on top of it.
  /// @conformance
  run disjointReplayRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(begin("bob", ROOT, BOB_B))
    .then(readRef("alice"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_2, version: 3 },
        treeOf(state.bucket, ALICE_2) == Map("a" -> "1", "b" -> "1"),
        state.clients.get("alice").outcome == Published,
        state.clients.get("bob").outcome == Published,
      }),
      state' = state,
    })

  /// Both write `a`; bob lands first. Alice's re-read finds her path changed under her and
  /// fails with a conflict, publishing nothing.
  /// @conformance
  run overlapConflictRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(begin("bob", ROOT, BOB_A))
    .then(readRef("alice"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(readRef("alice"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: BOB_1, version: 2 },
        state.clients.get("alice").outcome == Conflicted,
        branch(state.bucket) == Set(ROOT, BOB_1),
      }),
      state' = state,
    })

  /// The CAS lands but its answer is lost. Reading the ref shows the commit won.
  /// @conformance
  run timeoutAppliedRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Lost))
    .then(recheck("alice"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_1, version: 2 },
        state.clients.get("alice").outcome == Published,
      }),
      state' = state,
    })

  /// The CAS never reaches the store. The client retries and lands the same commit: same
  /// parent, same content, same name.
  /// @conformance
  run timeoutDroppedRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Dropped))
    .then(recheck("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_1, version: 2 },
        liveKeys(state.bucket.commits) == Set(ROOT, ALICE_1),
        state.clients.get("alice").outcome == Published,
      }),
      state' = state,
    })

  /// Bob's CAS lands unanswered, and alice lands on top of it before bob looks. Bob's
  /// commit is in the history under alice's, so bob published.
  /// @conformance
  run timeoutLandedUnderRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(begin("bob", ROOT, BOB_B))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Lost))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(recheck("bob"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_2, version: 3 },
        state.clients.get("bob").outcome == Published,
        onBranch(state.bucket, BOB_1),
      }),
      state' = state,
    })
}
