// -*- mode: Bluespec; -*-
/// Interleavings the orchestrator and the runner must follow (`actions.qnt`), step by step:
/// a refinement harness (with the orchestrator, T1.7, and the runner, T2.5) will run each
/// against the real code with Tier 1 writes and Tier 2 requests held until the scenario
/// releases them. The use cases of the plan's section 5 that touch state: UC1 a push runs
/// jobs, UC2 cancel-in-progress, UC3 needs, UC4 a runner dies, UC6 rerun, UC7 an
/// environment gate, UC8 a cron slot; UC9 checks are the projection. (UC5 logs, UC10
/// artifacts and caches, UC11 listing and retention, UC12 secrets are not state of this
/// model.) `o1`, `o2` are orchestrator replicas, `r1`..`r3` runners; push1 is by alice and
/// push2 by bob, both in one concurrency group.
module actionsScenarios {
  import actions.* from "./actions"

  pure def jb(id: int, att: int, j: str): JobKey = { rid: id, att: att, job: j }
  pure val BLD: JobKey = jb(1, 1, "build")
  pure val TST: JobKey = jb(1, 1, "test")
  pure val DEP: JobKey = jb(1, 1, "deploy")
  pure val CK_BUILD_1: CheckKey = { sha: 1, job: "build" }

  // Each step below is one action, so that a refinement harness can drive it: a replica's
  // tick is `read(o, i)` then `write(o)`; a runner's claim `claimRead(r, jk)` then
  // `claimWrite(r)`; a lease lapses after `TTL` (2) `tick`s. Several runs start with push1
  // triggered, r1 holding run 1's build (in progress), and then r1 running its steps and the
  // orchestrator accepting the result.

  /// UC1. A push runs a job: the run is created with its claim, jobs and concurrency file in
  /// one commit; the runner's lease and result; the orchestrator's acceptance and the check.
  /// @conformance
  run pushRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(all {
      assert(all {
        state.t1.runs.keys() == Set(1),
        state.t1.runs.get(1) == { key: "push1", att: 1, done: false, concl: NoConcl, cancel: false },
        state.t1.claims == Map("push1" -> 1),
        state.t1.groups == Map("main" -> 1),
        state.t1.jobs.get(BLD).status == Queued,
        state.t1.jobs.get(TST).status == Pending,
        state.t1.jobs.get(DEP).status == Pending,
        state.t2.queue == Set(),
      }),
      state' = state,
    })
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(all {
      assert(all {
        state.t2.queue == Set(BLD),
        state.t2.leases.get(BLD) == { holder: "r1", seq: 1, exp: 2 },
        // the job file does not know yet
        state.t1.jobs.get(BLD).status == Queued,
      }),
      state' = state,
    })
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(started("r1"))
    .then(refresh("r1"))
    .then(result("r1", Success))
    .then(read("o1", IAccept({ jk: BLD, seq: 1 })))
    .then(write("o1"))
    .then(read("o1", ICheck(BLD)))
    .then(write("o1"))
    .then(all {
      assert(all {
        state.t1.jobs.get(BLD).status == InProgress or state.t1.jobs.get(BLD).status == Completed,
        state.t1.jobs.get(BLD).concl == Success,
        state.t1.jobs.get(BLD).by == "r1",
        state.t1.jobs.get(BLD).byseq == 1,
        state.t1.checks.get(CK_BUILD_1) == { rid: 1, att: 1, state: Success },
        state.runners.get("r1").ph == RIdle,
      }),
      state' = state,
    })

  /// UC2. A second push supersedes the first: one commit creates run 2, takes the group over
  /// and cancels run 1 (jobs not started are completed cancelled; the running build gets a
  /// marker, its runner stops and reports cancelled, and the run concludes cancelled).
  /// @conformance
  run supersedeRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(started("r1"))
    .then(fire("push2"))
    .then(read("o2", ITrigger("push2")))
    .then(write("o2"))
    .then(all {
      assert(all {
        state.t1.runs.get(1).cancel,
        not(state.t1.runs.get(1).done),
        state.t1.runs.get(2).key == "push2",
        state.t1.groups == Map("main" -> 2),
        state.t1.jobs.get(BLD).status == InProgress,
        state.t1.jobs.get(TST).concl == Cancelled,
        state.t1.jobs.get(DEP).concl == Cancelled,
        state.t1.jobs.get(jb(2, 1, "build")).status == Queued,
      }),
      state' = state,
    })
    .then(sync)
    .then(all { assert(state.t2.cancels == Set(BLD)), state' = state })
    .then(result("r1", Cancelled))
    .then(read("o1", IAccept({ jk: BLD, seq: 1 })))
    .then(write("o1"))
    .then(read("o1", IFinish(1)))
    .then(write("o1"))
    .then(all {
      assert(all {
        state.t1.runs.get(1).done,
        state.t1.runs.get(1).concl == Cancelled,
        state.t1.jobs.get(BLD).concl == Cancelled,
        // the cancelled run's deploy was never queued, so never needed its approval
        state.t1.approvals == Set(),
        not(state.t1.runs.get(2).cancel),
      }),
      state' = state,
    })

  /// UC2, two replicas. Both read the empty branch for two different pushes: the first
  /// commit wins the run id and the group, the second finds them taken, reads again and
  /// cancels the first run: never two active runs in the group.
  /// @conformance
  run concurrentPushesRun = init
    .then(fire("push1"))
    .then(fire("push2"))
    .then(read("o1", ITrigger("push1")))
    .then(read("o2", ITrigger("push2")))
    .then(write("o1"))
    .then(write("o2"))
    .then(all {
      assert(all {
        state.t1.runs.keys() == Set(1),
        state.t1.claims == Map("push1" -> 1),
        state.pend.get("o2") == NoPend,
      }),
      state' = state,
    })
    .then(read("o2", ITrigger("push2")))
    .then(write("o2"))
    .then(all {
      assert(all {
        state.t1.runs.keys() == Set(1, 2),
        state.t1.runs.get(1).cancel,
        not(state.t1.runs.get(2).cancel),
        state.t1.groups == Map("main" -> 2),
        oneActivePerGroup,
      }),
      state' = state,
    })

  /// UC3. A need failed: the dependents are skipped (never queued), and the run fails.
  /// @conformance
  run needFailedRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(started("r1"))
    .then(result("r1", Failure))
    .then(read("o1", IAccept({ jk: BLD, seq: 1 })))
    .then(write("o1"))
    .then(read("o2", IPromote(TST)))
    .then(write("o2"))
    .then(read("o2", IPromote(DEP)))
    .then(write("o2"))
    .then(all {
      assert(all {
        state.t1.jobs.get(TST).status == Completed,
        state.t1.jobs.get(TST).concl == Skipped,
        state.t1.jobs.get(DEP).concl == Skipped,
        not(deciding(state, IFinish(2))),
        deciding(state, IFinish(1)),
      }),
      state' = state,
    })
    .then(read("o1", IFinish(1)))
    .then(write("o1"))
    .then(all { assert(state.t1.runs.get(1).concl == Failure), state' = state })

  /// UC3. A need succeeded: its dependent is queued, the gated one waits for approval.
  /// @conformance
  run needMetRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(started("r1"))
    .then(result("r1", Success))
    .then(read("o1", IAccept({ jk: BLD, seq: 1 })))
    .then(write("o1"))
    .then(all { assert(deciding(state, IPromote(TST)) and deciding(state, IPromote(DEP))), state' = state })
    .then(read("o1", IPromote(TST)))
    .then(write("o1"))
    .then(read("o1", IPromote(DEP)))
    .then(write("o1"))
    .then(all {
      assert(all {
        state.t1.jobs.get(TST).status == Queued,
        state.t1.jobs.get(DEP).status == Waiting,
      }),
      state' = state,
    })

  /// UC4. The runner dies before reporting a step: the lease lapses, the sweep requeues the
  /// job, another runner takes the expired lease over (seq 2) and the job runs there.
  /// @conformance
  run lostBeforeStartRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(crash("r1"))
    .then(all { assert(not(deciding(state, ISweep(BLD)))), state' = state })
    .then(tick)
    .then(tick)
    .then(read("o2", ISweep(BLD)))
    .then(write("o2"))
    .then(sync)
    .then(all {
      assert(all {
        state.t1.jobs.get(BLD).status == Queued,
        state.t1.jobs.get(BLD).holder == "",
        state.t1.jobs.get(BLD).lost == 1,
        state.t2.queue == Set(BLD),
      }),
      state' = state,
    })
    .then(claimRead("r2", BLD))
    .then(claimWrite("r2"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(all {
      assert(all {
        state.t1.jobs.get(BLD).status == InProgress,
        state.t1.jobs.get(BLD).holder == "r2",
        state.t1.jobs.get(BLD).seq == 2,
      }),
      state' = state,
    })

  /// UC4. The runner dies after a step started: the job concludes failure, "runner lost",
  /// and its dependents are skipped.
  /// @conformance
  run lostAfterStartRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(started("r1"))
    .then(crash("r1"))
    .then(tick)
    .then(tick)
    .then(read("o1", ISweep(BLD)))
    .then(write("o1"))
    .then(all {
      assert(all {
        state.t1.jobs.get(BLD).status == Completed,
        state.t1.jobs.get(BLD).concl == Failure,
        state.t1.jobs.get(BLD).by == "",
        state.t1.jobs.get(BLD).lost == 1,
      }),
      state' = state,
    })
    .then(read("o1", IPromote(TST)))
    .then(write("o1"))
    .then(all { assert(state.t1.jobs.get(TST).concl == Skipped), state' = state })

  /// UC4. A runner that only stalled reports late: its lease was taken over and the job runs
  /// under seq 2, so its result (seq 1) is not accepted, and the new holder's is.
  /// @conformance
  run lateResultRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(tick)
    .then(tick)
    .then(read("o2", ISweep(BLD)))
    .then(write("o2"))
    .then(sync)
    .then(claimRead("r2", BLD))
    .then(claimWrite("r2"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(started("r1"))
    .then(result("r1", Success))
    .then(all {
      assert(all {
        not(deciding(state, IAccept({ jk: BLD, seq: 1 }))),
        state.t2.results.get({ jk: BLD, seq: 1 }).runner == "r1",
        // the job is the new holder's
        state.t1.jobs.get(BLD).holder == "r2",
      }),
      state' = state,
    })
    .then(started("r2"))
    .then(result("r2", Success))
    .then(read("o1", IAccept({ jk: BLD, seq: 2 })))
    .then(write("o1"))
    .then(all {
      assert(all {
        state.t1.jobs.get(BLD).concl == Success,
        state.t1.jobs.get(BLD).by == "r2",
        state.t1.jobs.get(BLD).byseq == 2,
      }),
      state' = state,
    })

  /// UC4 and UC9. A runner reports success after the sweep concluded its job "runner lost":
  /// the job stays failed and so does its check.
  /// @conformance
  run lateSuccessIsNotACheckRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(started("r1"))
    .then(tick)
    .then(tick)
    .then(read("o1", ISweep(BLD)))
    .then(write("o1"))
    .then(result("r1", Success))
    .then(read("o2", ICheck(BLD)))
    .then(write("o2"))
    .then(all {
      assert(all {
        state.t1.jobs.get(BLD).concl == Failure,
        state.t1.checks.get(CK_BUILD_1).state == Failure,
        not(deciding(state, IAccept({ jk: BLD, seq: 1 }))),
      }),
      state' = state,
    })

  /// A newer push cancels run 1 while a sweep requeues its running job (the runner's lease
  /// lapsed). The sweep lands first: the cancel commit, which read that job, finds it changed
  /// and conflicts; read again, it cancels the requeued job too. (Without the guard on the
  /// jobs it read, the cancelled run would keep a queued job: found by simulation.)
  /// @conformance
  run cancelRacesSweepRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(tick)
    .then(tick)
    .then(read("o2", ISweep(BLD)))
    .then(fire("push2"))
    .then(read("o1", ITrigger("push2")))
    .then(write("o2"))
    .then(write("o1"))
    .then(all {
      assert(all {
        state.t1.jobs.get(BLD).status == Queued,
        not(state.t1.runs.get(1).cancel),
        state.t1.runs.keys() == Set(1),
      }),
      state' = state,
    })
    .then(read("o1", ITrigger("push2")))
    .then(write("o1"))
    .then(all {
      assert(all {
        state.t1.runs.get(1).cancel,
        state.t1.jobs.get(BLD).concl == Cancelled,
        cancelledIsIdle,
        oneActivePerGroup,
      }),
      state' = state,
    })

  /// A failed run is rerun while a newer push takes the group over. Both read the run done
  /// (so the push has nothing to cancel); the rerun lands first, the push finds the run
  /// changed and conflicts, and read again it cancels the rerun. (Without the guard on the
  /// previous holder, two runs would be active in the group: found by simulation.)
  /// @conformance
  run rerunRacesPushRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(started("r1"))
    .then(result("r1", Failure))
    .then(read("o1", IAccept({ jk: BLD, seq: 1 })))
    .then(write("o1"))
    .then(read("o1", IPromote(TST)))
    .then(write("o1"))
    .then(read("o1", IPromote(DEP)))
    .then(write("o1"))
    .then(read("o1", IFinish(1)))
    .then(write("o1"))
    .then(fire("push2"))
    .then(read("o1", IRerun(1)))
    .then(read("o2", ITrigger("push2")))
    .then(write("o1"))
    .then(write("o2"))
    .then(all {
      assert(all {
        state.t1.runs.keys() == Set(1),
        state.t1.runs.get(1).att == 2,
        not(state.t1.runs.get(1).done),
        state.pend.get("o2") == NoPend,
      }),
      state' = state,
    })
    .then(read("o2", ITrigger("push2")))
    .then(write("o2"))
    .then(all {
      assert(all {
        state.t1.runs.get(1).cancel,
        not(state.t1.runs.get(2).cancel),
        state.t1.groups == Map("main" -> 2),
        oneActivePerGroup,
        cancelledIsIdle,
      }),
      state' = state,
    })

  /// Two runners read the queue entry at once: the lease write has one winner, and the other
  /// finds the lease changed and leaves the job.
  /// @conformance
  run leaseRaceRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimRead("r2", BLD))
    .then(claimWrite("r1"))
    .then(claimWrite("r2"))
    .then(all {
      assert(all {
        state.t2.leases.get(BLD).holder == "r1",
        state.runners.get("r2").ph == RIdle,
        leaseExclusive,
      }),
      state' = state,
    })

  /// Two replicas accept the same result: the second finds the job changed and leaves it.
  /// @conformance
  run twoReplicasAcceptRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(started("r1"))
    .then(result("r1", Success))
    .then(read("o1", IAccept({ jk: BLD, seq: 1 })))
    .then(read("o2", IAccept({ jk: BLD, seq: 1 })))
    .then(write("o1"))
    .then(write("o2"))
    .then(all {
      assert(all {
        state.t1.jobs.get(BLD).concl == Success,
        state.pend.get("o2") == NoPend,
      }),
      state' = state,
    })

  /// A replica read a queued job a runner held, then a newer push cancelled its run: its
  /// write finds the job changed, and the cancelled job is not started.
  /// @conformance
  run staleStartRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(fire("push2"))
    .then(read("o2", ITrigger("push2")))
    .then(write("o2"))
    .then(write("o1"))
    .then(all {
      assert(all {
        state.t1.jobs.get(BLD).status == Completed,
        state.t1.jobs.get(BLD).concl == Cancelled,
        state.t1.runs.get(1).cancel,
      }),
      state' = state,
    })

  /// UC7. The deploy waits for an approval; the run's own actor (alice) does not count; bob's
  /// does, and the job is queued.
  /// @conformance
  run gateRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(started("r1"))
    .then(result("r1", Success))
    .then(read("o1", IAccept({ jk: BLD, seq: 1 })))
    .then(write("o1"))
    .then(read("o1", IPromote(DEP)))
    .then(write("o1"))
    .then(all { assert(state.t1.jobs.get(DEP).status == Waiting), state' = state })
    .then(approve("alice", 1, 1))
    .then(all { assert(not(deciding(state, IPromote(DEP)))), state' = state })
    .then(approve("bob", 1, 1))
    .then(read("o2", IPromote(DEP)))
    .then(write("o2"))
    .then(sync)
    .then(all {
      assert(all {
        state.t1.jobs.get(DEP).status == Queued,
        state.t2.queue.contains(DEP),
        gatesHold,
      }),
      state' = state,
    })

  /// UC8. A cron slot comes due and two replicas fire it: one run (the slot's claim is
  /// create-only), and a replica that reads again finds the slot claimed.
  /// @conformance
  run cronSlotRun = init
    .then(fire("cron1"))
    .then(read("o1", ITrigger("cron1")))
    .then(read("o2", ITrigger("cron1")))
    .then(write("o1"))
    .then(write("o2"))
    .then(all {
      assert(all {
        state.t1.runs.keys() == Set(1),
        state.t1.claims == Map("cron1" -> 1),
        // a cron run is in no concurrency group
        state.t1.groups == Map(),
        not(deciding(state, ITrigger("cron1"))),
      }),
      state' = state,
    })

  /// UC6. A cancelled run is rerun: attempt 2 starts, attempt 1 is untouched (frozen), and
  /// the rerun takes its group back, cancelling the newer run.
  /// @conformance
  run rerunRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(fire("push2"))
    .then(read("o1", ITrigger("push2")))
    .then(write("o1"))
    .then(read("o1", IFinish(1)))
    .then(write("o1"))
    .then(all {
      assert(all {
        state.t1.runs.get(1).concl == Cancelled,
        not(deciding(state, IRerun(2))),
      }),
      state' = state,
    })
    .then(read("o2", IRerun(1)))
    .then(write("o2"))
    .then(all {
      assert(all {
        state.t1.runs.get(1).att == 2,
        not(state.t1.runs.get(1).done),
        state.t1.jobs.get(BLD).concl == Cancelled,
        state.t1.jobs.get(jb(1, 2, "build")).status == Queued,
        state.t1.groups == Map("main" -> 1),
        state.t1.runs.get(2).cancel,
        oneActivePerGroup,
      }),
      state' = state,
    })

  /// UC1, UC3, UC6, UC7 together. build succeeds, test fails, deploy waits for bob and
  /// succeeds; the run fails. Rerunning the failed jobs copies build and deploy into attempt 2
  /// (no new approval) and queues test again; attempt 1 stays as it was.
  /// @conformance
  run rerunFailedRun = init
    .then(fire("push1"))
    .then(read("o1", ITrigger("push1")))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r1", BLD))
    .then(claimWrite("r1"))
    .then(read("o1", IStart(BLD)))
    .then(write("o1"))
    .then(started("r1"))
    .then(result("r1", Success))
    .then(read("o1", IAccept({ jk: BLD, seq: 1 })))
    .then(write("o1"))
    .then(read("o1", IPromote(TST)))
    .then(write("o1"))
    .then(read("o1", IPromote(DEP)))
    .then(write("o1"))
    .then(sync)
    .then(claimRead("r2", TST))
    .then(claimWrite("r2"))
    .then(read("o1", IStart(TST)))
    .then(write("o1"))
    .then(started("r2"))
    .then(result("r2", Failure))
    .then(read("o1", IAccept({ jk: TST, seq: 1 })))
    .then(write("o1"))
    .then(approve("bob", 1, 1))
    .then(read("o2", IPromote(DEP)))
    .then(write("o2"))
    .then(sync)
    .then(claimRead("r3", DEP))
    .then(claimWrite("r3"))
    .then(read("o1", IStart(DEP)))
    .then(write("o1"))
    .then(started("r3"))
    .then(result("r3", Success))
    .then(read("o1", IAccept({ jk: DEP, seq: 1 })))
    .then(write("o1"))
    .then(read("o1", IFinish(1)))
    .then(write("o1"))
    .then(read("o1", ICheck(TST)))
    .then(write("o1"))
    .then(all {
      assert(all {
        state.t1.runs.get(1).concl == Failure,
        state.t1.checks.get({ sha: 1, job: "test" }).state == Failure,
      }),
      state' = state,
    })
    .then(read("o2", IRerun(1)))
    .then(write("o2"))
    .then(read("o2", IPromote(jb(1, 2, "test"))))
    .then(write("o2"))
    .then(read("o2", ICheck(jb(1, 2, "build"))))
    .then(write("o2"))
    .then(all {
      assert(all {
        state.t1.runs.get(1).att == 2,
        state.t1.jobs.get(jb(1, 2, "build")).copied,
        state.t1.jobs.get(jb(1, 2, "build")).concl == Success,
        state.t1.jobs.get(jb(1, 2, "deploy")).copied,
        state.t1.jobs.get(jb(1, 2, "test")).status == Queued,
        // attempt 1 is as it was
        state.t1.jobs.get(TST).concl == Failure,
        state.t1.jobs.get(TST).by == "r2",
        state.t1.jobs.get(DEP).by == "r3",
        state.t1.checks.get(CK_BUILD_1) == { rid: 1, att: 2, state: Success },
        state.t1.checks.get({ sha: 1, job: "test" }).att == 1,
      }),
      state' = state,
    })
}
