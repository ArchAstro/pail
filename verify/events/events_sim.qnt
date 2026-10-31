// -*- mode: Bluespec; -*-
/// `events.qnt`'s protocol under random interleavings, with a ghost remembering what the
/// store ever held and in what order the consumer emitted and saved, for the properties
/// that need history: noPhantom, per-key order, cursorMonotone. (The refinement cannot see
/// ghosts, so they stay out of `events.qnt`'s state.) `npm run simulate:events` checks
/// `simSafety` here; `npm run mutate:events` flips each design switch of `events.qnt` and
/// expects a violation.
module eventsSim {
  import events.* from "./events"

  type Ghost = {
    // every state a key ever had
    hist: Set[Ev],
    // the store's counter at the last emitted and the last saved state of each key
    emitG: int -> int,
    saveG: int -> int,
    // an emit or a save went backwards
    orderBad: bool,
    cursorBad: bool,
    // for the witnesses
    resumed: bool,
    rescued: bool,
    dropped: bool,
  }

  var ghost: Ghost

  pure val GHOST0: Ghost = {
    hist: KEYS.map(k => { key: k, obs: Gone }),
    emitG: KEYS.mapBy(k => 0),
    saveG: KEYS.mapBy(k => 0),
    orderBad: false,
    cursorBad: false,
    resumed: false,
    rescued: false,
    dropped: false,
  }

  /// What the step `a`, taking `s` to `t`, did to the ghost.
  pure def ghostAfter(g: Ghost, s: State, a: Act, t: State): Ghost = {
    val hist = g.hist.union(KEYS.map(k => { key: k, obs: storeObs(t, k) }))
    val g1 = { ...g, hist: hist }
    match a {
      | AEmit => match s.pend {
        | Pend(p) => { ...g1, orderBad: g1.orderBad or p.g < g1.emitG.get(p.key), emitG: g1.emitG.set(p.key, p.g) }
        | NoPend => g1
      }
      | ASave => match s.pend {
        | Pend(p) => { ...g1, cursorBad: g1.cursorBad or p.g < g1.saveG.get(p.key), saveG: g1.saveG.set(p.key, p.g) }
        | NoPend => g1
      }
      | AStart(_) => { ...g1, resumed: g1.resumed or isPend(t.pend) }
      | AScan(_) => { ...g1, rescued: g1.rescued or isPend(t.pend) }
      | ADrop(_) => { ...g1, dropped: true }
      | _ => g1
    }
  }

  action simInit = all { state' = INITIAL, ghost' = GHOST0 }

  /// A step's kind: a trace picks among the kinds that can happen, then a step of that kind,
  /// so that the rare (a crash, a lost notification) are not drowned by the many.
  pure def kindOf(a: Act): int = match a {
    | APut(_) => 0
    | ARemove(_) => 1
    | ADrop(_) => 2
    | ARedeliver(_) => 3
    | ANote(_) => 4
    | AScan(_) => 5
    | AStart(_) => 6
    | AMark => 7
    | AEmit => 8
    | ASave => 9
    | AFinish => 10
    | ACrash => 11
    | ARestart => 12
  }

  action simStep = {
    val all_ = acts(state)
    nondet kind = all_.map(kindOf).oneOf()
    nondet a = all_.filter(x => kindOf(x) == kind).oneOf()
    all {
      state' = apply(state, a),
      ghost' = ghostAfter(ghost, state, a, apply(state, a)),
    }
  }

  // ---------- properties that need the ghost ----------

  /// noPhantom: every line reflects a state the key really had.
  val noPhantom: bool = state.out.indices().forall(i => ghost.hist.contains(state.out[i]))

  /// Per-key order: the states emitted for a key never go back in the store's order.
  val perKeyOrder: bool = not(ghost.orderBad)

  /// cursorMonotone: a key's saved state never goes back.
  val cursorMonotone: bool = not(ghost.cursorBad)

  val simSafety: bool = all { safety, noPhantom, perKeyOrder, cursorMonotone }

  // ---------- witnesses: each is violated by a run that reaches the case ----------

  /// A deletion is emitted.
  val neverDeleted: bool = not(state.out.indices().exists(i => state.out[i].obs == Gone))
  /// A state is emitted twice (the consumer crashed between the line and the cursor).
  val neverRepeated: bool = not(state.out.indices().exists(i => state.out.indices().exists(j => i < j and state.out[i] == state.out[j])))
  /// The start-up diff finds something to emit.
  val neverResumed: bool = not(ghost.resumed)
  /// The safety poll finds something to emit.
  val neverRescued: bool = not(ghost.rescued)
  /// A notification is lost.
  val neverDropped: bool = not(ghost.dropped)
}
