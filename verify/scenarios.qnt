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
        client(state, "alice").known.head == { commit: ALICE_1, version: 2 },
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
        // The ref's version is alice's commit's, not bob's: bob keeps no known head.
        client(state, "bob").known == NO_KNOWN,
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
    .then(gcDeletePage({ tree: Map("a" -> "1", "b" -> "0"), locs: Map() }))
    .then(gcListPacks)
    .then(gcListChunks)
    .then(gcDone)
    .then(all {
      assert(all {
        client(state, "alice").outcome == Unknown,
        liveKeys(state.bucket.commits) == Set(ROOT),
        liveKeys(state.bucket.pages) == Set(ROOT_PAGE),
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
    .then(gcListPacks)
    .then(gcListChunks)
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
    .then(gcListPacks)
    .then(gcListChunks)
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
        // Alice's commit is under dave's in the batch: only dave keeps a known head.
        client(state, "alice").known == NO_KNOWN,
        client(state, "dave").known.head == { commit: ALICE_C.append("dave+"), version: 4 },
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
    .then(gcListPacks)
    .then(gcListChunks)
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
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(createBranch("alice", Answered))
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
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(createBranch("alice", Answered))
    .then(stack("alice", ALICE_A))
    .then(beginBranch("carol"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(createBranch("carol", Answered))
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
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(createBranch("alice", Answered))
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
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(createBranch("alice", Answered))
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
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(createBranch("alice", Answered))
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
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(createBranch("alice", Answered))
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

  // ---------- known heads ----------

  /// Bob commits on the head alice's publish left her (`KnownHead`): his first attempt
  /// swaps from it without reading the ref.
  /// @conformance
  run knownHeadRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(beginKnown("bob", "alice", BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(all { assert(client(state, "bob").phase == Uploading), state' = state })
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_1.append("bob"), version: 3 },
        client(state, "bob").outcome == Published,
        client(state, "bob").attempt == 1,
        client(state, "bob").known == {
          has: true,
          head: { commit: ALICE_1.append("bob"), version: 3 },
          recent: Map(),
          lineage: NO_LINEAGE,
        },
      }),
      state' = state,
    })

  /// Carol lands after alice, so the head alice's publish left her is stale: bob's swap
  /// from it loses, and his second attempt reads the ref and replays onto carol's.
  /// @conformance
  run knownHeadStaleRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("carol", ALICE_1, CAROL_B))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(beginKnown("bob", "alice", BOB_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(all { assert(client(state, "bob").phase == Reading), state' = state })
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: ALICE_1.append("carol").append("bob"), version: 4 },
        treeOf(state.bucket, ALICE_1.append("carol").append("bob")) == Map("a" -> "2", "b" -> "2"),
        client(state, "bob").outcome == Published,
        client(state, "bob").attempt == 2,
      }),
      state' = state,
    })

  /// Alice combines alone (as in `combineAloneRun`); her commit is the batch's tip, so her
  /// publish leaves her a known head, with the ref's `recent`. Dave commits on it: his
  /// swap from it carries that `recent` forward.
  /// @conformance
  run knownHeadAfterCombineRun = init
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
    .then(beginKnown("dave", "alice", DAVE_A))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(upload("dave"))
    .then(swap("dave", Answered))
    .then(all {
      assert(all {
        client(state, "alice").known == {
          has: true,
          head: { commit: ALICE_C, version: 4 },
          recent: Map("alice" -> ALICE_C),
          lineage: NO_LINEAGE,
        },
        tip(state.bucket) == { commit: ALICE_C.append("dave"), version: 5 },
        recentOf(state.bucket) == Map("alice" -> ALICE_C),
        client(state, "dave").outcome == Published,
        client(state, "dave").attempt == 1,
      }),
      state' = state,
    })

  /// Carol rewords alice's commit, so the branch is at a new lineage; dave lands on the
  /// reword and keeps that lineage in his known head. Bob commits on it: his swap from it
  /// carries the lineage forward, as a read would have.
  /// @conformance
  run knownHeadAfterRewriteRun = init
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
    .then(begin("dave", ALICE_RW, DAVE_B))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(readRef("dave"))
    .then(upload("dave"))
    .then(swap("dave", Answered))
    .then(beginKnown("bob", "dave", BOB_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(all {
      assert(all {
        client(state, "dave").known.lineage == { epoch: 1, rewrite: 1 },
        tip(state.bucket).commit == ALICE_RW.append("dave").append("bob"),
        lineageOf(state.bucket) == { epoch: 1, rewrite: 1 },
        client(state, "bob").outcome == Published,
        client(state, "bob").attempt == 1,
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

  // ---------- squash and fixup ----------

  /// Carol's plan melds bob's commit into alice's (`squash`/`fixup`): one commit on the
  /// root with both changes, each one's successor; nothing dropped.
  /// @conformance
  run rebaseSquashRun = init
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
    .then(beginRebase("carol", "squash"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == ["root", "bob", "&", "alice", "~carol"],
        treeOf(state.bucket, tip(state.bucket).commit) == Map("a" -> "1", "b" -> "1"),
        authorsOf(tip(state.bucket).commit) == Set("alice", "bob"),
        recordOf(state.bucket, 1).successors == Map(
          ALICE_1 -> ["root", "bob", "&", "alice", "~carol"],
          BOB_ON_ALICE -> ["root", "bob", "&", "alice", "~carol"],
        ),
        recordOf(state.bucket, 1).dropped == Set(),
        client(state, "carol").outcome == Rewritten,
      }),
      state' = state,
    })


  /// A fixup moved down: carol's plan melds alice's commit into bob's, which goes first.
  /// Bob's change is to `b`, so it applies on the root; the one commit has both.
  /// @conformance
  run rebaseSquashUnderRun = init
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
    .then(beginRebase("carol", "squashUnder"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == ["root", "alice", "&", "bob", "~carol"],
        treeOf(state.bucket, tip(state.bucket).commit) == Map("a" -> "1", "b" -> "1"),
        recordOf(state.bucket, 1).successors == Map(
          ALICE_1 -> ["root", "alice", "&", "bob", "~carol"],
          BOB_ON_ALICE -> ["root", "alice", "&", "bob", "~carol"],
        ),
      }),
      state' = state,
    })


  /// Bob's commit rewrites `a` from alice's content: melded into alice's it applies (her
  /// change comes first in the group), but led by it on the root it does not, a conflict
  /// before carol leases.
  /// @conformance
  run rebaseSquashConflictRun = init
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
    .then(beginRebase("carol", "squashUnder"))
    .then(all {
      assert(all {
        tip(state.bucket).commit == BOB_ON_ALICE,
        client(state, "carol").outcome == Conflicted,
        client(state, "carol").phase == Done,
      }),
      state' = state,
    })


  /// The same commits squashed in order apply: alice's `a` first, then bob's over it.
  /// @conformance
  run rebaseSquashOverwritesRun = init
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
    .then(beginRebase("carol", "squash"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == ["root", "bob", "&", "alice", "~carol"],
        treeOf(state.bucket, tip(state.bucket).commit) == Map("a" -> "2", "b" -> "0"),
      }),
      state' = state,
    })

  /// Bob's commit puts `a` back as the root had it.
  pure val BOB_REVERTS_A: Delta = Map("a" -> "0")

  /// Carol's plan melds bob's revert into alice's commit: together they change nothing,
  /// so both are dropped and the branch is the root again.
  /// @conformance
  run rebaseSquashEmptyRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", ALICE_1, BOB_REVERTS_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(beginRebase("carol", "squash"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(all {
      assert(all {
        tip(state.bucket).commit == ROOT,
        recordOf(state.bucket, 1).successors == Map(),
        recordOf(state.bucket, 1).dropped == Set(ALICE_1, BOB_ON_ALICE),
        client(state, "carol").outcome == Rewritten,
      }),
      state' = state,
    })

  // ---------- lease timing ----------
  //
  // Time is `tick`s (`bucket.qnt`: `TTL` 6, a refresh every 2, a check at 3). A holder
  // whose refreshes never land (a stall: the requests are left held) goes stale: to the
  // others when its lease is `TTL` old by their clock, `SKEW` ahead of the store's, to
  // itself when `Lease::check` finds it more than `MAX_AGE` old.

  /// A holder that stopped refreshing is ignored once its lease is `TTL` old by the
  /// lister's clock (a tick ahead of the store's): at tick 5 gc still waits for bob, at
  /// tick 6 it does not.
  /// @conformance
  run expiredLeaseIsIgnoredRun = init
    .then(begin("bob", ROOT, BOB_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(gcLease)
    .then(gcCheckLeases)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(gcCheckLeases)
    .then(all { assert(state.collector.phase == GcDraining), state' = state })
    .then(tick)
    .then(gcCheckLeases)
    .then(all {
      assert(all {
        state.collector.phase == GcLocked,
        isLive(state.bucket.locks, "bob"),
        client(state, "bob").phase == Reading,
      }),
      state' = state,
    })
    .then(gcMark)
    .then(gcListCommits)
    .then(gcListPages)
    .then(gcListPacks)
    .then(gcListChunks)
    .then(gcDone)

  /// A writer whose refreshes never land finds its lease too old (tick 4, more than
  /// `MAX_AGE`) and refuses its swap: the swap never leaves, as a dropped one. It rechecks,
  /// retries, refuses again and fails, publishing nothing.
  /// @conformance
  run staleWriterRefusesSwapRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(upload("alice"))
    .then(all { assert(client(state, "alice").phase == Rechecking), state' = state })
    .then(recheck("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(recheck("alice"))
    .then(all {
      assert(all {
        client(state, "alice").outcome == Unknown,
        tip(state.bucket) == { commit: ROOT, version: 1 },
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// A lease counts from when its put started, not when the put (or the listing after it)
  /// returned: alice's put takes two ticks to land, so at tick 4 she is 4 old, not 2.
  /// @conformance
  run leaseCountsFromItsPutRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(tick)
    .then(tick)
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(all { assert(state.timing.get("alice").refreshed == 0), state' = state })
    .then(readRef("alice"))
    .then(tick)
    .then(tick)
    .then(upload("alice"))
    .then(all {
      assert(all {
        client(state, "alice").phase == Rechecking,
        tip(state.bucket) == { commit: ROOT, version: 1 },
      }),
      state' = state,
    })

  /// A swap is allowed at exactly `MAX_AGE`: alice's refresh landed at tick 2 (it counts
  /// from tick 2, when it was sent), so at tick 5 she is 3 old and passes; her swap lands
  /// a tick later, within `LATENCY` of the check.
  /// @conformance
  run swapAtTheEdgeOfTheMarginRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(tick)
    .then(tick)
    .then(refreshLand("alice", Answered))
    .then(refreshAck("alice"))
    .then(tick)
    .then(tick)
    .then(tick)
    .then(upload("alice"))
    .then(all {
      assert(all {
        client(state, "alice").phase == Swapping,
        state.timing.get("alice").refreshed == 2,
      }),
      state' = state,
    })
    .then(tick)
    .then(swap("alice", Answered))
    .then(all {
      assert(all {
        client(state, "alice").outcome == Published,
        tip(state.bucket) == { commit: ALICE_1, version: 2 },
      }),
      state' = state,
    })

  /// A refresh counts from when it was sent, not when its reply came back: alice's, sent
  /// at tick 2, lands at once but is answered at tick 3. At tick 6 she is 4 old (3 if the
  /// reply's time counted) and refuses.
  /// @conformance
  run refreshCountsFromItsSendRun = init
    .then(begin("alice", ROOT, ALICE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(tick)
    .then(tick)
    .then(refreshLand("alice", Answered))
    .then(tick)
    .then(refreshAck("alice"))
    .then(all { assert(state.timing.get("alice").refreshed == 2), state' = state })
    .then(tick)
    .then(tick)
    .then(tick)
    .then(upload("alice"))
    .then(all {
      assert(all {
        client(state, "alice").phase == Rechecking,
        tip(state.bucket) == { commit: ROOT, version: 1 },
      }),
      state' = state,
    })

  /// gc holds its lease from the start but stalls: its refresh never lands, so by tick 6
  /// the writers cannot see it, and alice makes a stack. gc then lists it as garbage (it
  /// marked before) and, finding its own lease too old, deletes nothing and gives up.
  /// Without the check gc deleted the stack's commit under alice's ref.
  /// @conformance
  run staleGcRefusesToDeleteRun = init
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(createBranch("alice", Answered))
    .then(stack("alice", ALICE_A))
    .then(graceElapses)
    .then(gcListCommits)
    .then(all {
      assert(all {
        state.collector.phase == GcSweepingChunks,
        state.collector.doomedCommits == Set(),
        isLive(state.bucket.commits, ALICE_1),
      }),
      state' = state,
    })
    .then(gcDone)
    .then(all {
      assert(all {
        branchIsComplete,
        state.collector.phase == GcIdle,
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// gc waits four ticks for its listing, so its lease is old when it holds it. It counts
  /// from when the lease was written (tick 0), not from the listing (tick 4), or it would
  /// pass its check at tick 6, when writers no longer see its lease.
  /// @conformance
  run gcLeaseCountsFromItsWriteRun = init
    .then(gcLease)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(gcCheckLeases)
    .then(all { assert(state.timing.get(GC).refreshed == 0), state' = state })
    .then(gcMark)
    .then(tick)
    .then(tick)
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(createBranch("alice", Answered))
    .then(stack("alice", ALICE_A))
    .then(graceElapses)
    .then(gcListCommits)
    .then(all {
      assert(all {
        state.collector.phase == GcSweepingChunks,
        isLive(state.bucket.commits, ALICE_1),
      }),
      state' = state,
    })
    .then(gcDone)

  /// The same gc, whose first refresh is sent at tick 6 and lands after alice made her
  /// stack: it does not bring gc back to life. Its lease had gone unseen for a moment,
  /// and gc, too old to pass its check when the reply came, stays so.
  /// @conformance
  run lateRefreshDoesNotReviveRun = init
    .then(gcLease)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(tick)
    .then(tick)
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(createBranch("alice", Answered))
    .then(stack("alice", ALICE_A))
    .then(refreshLand(GC, Answered))
    .then(refreshAck(GC))
    .then(all { assert(state.timing.get(GC).refreshed == 0), state' = state })
    .then(graceElapses)
    .then(gcListCommits)
    .then(all {
      assert(all {
        state.collector.phase == GcSweepingChunks,
        isLive(state.bucket.commits, ALICE_1),
      }),
      state' = state,
    })
    .then(gcDone)

  /// A rewriter whose lease went stale before its swap returns the error and moves nothing
  /// (`rewrite_ref` checks, then swaps; its record is written, harmlessly, and kept).
  /// @conformance
  run staleRewriterRefusesRun = init
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
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(rwRead("carol"))
    .then(all {
      assert(all {
        client(state, "carol").outcome == Unknown,
        tip(state.bucket) == { commit: ALICE_1, version: 2 },
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// A lander whose lease went stale refuses its swap as a dropped one: it rereads the
  /// branch (and, the lease still stale, would refuse again).
  /// @conformance
  run staleLanderRefusesRun = init
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(createBranch("alice", Answered))
    .then(stack("alice", ALICE_A))
    .then(beginLand("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(ldRead("alice"))
    .then(all {
      assert(all {
        client(state, "alice").phase == LdRechecking,
        tip(state.bucket) == { commit: ROOT, version: 1 },
      }),
      state' = state,
    })

  /// A combining writer whose lease went stale refuses its batch swap as a dropped one,
  /// and rereads the ref: nothing landed, so it polls again.
  /// @conformance
  run staleCombinerRefusesRun = init
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
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(gather("alice", Set()))
    .then(all { assert(client(state, "alice").phase == BatchRechecking), state' = state })
    .then(batchRecheck("alice"))
    .then(all {
      assert(all {
        client(state, "alice").phase == Polling,
        tip(state.bucket) == { commit: CAROL_1, version: 3 },
      }),
      state' = state,
    })

  // ---------- packs and repacking ----------

  pure val BIG_A: Delta = Map("a" -> "p1")
  pure val BIG_B: Delta = Map("b" -> "p3")
  pure val BIG_A2: Delta = Map("a" -> "p2")
  pure val BIG_1: CommitId = ["root", "alice"]
  /// A repack of `BIG_1` by carol: her delta writes `a` with the content it has.
  pure val REPACKED: CommitId = ["root", "alice", "carol"]

  /// Alice writes a big file (a pack of her own). Carol's repack is a commit whose delta
  /// writes the file with the content it has: same tree, an empty change list, the file in
  /// a pack of hers. Both packs stay (the history names alice's), and gc, finding both
  /// named, deletes none.
  /// @conformance
  run repackKeepsContentRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("carol", BIG_1, BIG_A))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: REPACKED, version: 3 },
        client(state, "carol").outcome == Published,
        treeOf(state.bucket, REPACKED) == treeOf(state.bucket, BIG_1),
        deltaOf(state.bucket, REPACKED, BIG_1) == Map(),
        body(state.bucket.commits, BIG_1).locs == Map("a" -> ["alice"]),
        body(state.bucket.commits, REPACKED).locs == Map("a" -> ["carol"]),
        liveKeys(state.bucket.packs) == Set(["alice"], ["carol"]),
        body(state.bucket.packs, ["carol"]) == Set("p1"),
        relocateKeepsContent,
        refOnlyMovesForward,
      }),
      state' = state,
    })
    .then(graceElapses)
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcListPages)
    .then(gcListPacks)
    .then(gcListChunks)
    .then(gcDone)
    .then(all {
      assert(all {
        liveKeys(state.bucket.packs) == Set(["alice"], ["carol"]),
        packsAreStored,
      }),
      state' = state,
    })

  /// Carol's repack and bob's change to another path both read alice's tip; carol lands
  /// first. Bob's CAS loses; he re-reads, replays his delta on carol's commit and lands:
  /// his page names the file where carol's repack put it, and his own file in his pack. A
  /// writer based before a repack neither conflicts with it nor undoes it.
  /// @conformance
  run repackRacesStaleWriterRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("carol", BIG_1, BIG_A))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(begin("bob", BIG_1, BIG_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("carol"))
    .then(readRef("bob"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(all {
      assert(all {
        tip(state.bucket) == { commit: REPACKED.append("bob"), version: 4 },
        client(state, "carol").outcome == Published,
        client(state, "bob").outcome == Published,
        treeOf(state.bucket, REPACKED.append("bob")) == Map("a" -> "p1", "b" -> "p3"),
        body(state.bucket.commits, REPACKED.append("bob")).locs == Map("a" -> ["carol"], "b" -> ["bob"]),
        liveKeys(state.bucket.packs) == Set(["alice"], ["carol"], ["bob"]),
        packsAreStored,
        relocateKeepsContent,
      }),
      state' = state,
    })

  /// Bob changes `a` while carol's repack of it is in flight. Carol's CAS loses; her
  /// re-read finds `a` changed under her (`check_disjoint`) and fails with a conflict:
  /// nothing of hers is on the branch. Her pack, commit and page (the tree alice's page has,
  /// in her pack) are stored, named by nothing; a repack that recomputes from the new tip
  /// (dave) lands, and gc, once they are old, deletes them and no other.
  /// @conformance
  run repackRacesChangedPathRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("carol", BIG_1, BIG_A))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(begin("bob", BIG_1, BIG_A2))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("carol"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(readRef("carol"))
    .then(all {
      assert(all {
        client(state, "carol").outcome == Conflicted,
        branch(state.bucket) == Set(ROOT, BIG_1, ["root", "alice", "bob"]),
        liveKeys(state.bucket.packs) == Set(["alice"], ["bob"], ["carol"]),
        packsAreStored,
      }),
      state' = state,
    })
    .then(begin("dave", ["root", "alice", "bob"], BIG_A2))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(readRef("dave"))
    .then(upload("dave"))
    .then(swap("dave", Answered))
    .then(graceElapses)
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcDeleteCommit(REPACKED))
    .then(gcListPages)
    .then(gcDeletePage({ tree: Map("a" -> "p1", "b" -> "0"), locs: Map("a" -> ["carol"]) }))
    .then(gcListPacks)
    .then(gcDeletePack(["carol"]))
    .then(gcListChunks)
    .then(gcDone)
    .then(all {
      assert(all {
        client(state, "dave").outcome == Published,
        liveKeys(state.bucket.packs) == Set(["alice"], ["bob"], ["dave"]),
        body(state.bucket.commits, ["root", "alice", "bob", "dave"]).locs == Map("a" -> ["dave"]),
        packsAreStored,
        relocateKeepsContent,
      }),
      state' = state,
    })

  /// The race the lease closes, for a pack. Carol's repack stores its pack and commit,
  /// but her swap never arrives; by the time she retries the pack is old. Her retry stores
  /// nothing new (the pack and commit exist already, so keep their age: the real retry
  /// uploads no packs at all). gc starts between her upload and her swap, and waits for her
  /// lease; once she has landed it marks her commit and her pack.
  /// @conformance
  run gcWaitsForARepackReusingAnOldPackRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("carol", BIG_1, BIG_A))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Dropped))
    .then(recheck("carol"))
    .then(graceElapses)
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(gcLease)
    .then(gcCheckLeases)
    .then(all { assert(state.collector.phase == GcDraining), state' = state })
    .then(gcCheckLeases)
    .then(swap("carol", Answered))
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcListPages)
    .then(gcListPacks)
    .then(gcListChunks)
    .then(gcDone)
    .then(all {
      assert(all {
        client(state, "carol").outcome == Published,
        tip(state.bucket).commit == REPACKED,
        liveKeys(state.bucket.packs) == Set(["alice"], ["carol"]),
        packsAreStored,
      }),
      state' = state,
    })

  /// Alice's commit never lands (both swaps are lost), leaving a pack named by no commit a
  /// ref reaches. gc, run while it is young, spares it (it may belong to a writer outside
  /// any lease); once the grace period has passed, the next run deletes it.
  /// @conformance
  run gcSparesAYoungPackRun = init
    .then(begin("alice", ROOT, BIG_A))
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
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcListPages)
    .then(gcListPacks)
    .then(gcListChunks)
    .then(all {
      assert(all {
        state.collector.doomedPacks == Set(),
        liveKeys(state.bucket.packs) == Set(["alice"]),
      }),
      state' = state,
    })
    .then(gcDone)
    .then(graceElapses)
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcDeleteCommit(BIG_1))
    .then(gcListPages)
    .then(gcDeletePage({ tree: Map("a" -> "p1", "b" -> "0"), locs: Map("a" -> ["alice"]) }))
    .then(gcListPacks)
    .then(gcDeletePack(["alice"]))
    .then(gcListChunks)
    .then(gcDone)
    .then(all {
      assert(all {
        liveKeys(state.bucket.packs) == Set(),
        liveKeys(state.bucket.commits) == Set(ROOT),
        packsAreStored,
      }),
      state' = state,
    })

  /// An abandoned commit of alice's leaves its commit, page and pack behind (her swaps
  /// never arrived). gc deletes the commit and page, then stalls: its refresh never lands,
  /// and by the time it lists `packs/` its lease is too old to pass the check before a
  /// pack delete. It gives up with the pack stored. (Without the check on pack deletes
  /// gc deleted the pack of a commit a writer could have reused since.)
  /// @conformance
  run staleGcRefusesToDeletePackRun = init
    .then(begin("alice", ROOT, BIG_A))
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
    .then(gcDeleteCommit(BIG_1))
    .then(gcListPages)
    .then(gcDeletePage({ tree: Map("a" -> "p1", "b" -> "0"), locs: Map("a" -> ["alice"]) }))
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(gcListPacks)
    .then(all {
      assert(all {
        state.collector.phase == GcSweepingChunks,
        state.collector.doomedPacks == Set(),
        isLive(state.bucket.packs, ["alice"]),
      }),
      state' = state,
    })
    .then(gcDone)

  // ---------- chunks ----------

  pure val HUGE_A: Delta = Map("a" -> "h1")
  pure val HUGE_B: Delta = Map("b" -> "h2")
  pure val HUGE_1: CommitId = ["root", "alice"]
  pure val HUGE_PAGE_A = { tree: Map("a" -> "h1", "b" -> "0"), locs: Map() }

  /// Alice writes a huge file (`h1`: chunks `k1`, `k2`), landing it; once the chunks are
  /// old, bob writes another (`h2`: `k1`, `k3`) on top. His upload stores `k3` alone: `k1`
  /// exists, so the create writes nothing and `k1` keeps its age (and is not young). gc
  /// finds every chunk named and deletes none.
  /// @conformance
  run chunksAreSharedRun = init
    .then(begin("alice", ROOT, HUGE_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(all {
      assert(all {
        liveKeys(state.bucket.chunks) == Set("k1", "k2"),
        state.bucket.young.chunks == Set("k1", "k2"),
        liveKeys(state.bucket.packs) == Set(),
        chunksAreStored,
        chunkedFilesReadBack,
      }),
      state' = state,
    })
    .then(graceElapses)
    .then(begin("bob", HUGE_1, HUGE_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(all {
      assert(all {
        client(state, "bob").outcome == Published,
        liveKeys(state.bucket.chunks) == Set("k1", "k2", "k3"),
        state.bucket.young.chunks == Set("k3"),
        chunksOf(state.bucket, HUGE_1.append("bob")) == Set("k1", "k2", "k3"),
        chunksAreStored,
        chunkedFilesReadBack,
      }),
      state' = state,
    })
    .then(graceElapses)
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcListPages)
    .then(gcListPacks)
    .then(gcListChunks)
    .then(all {
      assert(all {
        state.collector.liveChunks == Set("k1", "k2", "k3"),
        state.collector.doomedChunks == Set(),
      }),
      state' = state,
    })
    .then(gcDone)
    .then(all {
      assert(all {
        liveKeys(state.bucket.chunks) == Set("k1", "k2", "k3"),
        chunksAreStored,
        gcSparesNamedChunks,
      }),
      state' = state,
    })

  /// Alice's huge file never lands (both swaps are lost), leaving chunks, a page and a
  /// commit named by nothing a ref reaches. gc, run while they are young, spares them;
  /// once the grace period has passed, the next run deletes the commit, page and chunks.
  /// @conformance
  run gcSparesAYoungChunkRun = init
    .then(begin("alice", ROOT, HUGE_A))
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
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcListPages)
    .then(gcListPacks)
    .then(gcListChunks)
    .then(all {
      assert(all {
        state.collector.doomedChunks == Set(),
        liveKeys(state.bucket.chunks) == Set("k1", "k2"),
      }),
      state' = state,
    })
    .then(gcDone)
    .then(graceElapses)
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcDeleteCommit(HUGE_1))
    .then(gcListPages)
    .then(gcDeletePage(HUGE_PAGE_A))
    .then(gcListPacks)
    .then(gcListChunks)
    .then(all { assert(state.collector.doomedChunks == Set("k1", "k2")), state' = state })
    .then(gcDeleteChunk("k2"))
    .then(gcDeleteChunk("k1"))
    .then(gcDone)
    .then(all {
      assert(all {
        liveKeys(state.bucket.chunks) == Set(),
        liveKeys(state.bucket.commits) == Set(ROOT),
        chunksAreStored,
      }),
      state' = state,
    })

  /// A chunk a live commit names outlives the abandoned commit that stored it. Alice's
  /// huge file (`k1`, `k2`) never lands; bob's (`k1`, `k3`) does. Once all are old, gc
  /// deletes alice's commit, page and `k2`, and keeps `k1` and `k3`: bob's page names them.
  /// @conformance
  run gcKeepsASharedChunkRun = init
    .then(begin("alice", ROOT, HUGE_A))
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
    .then(begin("bob", ROOT, HUGE_B))
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
    .then(gcDeleteCommit(HUGE_1))
    .then(gcListPages)
    .then(gcDeletePage(HUGE_PAGE_A))
    .then(gcListPacks)
    .then(gcListChunks)
    .then(all {
      assert(all {
        state.collector.liveChunks == Set("k1", "k3"),
        state.collector.doomedChunks == Set("k2"),
      }),
      state' = state,
    })
    .then(gcDeleteChunk("k2"))
    .then(gcDone)
    .then(all {
      assert(all {
        liveKeys(state.bucket.chunks) == Set("k1", "k3"),
        tip(state.bucket).commit == ["root", "bob"],
        chunksAreStored,
        chunkedFilesReadBack,
      }),
      state' = state,
    })

  /// The race the lease closes, for a chunk (dedup). Alice's huge file never lands, but
  /// her chunks stay; by the time bob writes another sharing `k1` they are old. His upload
  /// stores `k3` and writes nothing for `k1` (it exists), so `k1` stays old, named by no
  /// commit a ref reaches. gc starts between his upload and his swap, and waits for his
  /// lease; once he has landed it marks his commit and, with it, `k1`. Alice's own chunk,
  /// `k2`, goes.
  /// @conformance
  run gcWaitsForAWriterReusingAnOldChunkRun = init
    .then(begin("alice", ROOT, HUGE_A))
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
    .then(begin("bob", ROOT, HUGE_B))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(all {
      assert(all {
        state.bucket.young.chunks == Set("k3"),
        isLive(state.bucket.chunks, "k1"),
      }),
      state' = state,
    })
    .then(gcLease)
    .then(gcCheckLeases)
    .then(all { assert(state.collector.phase == GcDraining), state' = state })
    .then(gcCheckLeases)
    .then(swap("bob", Answered))
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcDeleteCommit(HUGE_1))
    .then(gcListPages)
    .then(gcDeletePage(HUGE_PAGE_A))
    .then(gcListPacks)
    .then(gcListChunks)
    .then(gcDeleteChunk("k2"))
    .then(gcDone)
    .then(all {
      assert(all {
        client(state, "bob").outcome == Published,
        liveKeys(state.bucket.chunks) == Set("k1", "k3"),
        chunksAreStored,
        chunkedFilesReadBack,
      }),
      state' = state,
    })

  /// An abandoned commit of alice's leaves its chunks behind. gc deletes the commit and
  /// page, then stalls: by the time it lists `chunks/` its lease is too old to pass the
  /// check before a chunk delete. It gives up with the chunks stored. (Without the check on
  /// chunk deletes gc deleted chunks of a commit a writer could have reused since.)
  /// @conformance
  run staleGcRefusesToDeleteChunkRun = init
    .then(begin("alice", ROOT, HUGE_A))
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
    .then(gcDeleteCommit(HUGE_1))
    .then(gcListPages)
    .then(gcDeletePage(HUGE_PAGE_A))
    .then(gcListPacks)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(gcListChunks)
    .then(all {
      assert(all {
        state.collector.phase == GcSweepingChunks,
        state.collector.doomedChunks == Set(),
        liveKeys(state.bucket.chunks) == Set("k1", "k2"),
      }),
      state' = state,
    })
    .then(gcDone)

  // ---------- reuse ----------

  /// Alice writes a big file; bob, based on her commit, writes the same content at
  /// another path (a move or copy: his base has it). He reuses her pack: his page names it
  /// for `b` and he stores no pack. A huge file's chunks are named by its content, so
  /// reusing one stores no chunk either. gc keeps her pack: both pages name it.
  /// @conformance
  run reuseNamesTheBasePackRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", BIG_1, Map("b" -> "p1")))
    .then(all { assert(client(state, "bob").reuse == Map("b" -> ["alice"])), state' = state })
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(all {
      assert(all {
        client(state, "bob").outcome == Published,
        body(state.bucket.commits, BIG_1.append("bob")).locs == Map("a" -> ["alice"], "b" -> ["alice"]),
        liveKeys(state.bucket.packs) == Set(["alice"]),
        packsAreStored,
        relocateKeepsContent,
      }),
      state' = state,
    })
    .then(graceElapses)
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcListPages)
    .then(gcListPacks)
    .then(gcListChunks)
    .then(gcDone)
    .then(all {
      assert(all {
        liveKeys(state.bucket.packs) == Set(["alice"]),
        packsAreStored,
      }),
      state' = state,
    })

  /// Bob's reused pack (alice's) is what his page names for `b` even when he lands on a
  /// newer head: carol rewrote `a` meanwhile (her own pack), and bob's replay keeps carol's
  /// `a` and alice's pack for `b`.
  /// @conformance
  run reuseSurvivesAReplayRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", BIG_1, Map("b" -> "p1")))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(begin("carol", BIG_1, BIG_A2))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("bob"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(all {
      assert(all {
        client(state, "bob").outcome == Published,
        tip(state.bucket).commit == BIG_1.append("carol").append("bob"),
        body(state.bucket.commits, tip(state.bucket).commit).locs == Map("a" -> ["carol"], "b" -> ["alice"]),
        treeOf(state.bucket, tip(state.bucket).commit) == Map("a" -> "p2", "b" -> "p1"),
        liveKeys(state.bucket.packs) == Set(["alice"], ["carol"]),
        packsAreStored,
        relocateKeepsContent,
      }),
      state' = state,
    })

  // ---------- packs through rewrites and landing ----------
  //
  // A rewrite's copy and a landed stack's copy are written from the original's change list,
  // whose entries carry their locations: the copy's page names the original's pack and
  // stores none. Only a writer's upload makes a pack.

  pure val BIG_BOB: Delta = Map("b" -> "p3")

  /// Alice writes `a` and bob `b`, each a big file in a pack of his own. Carol swaps the two
  /// commits: both copies point at their originals' packs, no pack is stored, and gc, with
  /// the originals only in the reflog, keeps both.
  /// @conformance
  run rebaseReorderKeepsPacksRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", BIG_1, BIG_BOB))
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
        tip(state.bucket).commit == ["root", "bob", "~carol", "alice", "~carol"],
        body(state.bucket.commits, ["root", "bob", "~carol"]).locs == Map("b" -> ["bob"]),
        body(state.bucket.commits, tip(state.bucket).commit).locs == Map("a" -> ["alice"], "b" -> ["bob"]),
        liveKeys(state.bucket.packs) == Set(["alice"], ["bob"]),
        packsAreWritersOwn,
        packsAreStored,
        relocateKeepsContent,
      }),
      state' = state,
    })
    .then(graceElapses)
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcListPages)
    .then(gcListPacks)
    .then(gcListChunks)
    .then(gcDone)
    .then(all {
      assert(all {
        liveKeys(state.bucket.packs) == Set(["alice"], ["bob"]),
        packsAreStored,
        relocateKeepsContent,
      }),
      state' = state,
    })

  /// Carol melds bob's commit into alice's: the one commit has `a` from alice's pack and `b`
  /// from bob's, and stores no pack.
  /// @conformance
  run rebaseSquashKeepsPacksRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", BIG_1, BIG_BOB))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(beginRebase("carol", "squash"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(all {
      assert(all {
        authorsOf(tip(state.bucket).commit) == Set("alice", "bob"),
        body(state.bucket.commits, tip(state.bucket).commit).locs == Map("a" -> ["alice"], "b" -> ["bob"]),
        liveKeys(state.bucket.packs) == Set(["alice"], ["bob"]),
        packsAreWritersOwn,
        relocateKeepsContent,
      }),
      state' = state,
    })

  /// Both commits write `a`, to different big contents; melded, the later entry replaces the
  /// earlier one's, location and all: the one commit names bob's pack for `a`. Alice's pack
  /// stays (the reflog's record names her commit).
  /// @conformance
  run rebaseSquashLaterPackWinsRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(begin("bob", BIG_1, BIG_A2))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(beginRebase("carol", "squash"))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(rwStore("carol"))
    .then(rwRead("carol"))
    .then(rwSwap("carol", Answered))
    .then(all {
      assert(all {
        treeOf(state.bucket, tip(state.bucket).commit) == Map("a" -> "p2", "b" -> "0"),
        body(state.bucket.commits, tip(state.bucket).commit).locs == Map("a" -> ["bob"]),
        liveKeys(state.bucket.packs) == Set(["alice"], ["bob"]),
        packsAreWritersOwn,
        relocateKeepsContent,
      }),
      state' = state,
    })
    .then(graceElapses)
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcListPages)
    .then(gcListPacks)
    .then(gcListChunks)
    .then(gcDone)
    .then(all {
      assert(all {
        liveKeys(state.bucket.packs) == Set(["alice"], ["bob"]),
        packsAreStored,
      }),
      state' = state,
    })

  /// Carol rewords alice's commit while bob lands a big file on it. The reword's copy
  /// keeps alice's page (same locations); bob's commit carried onto it names bob's pack.
  /// @conformance
  run carryKeepsPacksRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(beginRewrite("carol"))
    .then(begin("bob", BIG_1, BIG_BOB))
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
        tip(state.bucket).commit == ["root", "alice", "~carol", "bob", "~carol"],
        body(state.bucket.commits, ["root", "alice", "~carol"]).locs == Map("a" -> ["alice"]),
        body(state.bucket.commits, tip(state.bucket).commit).locs == Map("a" -> ["alice"], "b" -> ["bob"]),
        liveKeys(state.bucket.packs) == Set(["alice"], ["bob"]),
        packsAreWritersOwn,
        packsAreStored,
        relocateKeepsContent,
      }),
      state' = state,
    })

  /// Alice stacks a big file on the root; carol lands another on the branch first. Alice's
  /// land copies her commit onto carol's: the copy names alice's pack for `a` and carol's
  /// for `b`, and stores no pack.
  /// @conformance
  run landCopyKeepsPacksRun = init
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(createBranch("alice", Answered))
    .then(stack("alice", BIG_A))
    .then(begin("carol", ROOT, BIG_BOB))
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
    .then(all {
      assert(all {
        client(state, "alice").outcome == Published,
        tip(state.bucket).commit == ["root", "carol", "alice", "~alice"],
        body(state.bucket.commits, tip(state.bucket).commit).locs == Map("a" -> ["alice"], "b" -> ["carol"]),
        liveKeys(state.bucket.packs) == Set(["alice"], ["carol"]),
        packsAreWritersOwn,
        packsAreStored,
        relocateKeepsContent,
      }),
      state' = state,
    })
    .then(graceElapses)
    .then(gcLease)
    .then(gcCheckLeases)
    .then(gcMark)
    .then(gcListCommits)
    .then(gcListPages)
    .then(gcListPacks)
    .then(gcListChunks)
    .then(gcDone)
    .then(all {
      assert(all {
        liveKeys(state.bucket.packs) == Set(["alice"], ["carol"]),
        packsAreStored,
      }),
      state' = state,
    })

  // ---------- content reuse through combining ----------

  pure val CAROL_PACK: CommitId = ["root", "carol"]

  /// Carol writes a big file. Alice, based on it, writes the same content at `b` (a copy:
  /// her base has it), so her upload stores no pack. Bob and dave land on `a` first; she
  /// loses twice and combines. Her intent carries the entry with the location her upload
  /// found, so the commit the round makes for it names carol's pack for `b`: still no pack
  /// of alice's.
  /// @conformance
  run reuseThroughCombiningRun = init
    .then(begin("carol", ROOT, BIG_A))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(begin("alice", CAROL_PACK, Map("b" -> "p1")))
    .then(all { assert(client(state, "alice").reuse == Map("b" -> ["carol"])), state' = state })
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(begin("bob", CAROL_PACK, BOB_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(readRef("alice"))
    .then(begin("dave", CAROL_PACK.append("bob"), Map("a" -> "3")))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(readRef("dave"))
    .then(upload("dave"))
    .then(swap("dave", Answered))
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
        client(state, "alice").outcome == Published,
        tip(state.bucket).commit == CAROL_PACK.append("bob").append("dave").append("alice+"),
        treeOf(state.bucket, tip(state.bucket).commit) == Map("a" -> "3", "b" -> "p1"),
        body(state.bucket.commits, tip(state.bucket).commit).locs == Map("b" -> ["carol"]),
        liveKeys(state.bucket.packs) == Set(["carol"]),
        packsAreStored,
        relocateKeepsContent,
      }),
      state' = state,
    })

  // ---------- making a stack's branch ----------

  /// Alice's branch is created under a lease of its own, checked before the write
  /// (`create_branch`). Her lease was put at tick 0 and is listed at tick 4, past the
  /// margin: the check refuses, nothing is written, the call fails and its lease goes.
  /// @conformance
  run staleBranchCreateRefusesRun = init
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(tick)
    .then(tick)
    .then(tick)
    .then(tick)
    .then(checkLeases("alice"))
    .then(all {
      assert(all {
        client(state, "alice").phase == Done,
        client(state, "alice").outcome == Unknown,
        not(state.bucket.refs.keys().contains("alice")),
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// The same at the edge of the margin: three ticks, and the check passes.
  /// @conformance
  run branchCreateAtTheEdgeRun = init
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(tick)
    .then(tick)
    .then(tick)
    .then(checkLeases("alice"))
    .then(createBranch("alice", Answered))
    .then(all {
      assert(all {
        client(state, "alice").phase == Branched,
        body(state.bucket.refs, "alice").commit == ROOT,
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  /// The write lands but its answer is lost: the call fails, and the branch exists, naming
  /// the commit it was made at. A stack is not made.
  /// @conformance
  run createBranchLostReplyRun = init
    .then(beginBranch("alice"))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(createBranch("alice", Lost))
    .then(all {
      assert(all {
        client(state, "alice").phase == Done,
        client(state, "alice").outcome == Unknown,
        body(state.bucket.refs, "alice").commit == ROOT,
        liveKeys(state.bucket.locks) == Set(),
      }),
      state' = state,
    })

  // ---------- amortized compaction: soft relocations ----------
  //
  // A commit that moves old files of its base into the pack it uploads anyway
  // (`CommitOptions::relocs`, README "Amortized compaction"). The relocations are soft: a
  // path the head changed, or already moved, is dropped silently on replay and never
  // conflicts; the commit's own change always lands.

  pure val OLD: CommitId = ["root", "alice"]
  /// Alice's `a = p1`, in a pack of hers; bob writes `b = p3` and relocates `a`.
  pure val BOB_B3: Delta = Map("b" -> "p3")
  pure val RELOC_A: Delta = Map("a" -> "p1")
  pure val CAROL_A2: Delta = Map("a" -> "p2")
  pure val CAROL_A3: Delta = Map("a" -> "p3")
  pure val DAVE_A2: Delta = Map("a" -> "p2")


  /// Nothing races: bob's `b` lands, and `a` moves into his pack (same tree, the file in
  /// bob's pack and alice's stays).
  /// @conformance
  run softKeptRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(beginSoft("bob", OLD, BOB_B3, RELOC_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(all {
      assert(all {
        client(state, "bob").outcome == Published,
        treeOf(state.bucket, OLD.append("bob")) == Map("a" -> "p1", "b" -> "p3"),
        body(state.bucket.commits, OLD.append("bob")).locs == Map("a" -> ["bob"], "b" -> ["bob"]),
        body(state.bucket.packs, ["bob"]) == Set("p1", "p3"),
        relocationsKeepContent,
        relocateKeepsContent,
      }),
      state' = state,
    })

  /// Carol changes `a` while bob's commit, which relocates it, is in flight. Bob's CAS
  /// loses; on the replay his relocation of `a` is dropped (carol's content stays) and his
  /// own `b` lands: no conflict, nothing of his lost. His pack still holds what he planned.
  /// @conformance
  run softDroppedWhenHeadChangedRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(beginSoft("bob", OLD, BOB_B3, RELOC_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(begin("carol", OLD, CAROL_A2))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("bob"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(all {
      assert(all {
        client(state, "bob").outcome == Published,
        client(state, "carol").outcome == Published,
        tip(state.bucket).commit == OLD.append("carol").append("bob"),
        treeOf(state.bucket, OLD.append("carol").append("bob")) == Map("a" -> "p2", "b" -> "p3"),
        body(state.bucket.commits, OLD.append("carol").append("bob")).locs == Map("a" -> ["carol"], "b" -> ["bob"]),
        body(state.bucket.packs, ["bob"]) == Set("p1", "p3"),
        relocationsKeepContent,
        relocationsNeverConflict,
        relocateKeepsContent,
        packsAreStored,
      }),
      state' = state,
    })

  /// A repack (carol's delta writes `a` with the content it has) lands first. Bob's
  /// relocation of `a` finds it already moved elsewhere and is dropped: carol's stays.
  /// @conformance
  run softDroppedWhenHeadRelocatedRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(beginSoft("bob", OLD, BOB_B3, RELOC_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(begin("carol", OLD, RELOC_A))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("bob"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(readRef("bob"))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(all {
      assert(all {
        client(state, "bob").outcome == Published,
        treeOf(state.bucket, OLD.append("carol").append("bob")) == Map("a" -> "p1", "b" -> "p3"),
        body(state.bucket.commits, OLD.append("carol").append("bob")).locs == Map("a" -> ["carol"], "b" -> ["bob"]),
        relocationsKeepContent,
        relocateKeepsContent,
        packsAreStored,
      }),
      state' = state,
    })

  /// The same through combining. Bob loses to dave's change of `a`, then to carol's, and
  /// combines: his intent carries the relocation, dropped in the round (`a` changed since
  /// his base), his own `b` landing in the batch, in the pack he uploaded at the start.
  /// @conformance
  run softDroppedInCombineRun = init
    .then(begin("alice", ROOT, BIG_A))
    .then(lease("alice"))
    .then(checkLeases("alice"))
    .then(readRef("alice"))
    .then(upload("alice"))
    .then(swap("alice", Answered))
    .then(beginSoft("bob", OLD, BOB_B3, RELOC_A))
    .then(lease("bob"))
    .then(checkLeases("bob"))
    .then(readRef("bob"))
    .then(begin("dave", OLD, DAVE_A2))
    .then(lease("dave"))
    .then(checkLeases("dave"))
    .then(readRef("dave"))
    .then(upload("dave"))
    .then(swap("dave", Answered))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(readRef("bob"))
    .then(begin("carol", OLD.append("dave"), CAROL_A3))
    .then(lease("carol"))
    .then(checkLeases("carol"))
    .then(readRef("carol"))
    .then(upload("carol"))
    .then(swap("carol", Answered))
    .then(upload("bob"))
    .then(swap("bob", Answered))
    .then(all { assert(client(state, "bob").phase == Intending), state' = state })
    .then(intend("bob", 1))
    .then(pollRef("bob"))
    .then(pollList("bob"))
    .then(gather("bob", Set()))
    .then(batchSwap("bob", Answered))
    .then(cleanup("bob"))
    .then(all {
      assert(all {
        client(state, "bob").outcome == Published,
        tip(state.bucket).commit == OLD.append("dave").append("carol").append("bob+"),
        treeOf(state.bucket, tip(state.bucket).commit) == Map("a" -> "p3", "b" -> "p3"),
        body(state.bucket.commits, tip(state.bucket).commit).locs == Map("a" -> ["carol"], "b" -> ["bob"]),
        body(state.bucket.packs, ["bob"]) == Set("p1", "p3"),
        relocationsKeepContent,
        relocationsNeverConflict,
        relocateKeepsContent,
        packsAreStored,
      }),
      state' = state,
    })
}
