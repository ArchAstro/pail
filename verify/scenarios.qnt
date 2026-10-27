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

  // ---------- rewrites ----------

  /// Alice's commit, reworded by carol: on the root, with alice's change.
  pure val ALICE_RW: CommitId = ["root", "alice", "~carol"]
  /// Bob's commit on alice's, carried by carol onto ALICE_RW.
  pure val BOB_ON_ALICE: CommitId = ["root", "alice", "bob"]
  pure val BOB_CARRIED: CommitId = ["root", "alice", "~carol", "bob", "~carol"]

  /// Carol rewords alice's commit while bob lands on top of it: carol's attempt finds the
  /// ref past her tip, carries bob's commit onto her new one, records the rewrite (epoch
  /// 1, mapping both) and swaps.
  /// @conformance
  run rewordCarriesRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(beginRewrite("carol"))
    .then(begin("bob", ALICE_1, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: BOB_CARRIED, version: 4 },
        lineageOf(state.bucket) == { epoch: 1, rewrite: 1 },
        recordOf(state.bucket, 1) == {
          old: BOB_ON_ALICE,
          new: BOB_CARRIED,
          epoch: 1,
          prev: 0,
          by: "carol",
          successors: Map(ALICE_1 -> ALICE_RW, BOB_ON_ALICE -> BOB_CARRIED),
        },
        treeOf(state.bucket, BOB_CARRIED) == Map("a" -> "1", "b" -> "1"),
        client(state, "carol").outcome == Rewritten,
        client(state, "bob").outcome == Published,
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// Alice's swap lands unanswered, and carol rewords alice's commit before alice looks.
  /// Alice's commit is gone from the branch, but the rewrite's record replaced a tip that
  /// had it: alice published. (Looking at the branch alone, alice retried, found her own
  /// change in the way and reported a conflict.)
  /// @conformance
  run rewordAfterLostSwapRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Lost))
    .then(beginRewrite("carol"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(recheck("alice"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_RW, version: 3 },
        not(onBranch(state.bucket, ALICE_1)),
        client(state, "alice").outcome == Published,
      }),
      state' = state,
    })

  /// Carol and dave both set out to reword alice's commit; carol lands first. Dave's
  /// attempt finds alice's commit gone from the branch: it moved, and dave publishes
  /// nothing (his new commit stays unreferenced).
  /// @conformance
  run rewriteMovedRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(beginRewrite("carol"))
    .then(beginRewrite("dave"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(rwStore("dave"))
    .then(rwRead("dave"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_RW, version: 3 },
        client(state, "dave").outcome == Moved,
        state.bucket.reflog.keys() == Set(1),
        isLive(state.bucket.commits, ["root", "alice", "~dave"]),
      }),
      state' = state,
    })

  /// Carol's rewrite swap lands unanswered, and dave rewrites the branch again before carol
  /// looks. The ref names dave's record, not carol's, but dave's leads back to it: carol
  /// rewrote. (Comparing the ref's lineage with her own alone, carol retried, found her
  /// tip gone from the branch and reported that it moved.)
  /// @conformance
  run rewriteLostThenRewrittenRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(beginRewrite("carol"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Lost))
    .then(beginRewrite("dave"))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(rwStore("dave"))
    .then(rwRead("dave"))
    .then(rwSwap("dave", Answered))
    .then(rwRecheck("carol"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ["root", "alice", "~dave"], version: 4 },
        lineageOf(state.bucket) == { epoch: 2, rewrite: 2 },
        recordOf(state.bucket, 2).prev == 1,
        client(state, "carol").outcome == Rewritten,
        client(state, "dave").outcome == Rewritten,
      }),
      state' = state,
    })

  /// Carol's rewrite swap never reaches the store. Her recheck finds the ref without her
  /// record, so she tries again: a second record (the first stays, naming nothing the
  /// ref does), and this swap lands.
  /// @conformance
  run rewriteSwapDroppedRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(beginRewrite("carol"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Dropped))
    .then(rwRecheck("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_RW, version: 3 },
        lineageOf(state.bucket) == { epoch: 1, rewrite: 2 },
        state.bucket.reflog.keys() == Set(1, 2),
        client(state, "carol").outcome == Rewritten,
      }),
      state' = state,
    })

  /// After carol rewords alice's commit, bob lands on the new history, keeping its lineage.
  /// Alice's commit is unreachable from the branch and old, but the rewrite's record names
  /// it: gc keeps it and its page.
  /// @conformance
  run gcKeepsRewrittenHistoryRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(beginRewrite("carol"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(begin("bob", ALICE_RW, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(graceElapses)
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcListPages)
    .then(gcDone)
    .then(all {
      assert(all {
        tip(state.bucket).commit == ALICE_RW.append("bob"),
        lineageOf(state.bucket) == { epoch: 1, rewrite: 1 },
        not(onBranch(state.bucket, ALICE_1)),
        liveKeys(state.bucket.commits) == Set(ROOT, ALICE_1, ALICE_RW, ALICE_RW.append("bob")),
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// Alice's intent lands in a round of her own (as in `combineAloneRun`), then dave
  /// rewords her commit. The rewrite keeps the ref's `recent` as it was: alice's intent
  /// still counts as landed (a retry finds it there), naming the commit it landed as.
  /// @conformance
  run rewordKeepsRecentRun = init
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
    .then(intend("alice", 1))
    .then(pollRef("alice"))
    .then(pollList("alice"))
    .then(gather("alice", Set()))
    .then(batchSwap("alice", Answered))
    .then(cleanup("alice"))
    .then(beginRewrite("dave"))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(rwStore("dave"))
    .then(rwRead("dave"))
    .then(rwSwap("dave", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == ALICE_C.append("~dave"),
        recentOf(state.bucket) == Map("alice" -> ALICE_C),
        lineageOf(state.bucket) == { epoch: 1, rewrite: 1 },
        client(state, "dave").outcome == Rewritten,
      }),
      state' = state,
    })
}
