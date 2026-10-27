// -*- mode: Bluespec; -*-
/// Interleavings the real `Repo::commit` and `gc` must follow, request by request
/// (`tests/refinement.rs` runs each against a store that holds every request until the
/// scenario releases it).
module scenarios {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"
  import pail.* from "./pail"

  pure val ALICE_A: Delta = Map("a" -> "1")
  pure val BOB_B: Delta = Map("b" -> "1")
  pure val BOB_A: Delta = Map("a" -> "2")

  pure val ALICE_1: CommitId = ["root", "alice"]
  pure val BOB_1: CommitId = ["root", "bob"]
  pure val ALICE_2: CommitId = ["root", "bob", "alice"]

  /// Both read the root; bob lands first. Alice's CAS loses, she re-reads, finds bob's
  /// change disjoint from hers and lands on top of it.
  /// @conformance
  run disjointReplayRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(begin("bob", ROOT, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
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
        client(state, "alice").outcome == Published,
        client(state, "bob").outcome == Published,
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// Both write `a`; bob lands first. Alice's re-read finds her path changed under her and
  /// fails with a conflict, publishing nothing.
  /// @conformance
  run overlapConflictRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(begin("bob", ROOT, BOB_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
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
        client(state, "alice").outcome == Conflicted,
        branch(state.bucket) == Set(ROOT, BOB_1),
      }),
      state' = state,
    })

  /// The CAS lands but its answer is lost. Reading the ref shows the commit won.
  /// @conformance
  run timeoutAppliedRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Lost))
    .then(recheck("alice"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_1, version: 2 },
        client(state, "alice").outcome == Published,
      }),
      state' = state,
    })

  /// The CAS never reaches the store. The client retries and lands the same commit: same
  /// parent, same content, same name.
  /// @conformance
  run timeoutDroppedRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
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
        client(state, "alice").outcome == Published,
      }),
      state' = state,
    })

  /// Bob's CAS lands unanswered, and alice lands on top of it before bob looks. Bob's
  /// commit is in the history under alice's, so bob published.
  /// @conformance
  run timeoutLandedUnderRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(begin("bob", ROOT, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
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
        client(state, "bob").outcome == Published,
        onBranch(state.bucket, BOB_1),
      }),
      state' = state,
    })

  // ---------- gc ----------

  /// Alice never learns whether her commit landed (it did not), leaving it and its page
  /// unreferenced. Once they are old, gc deletes both.
  /// @conformance
  run gcCollectsAnAbandonedCommitRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Dropped))
    .then(recheck("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Dropped))
    .then(recheck("alice"))
    .then(graceElapses)
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcDeleteCommit(ALICE_1))
    .then(gcListPages)
    .then(gcDeletePage(Map("a" -> "1", "b" -> "0")))
    .then(gcDone)
    .then(all {
      assert(all {
        client(state, "alice").outcome == Unknown,
        liveKeys(state.bucket.commits) == Set(ROOT),
        liveKeys(state.bucket.pages) == Set(ROOT_TREE),
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// The race the lease closes. Alice abandons a commit whose page is old by the time bob
  /// writes the same tree: bob's upload of that page writes nothing, so the page stays old.
  /// gc starts between bob's upload and his swap, and waits for bob's lease; once bob has
  /// landed, it marks his commit, page included.
  /// @conformance
  run gcWaitsForAWriterReusingAnOldPageRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Dropped))
    .then(recheck("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Dropped))
    .then(recheck("alice"))
    .then(graceElapses)
    .then(begin("bob", ROOT, ALICE_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(gcLease)
    .then(gcCheckLeases)
    .then(all { assert(state.collector.phase == GcDraining), state' = state })
    .then(gcCheckLeases)
    .then(swap("bob", Answered))
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcDeleteCommit(ALICE_1))
    .then(gcListPages)
    .then(gcDone)
    .then(all {
      assert(all {
        client(state, "bob").outcome == Published,
        branchIsComplete,
        liveKeys(state.bucket.commits) == Set(ROOT, BOB_1),
      }),
      state' = state,
    })

  /// A writer that finds gc's lease drops its own and waits; gc, finding no writer, runs.
  /// @conformance
  run writerWaitsForGcRun = init
    .then(gcLease)
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(all {
      assert(client(state, "alice").phase == Leasing and not(isLive(state.bucket.locks, "alice"))),
      state' = state,
    })
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcListPages)
    .then(gcDone)
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(all {
      assert(all {
        client(state, "alice").outcome == Published,
        tip(state.bucket).commit == ALICE_1,
      }),
      state' = state,
    })
}
