// -*- mode: Bluespec; -*-
/// Interleavings the real `Repo::commit` and `gc` must follow, request by request
/// (`tests/refinement.rs` runs each against a store that holds every request until the
/// scenario releases it).
module scenarios {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"
  import combine.* from "./combine"
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

  // ---------- combining ----------

  pure val CAROL_B: Delta = Map("b" -> "2")
  pure val DAVE_B: Delta = Map("b" -> "3")
  pure val DAVE_A: Delta = Map("a" -> "2")

  pure val CAROL_1: CommitId = ["root", "bob", "carol"]
  /// Alice's change as a combining round lands it.
  pure val ALICE_C: CommitId = ["root", "bob", "carol", "alice+"]

  /// Alice loses to bob, then to carol, and combines: her intent, alone in its batch,
  /// lands on carol's commit.
  /// @conformance
  run combineAloneRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(begin("bob", ROOT, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(readRef("alice"))
    .then(begin("carol", BOB_1, CAROL_B))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(all { assert(client(state, "alice").phase == Intending), state' = state })
    .then(intend("alice", 1))
    .then(pollRef("alice"))
    .then(pollList("alice"))
    .then(gather("alice", Set()))
    .then(batchSwap("alice", Answered))
    .then(cleanup("alice"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_C, version: 4 },
        recentOf(state.bucket) == Map("alice" -> ALICE_C),
        treeOf(state.bucket, ALICE_C) == Map("a" -> "1", "b" -> "2"),
        liveKeys(state.bucket.intents) == Set(),
        client(state, "alice").outcome == Published,
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// Dave's upload finds alice's intent pending and joins her instead of swapping. Not the
  /// oldest, dave waits; alice lands both intents in one swap, and dave finds his landed.
  /// @conformance
  run joinedBatchRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(begin("bob", ROOT, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(readRef("alice"))
    .then(begin("carol", BOB_1, CAROL_B))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(all { assert(client(state, "alice").phase == Intending), state' = state })
    .then(intend("alice", 1))
    .then(begin("dave", CAROL_1, DAVE_B))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(readRef("dave"))
    .then(upload("dave"))
    .then(all { assert(client(state, "dave").phase == Intending), state' = state })
    .then(intend("dave", 2))
    .then(pollRef("dave"))
    .then(pollList("dave"))
    .then(wait("dave"))
    .then(pollRef("alice"))
    .then(pollList("alice"))
    .then(gather("alice", Set()))
    .then(batchSwap("alice", Answered))
    .then(cleanup("alice"))
    .then(pollList("dave"))
    .then(pollRef("dave"))
    .then(all { assert(client(state, "dave").phase == Withdrawing), state' = state })
    .then(withdraw("dave"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_C.append("dave+"), version: 4 },
        recentOf(state.bucket) == Map("alice" -> ALICE_C, "dave" -> ALICE_C.append("dave+")),
        treeOf(state.bucket, ALICE_C.append("dave+")) == Map("a" -> "1", "b" -> "3"),
        liveKeys(state.bucket.intents) == Set(),
        client(state, "alice").outcome == Published,
        client(state, "dave").outcome == Published,
      }),
      state' = state,
    })

  /// Dave's intent writes `a` too. Alice's round lands hers first, so skips dave's; dave
  /// finds `a` changed since his base and reports the conflict, having published nothing.
  /// @conformance
  run conflictSkippedRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(begin("bob", ROOT, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(readRef("alice"))
    .then(begin("carol", BOB_1, CAROL_B))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(all { assert(client(state, "alice").phase == Intending), state' = state })
    .then(intend("alice", 1))
    .then(begin("dave", CAROL_1, DAVE_A))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(readRef("dave"))
    .then(upload("dave"))
    .then(all { assert(client(state, "dave").phase == Intending), state' = state })
    .then(intend("dave", 2))
    .then(pollRef("alice"))
    .then(pollList("alice"))
    .then(gather("alice", Set()))
    .then(batchSwap("alice", Answered))
    .then(cleanup("alice"))
    .then(pollRef("dave"))
    .then(pollList("dave"))
    .then(withdraw("dave"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_C, version: 4 },
        recentOf(state.bucket) == Map("alice" -> ALICE_C),
        liveKeys(state.bucket.intents) == Set(),
        client(state, "dave").outcome == Conflicted,
      }),
      state' = state,
    })

  /// Alice's batch swap lands but its reply is lost. Reading the ref shows her batch at
  /// the tip, her intent in `recent`: published, with her intent left for later cleanup.
  /// @conformance
  run batchSwapLostRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(begin("bob", ROOT, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(readRef("alice"))
    .then(begin("carol", BOB_1, CAROL_B))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(all { assert(client(state, "alice").phase == Intending), state' = state })
    .then(intend("alice", 1))
    .then(pollRef("alice"))
    .then(pollList("alice"))
    .then(gather("alice", Set()))
    .then(batchSwap("alice", Lost))
    .then(batchRecheck("alice"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_C, version: 4 },
        recentOf(state.bucket) == Map("alice" -> ALICE_C),
        liveKeys(state.bucket.intents) == Set("alice"),
        client(state, "alice").outcome == Published,
      }),
      state' = state,
    })
}
