// -*- mode: Bluespec; -*-
/// Interleavings the real `pail pr` plugin must follow (`pr.qnt`), write by write:
/// `tests/pr_refinement.rs` runs each against a store whose `put` and `land` calls are held
/// until the scenario releases them. alice opens the PR and is its author; bob reviews and
/// merges; carol interferes. The assertions spell out what each run shows.
module prScenarios {
  import pr.* from "./pr"

  pure val BASE_A_1: Spec = Line({ path: "a", line: 1, side: New, commit: HEAD })
  pure val A_2_AT_1: Spec = Line({ path: "a", line: 2, side: New, commit: 1 })
  pure val ALICE_1: CommentId = { ms: 1, who: "alice" }
  pure val ALICE_2: CommentId = { ms: 2, who: "alice" }
  pure val BOB_1: CommentId = { ms: 1, who: "bob" }
  pure val BOB_2: CommentId = { ms: 2, who: "bob" }
  pure val CAROL_1: CommentId = { ms: 1, who: "carol" }
  pure val APPROVE: Cmd = CmReview({ approve: true, commit: HEAD })

  /// Two clients open a PR at once. Both read no PR, both claim id 1; the second finds it
  /// taken and claims 2: neither overwrites the other.
  /// @conformance
  run concurrentCreateRun = init
    .then(begin("alice", Create(1)))
    .then(begin("bob", Create(2)))
    .then(release("alice"))
    .then(release("bob"))
    .then(release("bob"))
    .then(all {
      assert(all {
        state.prs.keys() == Set(1, 2),
        state.prs.get(1).author == "alice",
        state.prs.get(1).title == 1,
        state.prs.get(2).author == "bob",
        state.prs.get(2).title == 2,
        state.clients.get("bob").res == Acked,
      }),
      state' = state,
    })

  /// Two comments from one person in the same millisecond: the second finds the id taken
  /// and takes the next. Both are kept.
  /// @conformance
  run sameMillisecondRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("alice", CmComment({ ms: 1, body: 1, spec: Top })))
    .then(begin("alice2", CmComment({ ms: 1, body: 2, spec: Top })))
    .then(release("alice"))
    .then(release("alice2"))
    .then(release("alice2"))
    .then(all {
      assert(all {
        state.comments.keys() == Set(ALICE_1, ALICE_2),
        state.comments.get(ALICE_1).body == 1,
        state.comments.get(ALICE_2).body == 2,
      }),
      state' = state,
    })

  /// A line comment, a reply (which takes its parent's line), and a push that changes the
  /// line: the comment and its reply are outdated; a comment on a line the push left is not.
  /// @conformance
  run lineCommentRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", CmComment({ ms: 1, body: 1, spec: BASE_A_1 })))
    .then(release("bob"))
    .then(begin("bob", CmComment({ ms: 2, body: 2, spec: A_2_AT_1 })))
    .then(release("bob"))
    .then(begin("carol", CmComment({ ms: 1, body: 3, spec: Reply(BOB_1) })))
    .then(release("carol"))
    .then(view)
    .then(all { assert(state.viewed == Set()), state' = state })
    .then(push)
    .then(view)
    .then(all {
      assert(all {
        state.viewed == Set(BOB_1, CAROL_1),
        state.comments.get(CAROL_1).anchor == state.comments.get(BOB_1).anchor,
        state.comments.get(CAROL_1).reply == ReplyTo(BOB_1),
        state.comments.get(BOB_2).anchor != NoAnchor,
      }),
      state' = state,
    })

  /// Anchors the plugin must refuse: a line the file lacks, a file the PR did not change at
  /// that commit, a commit that is not the PR's, the old side's missing line, a reply to a
  /// comment that is not there. Nothing is written.
  /// @conformance
  run invalidAnchorRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", CmComment({ ms: 1, body: 1, spec: Line({ path: "a", line: 4, side: New, commit: HEAD }) })))
    .then(all { assert(state.clients.get("bob").res == Refused), state' = state })
    .then(begin("bob", CmComment({ ms: 1, body: 1, spec: Line({ path: "b", line: 1, side: New, commit: HEAD }) })))
    .then(all { assert(state.clients.get("bob").res == Refused), state' = state })
    .then(begin("bob", CmComment({ ms: 1, body: 1, spec: Line({ path: "a", line: 1, side: New, commit: 0 }) })))
    .then(all { assert(state.clients.get("bob").res == Refused), state' = state })
    .then(begin("bob", CmComment({ ms: 1, body: 1, spec: Line({ path: "a", line: 0, side: Old, commit: HEAD }) })))
    .then(all { assert(state.clients.get("bob").res == Refused), state' = state })
    .then(begin("bob", CmComment({ ms: 1, body: 1, spec: Line({ path: "c", line: 1, side: New, commit: HEAD }) })))
    .then(all { assert(state.clients.get("bob").res == Refused), state' = state })
    .then(begin("bob", CmComment({ ms: 1, body: 1, spec: Reply(ALICE_1) })))
    .then(all { assert(state.clients.get("bob").res == Refused), state' = state })
    .then(begin("bob", CmComment({ ms: 1, body: 1, spec: Line({ path: "a", line: 3, side: Old, commit: HEAD }) })))
    .then(release("bob"))
    .then(all {
      assert(all {
        state.comments.keys() == Set(BOB_1),
        state.comments.get(BOB_1).anchor == Anchored({ path: "a", line: 3, side: Old, commit: 1 }),
      }),
      state' = state,
    })

  /// The two sides of a diff have their own lines: `b` gains a line in the second commit, so
  /// its line 4 exists only on the new side, and a comment on it is outdated by the next push
  /// (which rewrites that line) while one on an old line never is.
  /// @conformance
  run sidesRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", CmComment({ ms: 1, body: 1, spec: Line({ path: "b", line: 4, side: New, commit: HEAD }) })))
    .then(all { assert(state.clients.get("bob").res == Refused), state' = state })
    .then(push)
    .then(begin("bob", CmComment({ ms: 1, body: 1, spec: Line({ path: "b", line: 4, side: Old, commit: HEAD }) })))
    .then(all { assert(state.clients.get("bob").res == Refused), state' = state })
    .then(begin("bob", CmComment({ ms: 1, body: 2, spec: Line({ path: "b", line: 4, side: New, commit: HEAD }) })))
    .then(release("bob"))
    .then(begin("bob", CmComment({ ms: 2, body: 3, spec: Line({ path: "b", line: 3, side: Old, commit: HEAD }) })))
    .then(release("bob"))
    .then(push)
    .then(view)
    .then(all {
      assert(all {
        state.viewed == Set(BOB_1),
        state.comments.get(BOB_2).anchor == Anchored({ path: "b", line: 3, side: Old, commit: 2 }),
      }),
      state' = state,
    })

  /// carol's edit read the open PR; bob claims it for a merge before it is written. The
  /// edit finds the record changed, reads it again, and refuses: a PR being merged takes no
  /// edit.
  /// @conformance
  run editVsMergeRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", APPROVE))
    .then(release("bob"))
    .then(ci(1))
    .then(begin("carol", EditTitle(2)))
    .then(begin("bob", Merge))
    .then(release("bob"))
    .then(release("carol"))
    .then(all {
      assert(all {
        state.clients.get("carol").res == Refused,
        state.prs.get(1).title == 1,
        state.prs.get(1).state == Merging({ by: "bob", commit: 1 }),
      }),
      state' = state,
    })
    .then(release("bob"))
    .then(release("bob"))
    .then(all {
      assert(all {
        state.prs.get(1).state == Merged({ commit: 1 }),
        state.prs.get(1).title == 1,
        state.landed == 1,
        state.clients.get("bob").res == Acked,
      }),
      state' = state,
    })

  /// carol's edit lands before bob's claim: the claim finds the record changed, judges the
  /// new one, and claims it, so the edit is in the merged record.
  /// @conformance
  run editBeforeClaimRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", APPROVE))
    .then(release("bob"))
    .then(ci(1))
    .then(begin("carol", EditTitle(2)))
    .then(begin("bob", Merge))
    .then(release("carol"))
    .then(release("bob"))
    .then(release("bob"))
    .then(release("bob"))
    .then(release("bob"))
    .then(all {
      assert(all {
        state.prs.get(1).state == Merged({ commit: 1 }),
        state.prs.get(1).title == 2,
        state.landed == 1,
      }),
      state' = state,
    })

  /// An edit and a ready race with another edit: each reads the record, writes second and
  /// loses the compare-and-swap, reads again and writes. No edit is lost.
  /// @conformance
  run concurrentEditsRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("alice", EditTitle(2)))
    .then(begin("bob", EditBody(2)))
    .then(begin("carol", EditDraft(true)))
    .then(release("alice"))
    .then(release("bob"))
    .then(release("bob"))
    .then(release("carol"))
    .then(release("carol"))
    .then(all {
      assert(all {
        state.prs.get(1).title == 2,
        state.prs.get(1).body == 2,
        state.prs.get(1).draft,
        state.prs.get(1).state == Open,
      }),
      state' = state,
    })

  /// carol read the open PR to edit it; bob merges it completely; carol's write finds the
  /// record changed, reads it merged, and refuses. The merge stays merged.
  /// @conformance
  run editAfterMergeRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", APPROVE))
    .then(release("bob"))
    .then(ci(1))
    .then(begin("carol", EditTitle(2)))
    .then(begin("bob", Merge))
    .then(release("bob"))
    .then(release("bob"))
    .then(release("bob"))
    .then(release("carol"))
    .then(all {
      assert(all {
        state.prs.get(1).state == Merged({ commit: 1 }),
        state.prs.get(1).title == 1,
        state.clients.get("carol").res == Refused,
        state.landed == 1,
      }),
      state' = state,
    })

  /// bob claims the PR; carol's close read it open. The close finds the record changed and
  /// refuses: a PR being merged is not closed under the merge.
  /// @conformance
  run closeAfterClaimRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", APPROVE))
    .then(release("bob"))
    .then(ci(1))
    .then(begin("carol", Close))
    .then(begin("bob", Merge))
    .then(release("bob"))
    .then(release("carol"))
    .then(release("bob"))
    .then(release("bob"))
    .then(all {
      assert(all {
        state.clients.get("carol").res == Refused,
        state.prs.get(1).state == Merged({ commit: 1 }),
        state.landed == 1,
      }),
      state' = state,
    })

  /// carol's close lands first; bob's claim finds the record changed, judges it again and
  /// refuses: a closed PR is never merged.
  /// @conformance
  run closeBeforeClaimRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", APPROVE))
    .then(release("bob"))
    .then(ci(1))
    .then(begin("carol", Close))
    .then(begin("bob", Merge))
    .then(release("carol"))
    .then(release("bob"))
    .then(all {
      assert(all {
        state.clients.get("bob").res == Refused,
        state.prs.get(1).state == Closed,
        state.landed == 0,
      }),
      state' = state,
    })

  /// Judged ready at commit 1; the branch is pushed before the land. The land is of commit 1
  /// or nothing: it finds the head moved, lands nothing, and gives the claim up.
  /// @conformance
  run pushBeforeLandRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", APPROVE))
    .then(release("bob"))
    .then(ci(1))
    .then(begin("bob", Merge))
    .then(release("bob"))
    .then(push)
    .then(release("bob"))
    .then(all {
      assert(all {
        state.landed == 0,
        state.prs.get(1).state == Merging({ by: "bob", commit: 1 }),
        state.tip == 2,
      }),
      state' = state,
    })
    .then(release("bob"))
    .then(all {
      assert(all {
        state.landed == 0,
        state.prs.get(1).state == Open,
        state.clients.get("bob").res == Refused,
      }),
      state' = state,
    })

  /// An approval is of the commit it was given on. After a push, bob's approval of commit 1
  /// does not count (the view says not mergeable, a merge refuses); approving the new head
  /// does, and the merge lands it.
  /// @conformance
  run staleApprovalRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", APPROVE))
    .then(release("bob"))
    .then(ci(1))
    .then(view)
    .then(all { assert(state.viewReady), state' = state })
    .then(push)
    .then(ci(2))
    .then(view)
    .then(all { assert(not(state.viewReady)), state' = state })
    .then(begin("bob", Merge))
    .then(all { assert(state.clients.get("bob").res == Refused), state' = state })
    .then(begin("bob", APPROVE))
    .then(release("bob"))
    .then(view)
    .then(all { assert(state.viewReady and state.reviews.get("bob").commit == 2), state' = state })
    .then(begin("bob", Merge))
    .then(release("bob"))
    .then(release("bob"))
    .then(release("bob"))
    .then(all {
      assert(all {
        state.prs.get(1).state == Merged({ commit: 2 }),
        state.landed == 2,
      }),
      state' = state,
    })

  /// A review names the commit it was read at, which may no longer be the head; one for a
  /// commit that is not the PR's is refused. A reviewer's second review replaces the first.
  /// @conformance
  run reviewCommitRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(push)
    .then(begin("bob", CmReview({ approve: true, commit: 1 })))
    .then(release("bob"))
    .then(all { assert(state.reviews.get("bob") == { approve: true, commit: 1 }), state' = state })
    .then(begin("bob", CmReview({ approve: true, commit: 0 })))
    .then(all { assert(state.clients.get("bob").res == Refused), state' = state })
    .then(begin("bob", CmReview({ approve: false, commit: HEAD })))
    .then(release("bob"))
    .then(all { assert(state.reviews.get("bob") == { approve: false, commit: 2 }), state' = state })

  /// A draft is never merged, and a closed PR is not either; reopening and marking ready
  /// make it mergeable again.
  /// @conformance
  run draftAndClosedRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", APPROVE))
    .then(release("bob"))
    .then(ci(1))
    .then(begin("alice", EditDraft(true)))
    .then(release("alice"))
    .then(begin("bob", Merge))
    .then(all { assert(state.clients.get("bob").res == Refused and state.landed == 0), state' = state })
    .then(begin("alice", Ready(false)))
    .then(release("alice"))
    .then(begin("alice", Close))
    .then(release("alice"))
    .then(begin("bob", Merge))
    .then(all { assert(state.clients.get("bob").res == Refused and state.landed == 0), state' = state })
    .then(begin("alice", Ready(true)))
    .then(all { assert(state.clients.get("alice").res == Refused), state' = state })
    .then(begin("alice", Reopen))
    .then(release("alice"))
    .then(view)
    .then(all { assert(state.prs.get(1).state == Open and not(state.prs.get(1).draft) and state.viewReady), state' = state })
    .then(begin("alice", Reopen))
    .then(all { assert(state.clients.get("alice").res == Refused), state' = state })

  /// Two merges: bob claims; carol's claim read the open PR, finds it claimed and refuses.
  /// Nothing lands twice.
  /// @conformance
  run oneMergeAtATimeRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", APPROVE))
    .then(release("bob"))
    .then(ci(1))
    .then(begin("bob", Merge))
    .then(begin("carol", Merge))
    .then(release("bob"))
    .then(release("carol"))
    .then(all { assert(state.clients.get("carol").res == Refused), state' = state })
    .then(release("bob"))
    .then(release("bob"))
    .then(all {
      assert(all {
        state.prs.get(1).state == Merged({ commit: 1 }),
        state.landed == 1,
      }),
      state' = state,
    })

  /// Once the commits are on the base the PR has no commits of its own: a line comment is
  /// refused (the diff is empty) before the merge is recorded, and comments are not outdated.
  /// @conformance
  run landedRangeRun = init
    .then(begin("alice", Create(1)))
    .then(release("alice"))
    .then(begin("bob", CmComment({ ms: 1, body: 1, spec: BASE_A_1 })))
    .then(release("bob"))
    .then(push)
    .then(begin("bob", APPROVE))
    .then(release("bob"))
    .then(ci(2))
    .then(begin("bob", Merge))
    .then(release("bob"))
    .then(release("bob"))
    .then(begin("carol", CmComment({ ms: 1, body: 2, spec: BASE_A_1 })))
    .then(all { assert(state.clients.get("carol").res == Refused and state.landed == 2), state' = state })
    .then(view)
    .then(all { assert(state.viewed == Set()), state' = state })
    .then(release("bob"))
    .then(view)
    .then(begin("carol", CmComment({ ms: 2, body: 3, spec: Reply(BOB_1) })))
    .then(release("carol"))
    .then(all {
      assert(all {
        state.viewed == Set(),
        state.prs.get(1).state == Merged({ commit: 2 }),
        state.comments.get({ ms: 2, who: "carol" }).reply == ReplyTo(BOB_1),
      }),
      state' = state,
    })
}
