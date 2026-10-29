// -*- mode: Bluespec; -*-
/// Soft relocations: a commit that moves old files of its base into the pack it uploads
/// anyway (`amortize`, README "Amortized compaction"). The relocations are soft: a path the
/// head changed, or already moved, is dropped silently on replay and never conflicts; the
/// commit's own change always lands.
module softScenarios {
  import store.* from "./store"
  import bucket.* from "./bucket"
  import world.* from "./world"
  import combine.* from "./combine"
  import pail.* from "./pail"

  pure val BIG_A: Delta = Map("a" -> "p1")
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
