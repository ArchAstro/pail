// -*- mode: Bluespec; -*-
/// What every enforcement point of a branch policy must do (`policy.qnt`): parse these
/// documents, then answer these requests. `tests/policy_refinement.rs` checks
/// `Policy::decide`; the ArchDev gateway checks its Go port with the generated Go adapter.
/// Each answer is the model's; the assertions spell out the expected ones.
module policyScenarios {
  import policy.* from "./policy"

  pure val STD: Doc = Map(
    "version" -> FInt(1),
    "admins" -> FTexts(["role:admin"]),
    "gc" -> FTexts(["role:gc"]),
    "branches" -> FRules(Map(
      ["m", "a", "i", "n"] /* main */ -> RuleObject(Map("writers" -> RTexts(["lander"]), "approvals" -> RInt(1), "required_checks" -> RTexts(["test"]))),
      ["r", "e", "l", "e", "a", "s", "e", "/", "*"] /* release/* */ -> RuleObject(Map("writers" -> RTexts(["lander"]), "approvals" -> RInt(2))),
      ["_", "p", "o", "l", "i", "c", "y"] /* _policy */ -> RuleObject(Map("writers" -> RTexts(["role:admin"]))),
      ["*"] /* * */ -> RuleObject(Map("writers" -> RTexts(["*"]))),
    )),
  )

  pure val STRICT: Doc = Map(
    "version" -> FInt(1),
    "branches" -> FRules(Map(
      ["m", "a", "i", "n"] /* main */ -> RuleObject(Map("writers" -> RTexts(["lander"]))),
    )),
  )

  pure val PRIVATE: Doc = Map(
    "version" -> FInt(1),
    "readers" -> FTexts(["group:eng", "user:bob"]),
    "admins" -> FTexts(["user:root"]),
    "gc" -> FTexts(["user:gcbot"]),
    "branches" -> FRules(Map(
      ["m", "a", "i", "n"] /* main */ -> RuleObject(Map("writers" -> RTexts(["group:eng"]))),
      ["p", "r", "/", "*"] /* pr/* */ -> RuleObject(Map("writers" -> RTexts(["*"]))),
    )),
  )

  pure val NESTED: Doc = Map(
    "version" -> FInt(1),
    "branches" -> FRules(Map(
      ["p", "r", "/", "*"] /* pr/* */ -> RuleObject(Map("writers" -> RTexts(["group:eng"]))),
      ["p", "r", "/", "a", "/", "*"] /* pr/a/* */ -> RuleObject(Map("writers" -> RTexts(["user:alice"]))),
      ["p", "r", "/", "a", "/", "b"] /* pr/a/b */ -> RuleObject(Map("writers" -> RTexts(["user:bob"]))),
      ["r", "e", "l", "e", "a", "s", "e", "/", "*"] /* release/* */ -> RuleObject(Map("writers" -> RTexts(["lander"]))),
      ["r", "e", "l", "e", "a", "s", "e", "/", "h", "o", "t", "f", "i", "x"] /* release/hotfix */ -> RuleObject(Map("writers" -> RTexts(["*"]))),
    )),
  )

  pure val EMPTY: Doc = Map(
    "version" -> FInt(1),
  )

  pure val CASE_SENSITIVE: Doc = Map(
    "version" -> FInt(1),
    "Readers" -> FTexts(["user:nobody-else"]),
    "Admins" -> FTexts(["*"]),
    "branches" -> FRules(Map(
      ["*"] /* * */ -> RuleObject(Map("Writers" -> RTexts(["*"]))),
    )),
  )

  pure val CLOSED_READERS: Doc = Map(
    "version" -> FInt(1),
    "readers" -> FTexts([]),
    "branches" -> FRules(Map(
      ["*"] /* * */ -> RuleObject(Map("writers" -> RTexts(["*"]))),
    )),
  )

  /// Requests under the `std` policy (part 1 of 6).
  /// @conformance
  run std1Run = init
    .then(load(STD))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Denied), state' = state })  // protected exact rule beats *
    .then(decide({ principal: "lander", groups: Set() }, Write, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "r", "e", "l", "e", "a", "s", "e", "/", "2", "0", "2", "6"] /* refs/release/2026 */))
    .then(all { assert(state.answer == Denied), state' = state })  // glob release/* beats *
    .then(decide({ principal: "lander", groups: Set() }, Write, ["r", "e", "f", "s", "/", "r", "e", "l", "e", "a", "s", "e", "/", "2", "0", "2", "6"] /* refs/release/2026 */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "lander", groups: Set() }, Write, ["r", "e", "f", "s", "/", "r", "e", "l", "e", "a", "s", "e", "/", "a", "/", "b"] /* refs/release/a/b */))
    .then(all { assert(state.answer == Allowed), state' = state })  // glob prefix covers nested names
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "p", "r", "/", "h", "e", "l", "l", "o"] /* refs/pr/hello */))
    .then(all { assert(state.answer == Allowed), state' = state })  // fallback * rule
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "p", "r", "/", "a", "/", "b"] /* refs/pr/a/b */))
    .then(all { assert(state.answer == Allowed), state' = state })  // nested branch name via *
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "a", "l", "i", "c", "e", "/", "w", "i", "p"] /* refs/alice/wip */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "_", "p", "o", "l", "i", "c", "y"] /* refs/_policy */))
    .then(all { assert(state.answer == Denied), state' = state })  // _policy is protected by its own rule
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Write, ["r", "e", "f", "s", "/", "_", "p", "o", "l", "i", "c", "y"] /* refs/_policy */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Delete, ["r", "e", "f", "s", "/", "p", "r", "/", "h", "e", "l", "l", "o"] /* refs/pr/hello */))
    .then(all { assert(state.answer == Allowed), state' = state })  // ref delete needs branch writer, same as write
    .then(decide({ principal: "user:alice", groups: Set() }, Delete, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Denied), state' = state })

  /// Requests under the `std` policy (part 2 of 6).
  /// @conformance
  run std2Run = init
    .then(load(STD))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "lander", groups: Set() }, Delete, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["i", "n", "t", "e", "n", "t", "s", "/", "m", "a", "i", "n", "/", "0", "0", "0", "1", "-", "a", "b", ".", "j", "s", "o", "n"] /* intents/main/0001-ab.json */))
    .then(all { assert(state.answer == Denied), state' = state })  // no combining into a protected branch
    .then(decide({ principal: "lander", groups: Set() }, Write, ["i", "n", "t", "e", "n", "t", "s", "/", "m", "a", "i", "n", "/", "0", "0", "0", "1", "-", "a", "b", ".", "j", "s", "o", "n"] /* intents/main/0001-ab.json */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["i", "n", "t", "e", "n", "t", "s", "/", "p", "r", "/", "h", "e", "l", "l", "o", "/", "1", ".", "j", "s", "o", "n"] /* intents/pr/hello/1.json */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["i", "n", "t", "e", "n", "t", "s", "/", "p", "r", "/", "a", "/", "b", "/", "1", ".", "j", "s", "o", "n"] /* intents/pr/a/b/1.json */))
    .then(all { assert(state.answer == Allowed), state' = state })  // nested branch pr/a/b: branch is all but last segment
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["i", "n", "t", "e", "n", "t", "s", "/", "r", "e", "l", "e", "a", "s", "e", "/", "2", "0", "2", "6", "/", "1", ".", "j", "s", "o", "n"] /* intents/release/2026/1.json */))
    .then(all { assert(state.answer == Denied), state' = state })  // intents/release/2026 -> branch release/2026
    .then(decide({ principal: "lander", groups: Set() }, Write, ["i", "n", "t", "e", "n", "t", "s", "/", "r", "e", "l", "e", "a", "s", "e", "/", "2", "0", "2", "6", "/", "1", ".", "j", "s", "o", "n"] /* intents/release/2026/1.json */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "l", "o", "g", "/", "m", "a", "i", "n", "/", "1", "7", "0", "0", "0", "0", "0", "0", "0", "0"] /* reflog/main/1700000000 */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "lander", groups: Set() }, Write, ["r", "e", "f", "l", "o", "g", "/", "m", "a", "i", "n", "/", "1", "7", "0", "0", "0", "0", "0", "0", "0", "0"] /* reflog/main/1700000000 */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "l", "o", "g", "/", "p", "r", "/", "a", "/", "b", "/", "1", "7", "0", "0", "0", "0", "0", "0", "0", "0"] /* reflog/pr/a/b/1700000000 */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "l", "o", "g", "/", "_", "p", "o", "l", "i", "c", "y", "/", "1", "7", "0", "0", "0", "0", "0", "0", "0", "0"] /* reflog/_policy/1700000000 */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Write, ["r", "e", "f", "l", "o", "g", "/", "_", "p", "o", "l", "i", "c", "y", "/", "1", "7", "0", "0", "0", "0", "0", "0", "0", "0"] /* reflog/_policy/1700000000 */))
    .then(all { assert(state.answer == Allowed), state' = state })

  /// Requests under the `std` policy (part 3 of 6).
  /// @conformance
  run std3Run = init
    .then(load(STD))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["i", "n", "t", "e", "n", "t", "s", "/", "m", "a", "i", "n"] /* intents/main */))
    .then(all { assert(state.answer == Allowed), state' = state })  // no '/' after the prefix: branch is empty, which only the * rule matches (pail quirk, kept identical)
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["c", "o", "m", "m", "i", "t", "s", "/", "a", "b", "c"] /* commits/abc */))
    .then(all { assert(state.answer == Allowed), state' = state })  // create-only content
    .then(decide({ principal: "user:alice", groups: Set() }, Delete, ["c", "o", "m", "m", "i", "t", "s", "/", "a", "b", "c"] /* commits/abc */))
    .then(all { assert(state.answer == Denied), state' = state })  // history is create-only
    .then(decide({ principal: "lander", groups: Set() }, Delete, ["c", "o", "m", "m", "i", "t", "s", "/", "a", "b", "c"] /* commits/abc */))
    .then(all { assert(state.answer == Denied), state' = state })  // writers are not gc
    .then(decide({ principal: "svc", groups: Set("role:gc") }, Delete, ["c", "o", "m", "m", "i", "t", "s", "/", "a", "b", "c"] /* commits/abc */))
    .then(all { assert(state.answer == Allowed), state' = state })  // gc may delete
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Delete, ["c", "o", "m", "m", "i", "t", "s", "/", "a", "b", "c"] /* commits/abc */))
    .then(all { assert(state.answer == Denied), state' = state })  // admins are not gc
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["p", "a", "g", "e", "s", "/", "a", "b", "c"] /* pages/abc */))
    .then(all { assert(state.answer == Allowed), state' = state })  // create-only content
    .then(decide({ principal: "user:alice", groups: Set() }, Delete, ["p", "a", "g", "e", "s", "/", "a", "b", "c"] /* pages/abc */))
    .then(all { assert(state.answer == Denied), state' = state })  // history is create-only
    .then(decide({ principal: "lander", groups: Set() }, Delete, ["p", "a", "g", "e", "s", "/", "a", "b", "c"] /* pages/abc */))
    .then(all { assert(state.answer == Denied), state' = state })  // writers are not gc
    .then(decide({ principal: "svc", groups: Set("role:gc") }, Delete, ["p", "a", "g", "e", "s", "/", "a", "b", "c"] /* pages/abc */))
    .then(all { assert(state.answer == Allowed), state' = state })  // gc may delete
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Delete, ["p", "a", "g", "e", "s", "/", "a", "b", "c"] /* pages/abc */))
    .then(all { assert(state.answer == Denied), state' = state })  // admins are not gc
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Allowed), state' = state })  // create-only content

  /// Requests under the `std` policy (part 4 of 6).
  /// @conformance
  run std4Run = init
    .then(load(STD))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Delete, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Denied), state' = state })  // history is create-only
    .then(decide({ principal: "lander", groups: Set() }, Delete, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Denied), state' = state })  // writers are not gc
    .then(decide({ principal: "svc", groups: Set("role:gc") }, Delete, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Allowed), state' = state })  // gc may delete
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Delete, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Denied), state' = state })  // admins are not gc
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["c", "h", "u", "n", "k", "s", "/", "a", "b", "c"] /* chunks/abc */))
    .then(all { assert(state.answer == Allowed), state' = state })  // create-only content
    .then(decide({ principal: "user:alice", groups: Set() }, Delete, ["c", "h", "u", "n", "k", "s", "/", "a", "b", "c"] /* chunks/abc */))
    .then(all { assert(state.answer == Denied), state' = state })  // history is create-only
    .then(decide({ principal: "lander", groups: Set() }, Delete, ["c", "h", "u", "n", "k", "s", "/", "a", "b", "c"] /* chunks/abc */))
    .then(all { assert(state.answer == Denied), state' = state })  // writers are not gc
    .then(decide({ principal: "svc", groups: Set("role:gc") }, Delete, ["c", "h", "u", "n", "k", "s", "/", "a", "b", "c"] /* chunks/abc */))
    .then(all { assert(state.answer == Allowed), state' = state })  // gc may delete
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Delete, ["c", "h", "u", "n", "k", "s", "/", "a", "b", "c"] /* chunks/abc */))
    .then(all { assert(state.answer == Denied), state' = state })  // admins are not gc
    .then(decide({ principal: "user:nobody", groups: Set() }, Write, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Allowed), state' = state })  // * writers includes everyone
    .then(decide({ principal: "svc", groups: Set("role:gc") }, Write, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Write, ["H", "E", "A", "D"] /* HEAD */))
    .then(all { assert(state.answer == Allowed), state' = state })  // admin sets the default branch

  /// Requests under the `std` policy (part 5 of 6).
  /// @conformance
  run std5Run = init
    .then(load(STD))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["H", "E", "A", "D"] /* HEAD */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "lander", groups: Set() }, Write, ["H", "E", "A", "D"] /* HEAD */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Delete, ["H", "E", "A", "D"] /* HEAD */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Write, ["c", "o", "n", "f", "i", "g", ".", "j", "s", "o", "n"] /* config.json */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Write, ["l", "o", "c", "k", "/", "x"] /* lock/x */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Delete, ["a", "n", "y", "t", "h", "i", "n", "g"] /* anything */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Write, ["r", "e", "f", "s"] /* refs */))
    .then(all { assert(state.answer == Denied), state' = state })  // 'refs' without slash is not a ref
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Write, ["c", "o", "m", "m", "i", "t", "s", "x", "/", "a", "b", "c"] /* commitsx/abc */))
    .then(all { assert(state.answer == Denied), state' = state })  // prefix must include the slash
    .then(decide({ principal: "user:nobody", groups: Set() }, Read, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Allowed), state' = state })  // default readers *
    .then(decide({ principal: "user:nobody", groups: Set() }, Read, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:nobody", groups: Set() }, Read, [] /*  */))
    .then(all { assert(state.answer == Allowed), state' = state })  // listing the whole repo
    .then(decide({ principal: "user:nobody", groups: Set() }, Write, ["r", "e", "f", "s", "/", "a", "n", "y", "t", "h", "i", "n", "g"] /* refs/anything */))
    .then(all { assert(state.answer == Allowed), state' = state })

  /// Requests under the `std` policy (part 6 of 6).
  /// @conformance
  run std6Run = init
    .then(load(STD))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["l", "o", "c", "k", "s", "/", "s", "h", "a", "r", "e", "d", "-", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "1", "-", "0", "0", "0", "0", "a", "b", "c", "d"] /* locks/shared-00000000000000000001-0000abcd */))
    .then(all { assert(state.answer == Allowed), state' = state })  // a writer takes a shared lease
    .then(decide({ principal: "user:alice", groups: Set() }, Delete, ["l", "o", "c", "k", "s", "/", "s", "h", "a", "r", "e", "d", "-", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "1", "-", "0", "0", "0", "0", "a", "b", "c", "d"] /* locks/shared-00000000000000000001-0000abcd */))
    .then(all { assert(state.answer == Allowed), state' = state })  // and releases it
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["l", "o", "c", "k", "s", "/", "e", "x", "c", "l", "u", "s", "i", "v", "e", "-", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "1", "-", "0", "0", "0", "0", "a", "b", "c", "d"] /* locks/exclusive-00000000000000000001-0000abcd */))
    .then(all { assert(state.answer == Denied), state' = state })  // only gc takes the exclusive lease
    .then(decide({ principal: "svc", groups: Set("role:gc") }, Write, ["l", "o", "c", "k", "s", "/", "e", "x", "c", "l", "u", "s", "i", "v", "e", "-", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "1", "-", "0", "0", "0", "0", "a", "b", "c", "d"] /* locks/exclusive-00000000000000000001-0000abcd */))
    .then(all { assert(state.answer == Allowed), state' = state })  // gc takes the exclusive lease
    .then(decide({ principal: "svc", groups: Set("role:gc") }, Delete, ["l", "o", "c", "k", "s", "/", "e", "x", "c", "l", "u", "s", "i", "v", "e", "-", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "1", "-", "0", "0", "0", "0", "a", "b", "c", "d"] /* locks/exclusive-00000000000000000001-0000abcd */))
    .then(all { assert(state.answer == Allowed), state' = state })  // and releases it
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["l", "o", "c", "k", "s", "/", "o", "t", "h", "e", "r"] /* locks/other */))
    .then(all { assert(state.answer == Denied), state' = state })  // lease names are shared- or exclusive-

  /// Requests under the `strict` policy.
  /// @conformance
  run strictRun = init
    .then(load(STRICT))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["i", "n", "t", "e", "n", "t", "s", "/", "m", "a", "i", "n"] /* intents/main */))
    .then(all { assert(state.answer == Denied), state' = state })  // same shape under a policy without a * rule
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Denied), state' = state })  // alice writes no branch
    .then(decide({ principal: "lander", groups: Set() }, Write, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "lander", groups: Set() }, Write, ["c", "o", "m", "m", "i", "t", "s", "/", "a", "b", "c"] /* commits/abc */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "svc", groups: Set("role:gc") }, Delete, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Denied), state' = state })  // no gc list in strict
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "f", "e", "a", "t", "u", "r", "e"] /* refs/feature */))
    .then(all { assert(state.answer == Denied), state' = state })  // unnamed branches are writable by nobody
    .then(decide({ principal: "lander", groups: Set() }, Write, ["r", "e", "f", "s", "/", "f", "e", "a", "t", "u", "r", "e"] /* refs/feature */))
    .then(all { assert(state.answer == Denied), state' = state })  // even the main writer
    .then(decide({ principal: "lander", groups: Set() }, Write, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "lander", groups: Set() }, Write, ["r", "e", "f", "s", "/", "m", "a", "i", "n", "x"] /* refs/mainx */))
    .then(all { assert(state.answer == Denied), state' = state })  // exact rules do not prefix-match
    .then(decide({ principal: "lander", groups: Set() }, Write, ["r", "e", "f", "s", "/", "m", "a", "i", "n", "/", "s", "u", "b"] /* refs/main/sub */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["i", "n", "t", "e", "n", "t", "s", "/", "f", "e", "a", "t", "u", "r", "e", "/", "1", ".", "j", "s", "o", "n"] /* intents/feature/1.json */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["l", "o", "c", "k", "s", "/", "s", "h", "a", "r", "e", "d", "-", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "1", "-", "0", "0", "0", "0", "a", "b", "c", "d"] /* locks/shared-00000000000000000001-0000abcd */))
    .then(all { assert(state.answer == Denied), state' = state })  // a non-writer takes no lease

  /// Requests under the `private` policy (part 1 of 2).
  /// @conformance
  run private1Run = init
    .then(load(PRIVATE))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:nobody", groups: Set() }, Write, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Allowed), state' = state })  // pr/* is open to *
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Write, ["H", "E", "A", "D"] /* HEAD */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["H", "E", "A", "D"] /* HEAD */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Read, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Denied), state' = state })  // not in readers
    .then(decide({ principal: "user:bob", groups: Set("group:eng") }, Read, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Allowed), state' = state })  // principal named in readers
    .then(decide({ principal: "user:carol", groups: Set("group:eng") }, Read, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Allowed), state' = state })  // group named in readers
    .then(decide({ principal: "user:alice", groups: Set("group:eng") }, Read, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:nobody", groups: Set() }, Read, [] /*  */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Read, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Denied), state' = state })  // admins are not implicitly readers
    .then(decide({ principal: "user:bob", groups: Set("group:eng") }, Write, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Allowed), state' = state })  // bob is in group:eng
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "p", "r", "/", "x"] /* refs/pr/x */))
    .then(all { assert(state.answer == Allowed), state' = state })

  /// Requests under the `private` policy (part 2 of 2).
  /// @conformance
  run private2Run = init
    .then(load(PRIVATE))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:gcbot", groups: Set() }, Delete, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:gcbot", groups: Set() }, Write, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Allowed), state' = state })  // pr/* is open
    .then(decide({ principal: "user:alice", groups: Set() }, Delete, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Denied), state' = state })

  /// Requests under the `nested` policy (part 1 of 2).
  /// @conformance
  run nested1Run = init
    .then(load(NESTED))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:carol", groups: Set("group:eng") }, Write, ["r", "e", "f", "s", "/", "p", "r", "/", "x"] /* refs/pr/x */))
    .then(all { assert(state.answer == Allowed), state' = state })  // pr/* -> group:eng
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "p", "r", "/", "x"] /* refs/pr/x */))
    .then(all { assert(state.answer == Denied), state' = state })  // alice not in group:eng
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "p", "r", "/", "a", "/", "x"] /* refs/pr/a/x */))
    .then(all { assert(state.answer == Allowed), state' = state })  // pr/a/* is longer than pr/*
    .then(decide({ principal: "user:carol", groups: Set("group:eng") }, Write, ["r", "e", "f", "s", "/", "p", "r", "/", "a", "/", "x"] /* refs/pr/a/x */))
    .then(all { assert(state.answer == Denied), state' = state })  // longest prefix wins, eng no longer applies
    .then(decide({ principal: "user:bob", groups: Set("group:eng") }, Write, ["r", "e", "f", "s", "/", "p", "r", "/", "a", "/", "b"] /* refs/pr/a/b */))
    .then(all { assert(state.answer == Allowed), state' = state })  // exact beats glob
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "p", "r", "/", "a", "/", "b"] /* refs/pr/a/b */))
    .then(all { assert(state.answer == Denied), state' = state })  // exact pr/a/b names only bob
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "p", "r", "/", "a", "/", "b", "/", "c"] /* refs/pr/a/b/c */))
    .then(all { assert(state.answer == Allowed), state' = state })  // pr/a/b is exact: pr/a/b/c falls to pr/a/*
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["i", "n", "t", "e", "n", "t", "s", "/", "p", "r", "/", "a", "/", "b", "/", "c", "/", "1", ".", "j", "s", "o", "n"] /* intents/pr/a/b/c/1.json */))
    .then(all { assert(state.answer == Allowed), state' = state })  // branch pr/a/b/c -> pr/a/*
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["i", "n", "t", "e", "n", "t", "s", "/", "p", "r", "/", "a", "/", "b", "/", "1", ".", "j", "s", "o", "n"] /* intents/pr/a/b/1.json */))
    .then(all { assert(state.answer == Denied), state' = state })  // branch pr/a/b -> exact rule, bob only
    .then(decide({ principal: "user:bob", groups: Set("group:eng") }, Write, ["i", "n", "t", "e", "n", "t", "s", "/", "p", "r", "/", "a", "/", "b", "/", "1", ".", "j", "s", "o", "n"] /* intents/pr/a/b/1.json */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "l", "o", "g", "/", "p", "r", "/", "a", "/", "b", "/", "1", "7"] /* reflog/pr/a/b/17 */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "user:bob", groups: Set("group:eng") }, Write, ["r", "e", "f", "l", "o", "g", "/", "p", "r", "/", "a", "/", "b", "/", "1", "7"] /* reflog/pr/a/b/17 */))
    .then(all { assert(state.answer == Allowed), state' = state })

  /// Requests under the `nested` policy (part 2 of 2).
  /// @conformance
  run nested2Run = init
    .then(load(NESTED))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:nobody", groups: Set() }, Write, ["r", "e", "f", "s", "/", "r", "e", "l", "e", "a", "s", "e", "/", "h", "o", "t", "f", "i", "x"] /* refs/release/hotfix */))
    .then(all { assert(state.answer == Allowed), state' = state })  // exact release/hotfix beats release/*
    .then(decide({ principal: "user:nobody", groups: Set() }, Write, ["r", "e", "f", "s", "/", "r", "e", "l", "e", "a", "s", "e", "/", "o", "t", "h", "e", "r"] /* refs/release/other */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "lander", groups: Set() }, Write, ["r", "e", "f", "s", "/", "r", "e", "l", "e", "a", "s", "e", "/", "o", "t", "h", "e", "r"] /* refs/release/other */))
    .then(all { assert(state.answer == Allowed), state' = state })
    .then(decide({ principal: "lander", groups: Set() }, Write, ["r", "e", "f", "s", "/", "r", "e", "l", "e", "a", "s", "e", "/", "h", "o", "t", "f", "i", "x", "2"] /* refs/release/hotfix2 */))
    .then(all { assert(state.answer == Allowed), state' = state })  // release/hotfix is exact, hotfix2 uses release/*
    .then(decide({ principal: "user:nobody", groups: Set() }, Write, ["r", "e", "f", "s", "/", "r", "e", "l", "e", "a", "s", "e", "/", "h", "o", "t", "f", "i", "x", "2"] /* refs/release/hotfix2 */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "lander", groups: Set() }, Write, ["r", "e", "f", "s", "/", "o", "t", "h", "e", "r"] /* refs/other */))
    .then(all { assert(state.answer == Denied), state' = state })  // no catch-all
    .then(decide({ principal: "user:nobody", groups: Set() }, Write, ["p", "a", "c", "k", "s", "/", "p", ".", "p", "a", "c", "k"] /* packs/p.pack */))
    .then(all { assert(state.answer == Allowed), state' = state })  // release/hotfix is open to *, so any caller writes a branch
    .then(decide({ principal: "lander", groups: Set() }, Write, ["r", "e", "f", "s", "/", "r", "e", "l", "e", "a", "s", "e", "/"] /* refs/release/ */))
    .then(all { assert(state.answer == Allowed), state' = state })  // branch 'release/' matches release/* (prefix)

  /// Requests under the `empty` policy.
  /// @conformance
  run emptyRun = init
    .then(load(EMPTY))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["p", "a", "c", "k", "s", "/", "x", ".", "p", "a", "c", "k"] /* packs/x.pack */))
    .then(all { assert(state.answer == Denied), state' = state })  // no branch rules: nobody writes
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Denied), state' = state })  // unnamed branch denied
    .then(decide({ principal: "user:alice", groups: Set() }, Read, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Allowed), state' = state })  // readers default to *
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["H", "E", "A", "D"] /* HEAD */))
    .then(all { assert(state.answer == Denied), state' = state })
    .then(decide({ principal: "user:root", groups: Set("role:admin") }, Write, ["H", "E", "A", "D"] /* HEAD */))
    .then(all { assert(state.answer == Denied), state' = state })  // no admins listed

  /// Requests under the `case_sensitive` policy.
  /// @conformance
  run caseSensitiveRun = init
    .then(load(CASE_SENSITIVE))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Read, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Allowed), state' = state })  // field names are case-sensitive: Readers is ignored, readers defaults to everyone
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["H", "E", "A", "D"] /* HEAD */))
    .then(all { assert(state.answer == Denied), state' = state })  // Admins is ignored
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Denied), state' = state })  // Writers is ignored: the rule has no writers

  /// Requests under the `closed_readers` policy.
  /// @conformance
  run closedReadersRun = init
    .then(load(CLOSED_READERS))
    .then(all { assert(state.policy != Invalid), state' = state })
    .then(decide({ principal: "user:alice", groups: Set() }, Read, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Denied), state' = state })  // empty readers list: nobody reads
    .then(decide({ principal: "user:alice", groups: Set() }, Write, ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */))
    .then(all { assert(state.answer == Allowed), state' = state })  // read rule does not affect writes

  /// Documents every enforcement point must refuse.
  /// @conformance
  run invalidPoliciesRun = init
    // {"version": 1, "readers": null}
    .then(load(Map(
      "version" -> FInt(1),
      "readers" -> FNull,
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"version": 1, "admins": null}
    .then(load(Map(
      "version" -> FInt(1),
      "admins" -> FNull,
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"version": 1, "gc": null}
    .then(load(Map(
      "version" -> FInt(1),
      "gc" -> FNull,
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"version": 1, "branches": null}
    .then(load(Map(
      "version" -> FInt(1),
      "branches" -> FNull,
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"version": 1, "branches": {"main": {"writers": null}}}
    .then(load(Map(
      "version" -> FInt(1),
      "branches" -> FRules(Map(
        ["m", "a", "i", "n"] /* main */ -> RuleObject(Map("writers" -> RNull)),
      )),
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"version": 1, "branches": {"main": null}}
    .then(load(Map(
      "version" -> FInt(1),
      "branches" -> FRules(Map(
        ["m", "a", "i", "n"] /* main */ -> RuleNull,
      )),
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"version": 1, "branches": {"main": {"writers": [null]}}}
    .then(load(Map(
      "version" -> FInt(1),
      "branches" -> FRules(Map(
        ["m", "a", "i", "n"] /* main */ -> RuleObject(Map("writers" -> RTextsThenNull([]))),
      )),
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"Version": 1}
    .then(load(Map(
      "Version" -> FInt(1),
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"branches": {}}
    .then(load(Map(
      "branches" -> FRules(Map()),
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"version": 1, "branches": {"a*b": {"writers": ["*"]}}}
    .then(load(Map(
      "version" -> FInt(1),
      "branches" -> FRules(Map(
        ["a", "*", "b"] /* a*b */ -> RuleObject(Map("writers" -> RTexts(["*"]))),
      )),
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"version": 1, "branches": {"*a": {"writers": ["*"]}}}
    .then(load(Map(
      "version" -> FInt(1),
      "branches" -> FRules(Map(
        ["*", "a"] /* *a */ -> RuleObject(Map("writers" -> RTexts(["*"]))),
      )),
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"version": 1, "branches": {"a**": {"writers": ["*"]}}}
    .then(load(Map(
      "version" -> FInt(1),
      "branches" -> FRules(Map(
        ["a", "*", "*"] /* a** */ -> RuleObject(Map("writers" -> RTexts(["*"]))),
      )),
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"version": 1, "branches": {"": {"writers": ["*"]}}}
    .then(load(Map(
      "version" -> FInt(1),
      "branches" -> FRules(Map(
        [] /*  */ -> RuleObject(Map("writers" -> RTexts(["*"]))),
      )),
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"version": 2}
    .then(load(Map(
      "version" -> FInt(2),
    )))
    .then(all { assert(state.policy == Invalid), state' = state })
    // {"version": 0}
    .then(load(Map(
      "version" -> FInt(0),
    )))
    .then(all { assert(state.policy == Invalid), state' = state })

  /// Which op each request is: reads; branch objects written by their writers; content
  /// only created (`If-None-Match: *`), anything else to it rewriting history.
  /// @conformance
  run requestOpsRun = init
    .then(requestOp("GET", ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */, ""))
    .then(all { assert(state.answer == Mapped(Read)), state' = state })
    .then(requestOp("HEAD", ["p", "a", "c", "k", "s", "/", "a", ".", "p", "a", "c", "k"] /* packs/a.pack */, ""))
    .then(all { assert(state.answer == Mapped(Read)), state' = state })
    .then(requestOp("PUT", ["p", "a", "c", "k", "s", "/", "a", ".", "p", "a", "c", "k"] /* packs/a.pack */, "*"))
    .then(all { assert(state.answer == Mapped(Write)), state' = state })
    .then(requestOp("PUT", ["p", "a", "c", "k", "s", "/", "a", ".", "p", "a", "c", "k"] /* packs/a.pack */, ""))
    .then(all { assert(state.answer == Mapped(Delete)), state' = state })
    .then(requestOp("PUT", ["c", "h", "u", "n", "k", "s", "/", "a"] /* chunks/a */, "3"))
    .then(all { assert(state.answer == Mapped(Delete)), state' = state })
    .then(requestOp("DELETE", ["c", "o", "m", "m", "i", "t", "s", "/", "a"] /* commits/a */, ""))
    .then(all { assert(state.answer == Mapped(Delete)), state' = state })
    .then(requestOp("DELETE", ["p", "a", "g", "e", "s", "/", "a"] /* pages/a */, "*"))
    .then(all { assert(state.answer == Mapped(Delete)), state' = state })
    .then(requestOp("PUT", ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */, ""))
    .then(all { assert(state.answer == Mapped(Write)), state' = state })
    .then(requestOp("PUT", ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */, "*"))
    .then(all { assert(state.answer == Mapped(Write)), state' = state })
    .then(requestOp("DELETE", ["r", "e", "f", "s", "/", "m", "a", "i", "n"] /* refs/main */, ""))
    .then(all { assert(state.answer == Mapped(Write)), state' = state })
    .then(requestOp("PUT", ["i", "n", "t", "e", "n", "t", "s", "/", "m", "a", "i", "n", "/", "1", ".", "j", "s", "o", "n"] /* intents/main/1.json */, ""))
    .then(all { assert(state.answer == Mapped(Write)), state' = state })
    .then(requestOp("POST", ["p", "a", "c", "k", "s", "/", "a", ".", "p", "a", "c", "k"] /* packs/a.pack */, ""))
    .then(all { assert(state.answer == Mapped(Write)), state' = state })
    .then(requestOp("PUT", ["l", "o", "c", "k", "s", "/", "s", "h", "a", "r", "e", "d", "-", "1"] /* locks/shared-1 */, ""))
    .then(all { assert(state.answer == Mapped(Write)), state' = state })
    .then(requestOp("DELETE", ["l", "o", "c", "k", "s", "/", "s", "h", "a", "r", "e", "d", "-", "1"] /* locks/shared-1 */, ""))
    .then(all { assert(state.answer == Mapped(Write)), state' = state })
    .then(requestOp("GET", ["H", "E", "A", "D"] /* HEAD */, ""))
    .then(all { assert(state.answer == Mapped(Read)), state' = state })
}

