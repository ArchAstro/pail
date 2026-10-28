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
          op: "rebase",
          old: BOB_ON_ALICE,
          new: BOB_CARRIED,
          epoch: 1,
          prev: 0,
          by: "carol",
          successors: Map(ALICE_1 -> ALICE_RW, BOB_ON_ALICE -> BOB_CARRIED),
          dropped: Set(),
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

  // ---------- undo and reset ----------

  pure val BOB_ON_RW: CommitId = ALICE_RW.append("bob")
  pure val BOB_UNDONE: CommitId = ["root", "alice", "bob", "~dave"]

  /// Carol rewords alice's commit; dave sets out to undo it, and bob lands on the reworded
  /// tip first. Dave's undo goes back to alice's commit and carries bob's onto it: its
  /// record maps carol's commit back to alice's, and bob's to its carried copy.
  /// @conformance
  run undoCarriesRun = init
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
    .then(beginUndo("dave"))
    .then(begin("bob", ALICE_RW, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(rwRead("dave"))
    .then(rwSwap("dave", Answered))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: BOB_UNDONE, version: 5 },
        lineageOf(state.bucket) == { epoch: 2, rewrite: 2 },
        recordOf(state.bucket, 2) == {
          op: "undo",
          old: BOB_ON_RW,
          new: BOB_UNDONE,
          epoch: 2,
          prev: 1,
          by: "dave",
          successors: Map(ALICE_RW -> ALICE_1, BOB_ON_RW -> BOB_UNDONE),
          dropped: Set(),
        },
        ancestorsOf(state.bucket, BOB_UNDONE) == Set(ROOT, ALICE_1),
        client(state, "dave").outcome == Rewritten,
        client(state, "bob").outcome == Published,
      }),
      state' = state,
    })

  /// Bob's swap lands unanswered; carol resets the branch to alice's commit, dropping
  /// bob's, before bob looks. The reset's record replaced a tip that had bob's commit: bob
  /// published. Dave then undoes the reset, and bob's commit is back.
  /// @conformance
  run resetAfterLostSwapRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", ALICE_1, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Lost))
    .then(beginReset("carol"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(recheck("bob"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_1, version: 4 },
        recordOf(state.bucket, 1) == {
          op: "reset",
          old: BOB_ON_ALICE,
          new: ALICE_1,
          epoch: 1,
          prev: 0,
          by: "carol",
          successors: Map(),
          dropped: Set(),
        },
        client(state, "bob").outcome == Published,
      }),
      state' = state,
    })
    .then(beginUndo("dave"))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(rwRead("dave"))
    .then(rwSwap("dave", Answered))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: BOB_ON_ALICE, version: 5 },
        lineageOf(state.bucket) == { epoch: 2, rewrite: 2 },
        recordOf(state.bucket, 2).op == "undo",
        client(state, "dave").outcome == Rewritten,
      }),
      state' = state,
    })

  /// Carol resets the branch to alice's commit, dropping bob's, while dave lands on bob's.
  /// Dave's change applies on alice's commit (it writes `a`, which bob left alone): carol
  /// carries it, writing it over alice's tree.
  /// @conformance
  run resetCarriesRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", ALICE_1, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(beginReset("carol"))
    .then(begin("dave", BOB_ON_ALICE, DAVE_A))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(readRef("dave"))
    .then(upload("dave"))
    .then(swap("dave", Answered))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == ["root", "alice", "dave", "~carol"],
        treeOf(state.bucket, ["root", "alice", "dave", "~carol"]) == Map("a" -> "2", "b" -> "0"),
        recordOf(state.bucket, 1).successors == Map(BOB_ON_ALICE.append("dave") -> ["root", "alice", "dave", "~carol"]),
        not(branch(state.bucket).exists(id => author(id) == "bob")),
        client(state, "carol").outcome == Rewritten,
      }),
      state' = state,
    })

  /// Carol sets out to reset the branch to the root, dropping alice's commit, while bob
  /// lands on it. Bob's change rewrites `a` from alice's content: on the root it does not
  /// apply, so carol's reset fails and publishes nothing.
  /// @conformance
  run resetConflictRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(beginReset("carol"))
    .then(begin("bob", ALICE_1, BOB_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwRead("carol"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: BOB_ON_ALICE, version: 3 },
        state.bucket.reflog.keys() == Set(),
        client(state, "carol").outcome == Conflicted,
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// Alice resets carol's commit off the branch; dave and bob both set out to undo the
  /// reset, and dave lands first. Bob's undo finds the branch rewritten since he read it:
  /// it moved, and he publishes nothing. (Comparing tips alone, bob found the reset's new
  /// tip still under the head, carried carol's commit onto itself, and landed her change
  /// twice.)
  /// @conformance
  run undoTwiceRun = init
    .then(begin("carol", ROOT, BOB_B))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(beginReset("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(rwRead("alice"))
    .then(rwSwap("alice", Answered))
    .then(beginUndo("dave"))
    .then(beginUndo("bob"))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(rwRead("dave"))
    .then(rwSwap("dave", Answered))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(rwRead("bob"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ["root", "carol"], version: 4 },
        lineageOf(state.bucket) == { epoch: 2, rewrite: 2 },
        state.bucket.reflog.keys() == Set(1, 2),
        client(state, "bob").outcome == Moved,
        client(state, "dave").outcome == Rewritten,
      }),
      state' = state,
    })

  // ---------- landing a stack ----------

  pure val ALICE_STACK: CommitId = ["root", "alice"]
  pure val ALICE_LANDED: CommitId = ["root", "carol", "alice", "~alice"]
  pure val CAROL_ON_ROOT: CommitId = ["root", "carol"]

  /// Alice stacks her commit on the root; carol publishes meanwhile, so alice's land copies
  /// the commit onto carol's. Its swap lands unanswered, and bob resets the branch,
  /// dropping the copy, before alice looks. The reset's record replaced a tip that had it:
  /// alice landed, and the reset stands. (Comparing the branch's tip with her copy alone,
  /// alice retried and landed her change again.)
  /// @conformance
  run landAfterResetRun = init
    .then(stack("alice", ALICE_A))
    .then(begin("carol", ROOT, BOB_B))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(beginLand("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(ldRead("alice"))
    .then(ldSwap("alice", Lost))
    .then(beginReset("bob"))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(rwRead("bob"))
    .then(rwSwap("bob", Answered))
    .then(ldRecheck("alice"))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: CAROL_ON_ROOT, version: 4 },
        client(state, "alice").outcome == Published,
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// Alice stacks her commit on the root and lands it while the branch is still there: a
  /// fast-forward, no copy. Carol's stack, also on the root, then lands as a copy onto
  /// alice's commit.
  /// @conformance
  run landFastForwardRun = init
    .then(stack("alice", ALICE_A))
    .then(stack("carol", BOB_B))
    .then(beginLand("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(ldRead("alice"))
    .then(ldSwap("alice", Answered))
    .then(beginLand("carol"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(ldRead("carol"))
    .then(ldSwap("carol", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == ["root", "alice", "carol", "~carol"],
        treeOf(state.bucket, ["root", "alice", "carol", "~carol"]) == Map("a" -> "1", "b" -> "1"),
        client(state, "alice").outcome == Published,
        client(state, "carol").outcome == Published,
      }),
      state' = state,
    })

  /// Alice stacks her commit on bob's; dave rewords bob's commit before
  /// alice lands. The merge base with the branch is the root, but bob's old commit (the
  /// reword's old tip) is a later one: alice's stack starts above it, so only her own
  /// commit is copied, onto the reworded one.
  /// @conformance
  run landAfterRewordRun = init
    .then(begin("bob", ROOT, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(stack("alice", ALICE_A))
    .then(beginRewrite("dave"))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(rwStore("dave"))
    .then(rwRead("dave"))
    .then(rwSwap("dave", Answered))
    .then(beginLand("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(ldRead("alice"))
    .then(ldSwap("alice", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == ["root", "bob", "~dave", "alice", "~alice"],
        ancestorsOf(state.bucket, tip(state.bucket).commit) == Set(ROOT, ["root", "bob", "~dave"]),
        client(state, "alice").outcome == Published,
      }),
      state' = state,
    })

  /// Alice stacks a change to `a` on the root; bob changes `a` on the branch first. Her
  /// land finds the path changed since her stack left the branch: a conflict, nothing
  /// landed.
  /// @conformance
  run landConflictRun = init
    .then(stack("alice", ALICE_A))
    .then(begin("bob", ROOT, BOB_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(beginLand("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(ldRead("alice"))
    .then(all {
      assert(all {
        tip(state.bucket).commit == BOB_1,
        client(state, "alice").outcome == Conflicted,
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// Alice's stack lands as a copy onto carol's commit; dave publishes on top; alice lands
  /// again. The branch has a copy of her commit above where her stack left it: nothing
  /// more lands.
  /// @conformance
  run landTwiceRun = init
    .then(stack("alice", ALICE_A))
    .then(begin("carol", ROOT, BOB_B))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(beginLand("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(ldRead("alice"))
    .then(ldSwap("alice", Answered))
    .then(begin("dave", ALICE_LANDED, DAVE_B))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(readRef("dave"))
    .then(upload("dave"))
    .then(swap("dave", Answered))
    .then(beginLand("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(ldRead("alice"))
    .then(all {
      assert(all {
        tip(state.bucket).commit == ALICE_LANDED.append("dave"),
        client(state, "alice").outcome == Published,
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// Alice stacks her commit on bob's; carol resets the branch, dropping bob's commit,
  /// before alice lands. The reset's old tip is where alice's stack left the branch: only
  /// her own commit lands, onto the root. (From the merge base alone, the root, bob's
  /// commit counted as part of her stack and landed again.)
  /// @conformance
  run landAfterResetDropRun = init
    .then(begin("bob", ROOT, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(stack("alice", ALICE_A))
    .then(beginReset("carol"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(beginLand("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(ldRead("alice"))
    .then(ldSwap("alice", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == ["root", "alice", "~alice"],
        not(branch(state.bucket).exists(id => author(id) == "bob")),
        client(state, "alice").outcome == Published,
      }),
      state' = state,
    })

  // ---------- rebase plans ----------

  pure val BOB_PICKED: CommitId = ["root", "bob", "~carol"]

  /// Carol's plan drops alice's commit from under bob's: bob's change, picked onto the
  /// root, applies (it writes `b`, which alice left alone). Dave lands on bob's commit
  /// meanwhile, and carol carries his on top. The record maps both and names alice's as
  /// dropped; her change is off the branch on purpose.
  /// @conformance
  run rebaseDropParentRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", ALICE_1, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(beginRebase("carol", "dropParent"))
    .then(begin("dave", BOB_ON_ALICE, DAVE_B))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(readRef("dave"))
    .then(upload("dave"))
    .then(swap("dave", Answered))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == BOB_PICKED.append("dave").append("~carol"),
        treeOf(state.bucket, BOB_PICKED) == Map("a" -> "0", "b" -> "1"),
        recordOf(state.bucket, 1) == {
          op: "rebase",
          old: BOB_ON_ALICE.append("dave"),
          new: BOB_PICKED.append("dave").append("~carol"),
          epoch: 1,
          prev: 0,
          by: "carol",
          successors: Map(
            BOB_ON_ALICE -> BOB_PICKED,
            BOB_ON_ALICE.append("dave") -> BOB_PICKED.append("dave").append("~carol"),
          ),
          dropped: Set(ALICE_1),
        },
        not(branch(state.bucket).exists(id => author(id) == "alice")),
        client(state, "alice").outcome == Published,
        client(state, "carol").outcome == Rewritten,
      }),
      state' = state,
    })

  /// Carol's plan keeps alice's commit and drops bob's on top: alice's is reused as it
  /// is, so nothing is stored before the swap.
  /// @conformance
  run rebaseDropTipRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", ALICE_1, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(beginRebase("carol", "dropTip"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == ALICE_1,
        recordOf(state.bucket, 1).successors == Map(),
        recordOf(state.bucket, 1).dropped == Set(BOB_ON_ALICE),
        client(state, "carol").outcome == Rewritten,
      }),
      state' = state,
    })

  /// Carol's plan swaps alice's and bob's commits: bob's picked onto the root, alice's on
  /// top of it.
  /// @conformance
  run rebaseReorderRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", ALICE_1, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(beginRebase("carol", "reorder"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == BOB_PICKED.append("alice").append("~carol"),
        treeOf(state.bucket, tip(state.bucket).commit) == Map("a" -> "1", "b" -> "1"),
        recordOf(state.bucket, 1).successors == Map(
          BOB_ON_ALICE -> BOB_PICKED,
          ALICE_1 -> BOB_PICKED.append("alice").append("~carol"),
        ),
        recordOf(state.bucket, 1).dropped == Set(),
      }),
      state' = state,
    })

  /// Bob's commit rewrites `a` from alice's content; carol's plan drops alice's from under
  /// it. On the root bob's change does not apply: a conflict before carol leases, nothing
  /// stored or published.
  /// @conformance
  run rebaseConflictRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", ALICE_1, BOB_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(beginRebase("carol", "dropParent"))
    .then(all {
      assert(all {
        tip(state.bucket).commit == BOB_ON_ALICE,
        state.bucket.reflog.keys() == Set(),
        client(state, "carol").outcome == Conflicted,
        not(state.bucket.locks.keys().contains("carol")),
      }),
      state' = state,
    })

  /// Carol sets `b`; alice changes `a` on top; bob writes `b` as carol left it, a commit
  /// that changes nothing. Dave's plan drops alice's commit from under bob's: bob's, on
  /// carol's, would change nothing either, so it is dropped as empty too. The branch is
  /// back at carol's commit.
  /// @conformance
  run rebaseEmptyRun = init
    .then(begin("carol", ROOT, BOB_B))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(begin("alice", CAROL_ON_ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", CAROL_ON_ROOT.append("alice"), BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(beginRebase("dave", "dropParent"))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(rwRead("dave"))
    .then(rwSwap("dave", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == CAROL_ON_ROOT,
        recordOf(state.bucket, 1).dropped == Set(CAROL_ON_ROOT.append("alice"), CAROL_ON_ROOT.append("alice").append("bob")),
        recordOf(state.bucket, 1).successors == Map(),
        client(state, "dave").outcome == Rewritten,
      }),
      state' = state,
    })

  // ---------- finding a commit after rewrites ----------

  /// Carol's plan drops alice's commit from under bob's while dave lands on bob's. Bob,
  /// who built on alice's commit, finds it dropped: its nearest kept ancestor, the root.
  /// Dave, who built on bob's, finds bob's picked copy.
  /// @conformance
  run locateAfterDropRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", ALICE_1, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(beginRebase("carol", "dropParent"))
    .then(begin("dave", BOB_ON_ALICE, DAVE_B))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(readRef("dave"))
    .then(upload("dave"))
    .then(swap("dave", Answered))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(locate("bob"))
    .then(locate("dave"))
    .then(all {
      assert(all {
        client(state, "bob").found == ROOT,
        client(state, "dave").found == BOB_PICKED,
        client(state, "dave").foundAt == { epoch: 1, rewrite: 1 },
      }),
      state' = state,
    })

  /// Carol rewords alice's commit while bob lands on it; bob finds it reworded. Dave
  /// undoes the reword, and bob, asking again, walks both records, oldest first, back to
  /// alice's commit.
  /// @conformance
  run locateThroughUndoRun = init
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
    .then(locate("bob"))
    .then(all {
      assert(client(state, "bob").found == ALICE_RW),
      state' = state,
    })
    .then(beginUndo("dave"))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(rwRead("dave"))
    .then(rwSwap("dave", Answered))
    .then(locate("bob"))
    .then(all {
      assert(all {
        lineageOf(state.bucket) == { epoch: 2, rewrite: 2 },
        client(state, "bob").found == ALICE_1,
      }),
      state' = state,
    })

  /// Carol rewords alice's commit while bob lands on it, then dave rewords the tip (bob's
  /// carried commit). Bob walks both records: carol's maps alice's commit to its reword,
  /// which dave's keeps. (Walking dave's alone, alice's commit is in neither history, and
  /// the walk falls back to the root.)
  /// @conformance
  run locateThroughRewordsRun = init
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
    .then(beginRewrite("dave"))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(rwStore("dave"))
    .then(rwRead("dave"))
    .then(rwSwap("dave", Answered))
    .then(locate("bob"))
    .then(all {
      assert(all {
        lineageOf(state.bucket) == { epoch: 2, rewrite: 2 },
        client(state, "bob").found == ALICE_RW,
      }),
      state' = state,
    })

  /// Dave reads bob's commit on alice's (his own change to `a` conflicts), then carol's plan
  /// swaps the two. Bob's commit maps to its pick on the root, which lacks alice's change;
  /// dave finds the place above it that has both, alice's moved commit.
  /// @conformance
  run locateThroughReorderRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", ALICE_1, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(begin("dave", ROOT, DAVE_A))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(readRef("dave"))
    .then(all {
      assert(all {
        client(state, "dave").phase == Done,
        client(state, "dave").seen.commit == BOB_ON_ALICE,
      }),
      state' = state,
    })
    .then(beginRebase("carol", "reorder"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(locate("dave"))
    .then(all {
      assert(client(state, "dave").found == BOB_PICKED.append("alice").append("~carol")),
      state' = state,
    })
}
