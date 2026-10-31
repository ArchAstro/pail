// -*- mode: Bluespec; -*-
/// `pail events`: a consumer that turns changes of a bucket into one line each, and survives
/// restarts. Refs (`refs/<branch>`: the version is the commit) and prefix objects
/// (`<repo>.actions/...`: the version is the generation) are the same thing here: a key
/// whose latest write has a version, or that is gone.
///
/// The design: a notification is only a hint. Truth is re-reading the key. The consumer
/// emits a key's state when it differs from the last it emitted, so duplicated and
/// reordered notifications cost a read, and a dropped one costs latency: a safety poll
/// re-reads everything. A cursor, written only after the line is out, lets a restarted
/// consumer diff the store against what it had emitted.
///
/// Steps of one consumer, each atomic in the model:
///
///   readKey   re-read a key (named by a notification, the safety poll or the start-up
///             diff); when it differs from what was last emitted, it becomes pending
///   mark      the cursor records that this key is being emitted (`dirty`)
///   emit      the line is written out (and the consumer remembers it)
///   save      the cursor takes the key's new state and clears the mark (every cursor
///             write is atomic: temp file and rename, or a CAS)
///   finish    the notification that caused the read is acknowledged
///
/// The environment: writers put and remove keys; the subscription holds notifications,
/// which may be dropped, delivered twice and in any order; the consumer may crash between
/// any two steps and restart from its cursor.
///
/// `events_sim.qnt` simulates it; `events_scenarios.qnt` holds the runs the real code is
/// refined against (`tests/events_refinement.rs`).
module events {
  pure val KEYS: Set[int] = Set(1, 2)
  /// The store issues at most this many versions (every put and remove takes one).
  pure val MAX_V = 6

  // The protocol's design choices. Each `true` is the design; `false` is the bug it
  // prevents, and what `npm run mutate:events` shows `safety` (or `simSafety`) catching.
  /// The source has notifications (GCS Pub/Sub). `false`: a poll source, which is the
  /// safety poll alone.
  pure val PUSH = true
  /// Notifications can be lost.
  pure val LOSSY = true
  /// A restarted consumer resumes from its cursor and re-reads every key. `false` (the
  /// first `pail events`): it takes the store's present state as already emitted.
  pure val RESCAN_ON_START = true
  /// The cursor is written after the line is out. `false`: before.
  pure val CURSOR_AFTER_EMIT = true
  /// The cursor records the intent to emit a key before the line is written, and the line's
  /// state after. `false`: only after. Then a crash between the line and the cursor, and
  /// a key that went back to the state the cursor holds, loses the change (a ref reset to
  /// the commit the cursor names, a key deleted again): the consumer sees nothing to emit
  /// while its readers saw the line.
  pure val WRITE_AHEAD = true
  /// A notification names a key, and the consumer re-reads it. `false`: it emits the state
  /// the notification carries.
  pure val RE_READ = true
  /// The consumer re-reads every key every so often, whatever it was told.
  pure val SAFETY_POLL = true
  /// A key is emitted only if it differs from what was last emitted. `false`: every read.
  pure val DEDUPE = true

  /// What reading a key finds: a version, or nothing. `Unsure` is what a restarted consumer
  /// believes of a key it may have been emitting when it died: equal to nothing, so the
  /// next read emits.
  type Obs = Gone | Live(int) | Unsure
  /// A line: the key's state now.
  type Ev = { key: int, obs: Obs }
  /// A notification: what a write made of a key. `g` is the store's counter at that write
  /// (the generation; nothing reads it but the simulation's ghost, which orders events).
  type Note = { key: int, obs: Obs, g: int }
  /// A key as stored: the counter at its last write (a remove counts) and whether it is there.
  type Slot = { ver: int, live: bool }

  /// A read the consumer has not finished: `emitted` and `saved` say how far it is. `note`
  /// is the notification it answers (none for the safety poll and the start-up diff).
  type Pending =
    | NoPend
    | Pend({ key: int, obs: Obs, g: int, note: Set[Note], marked: bool, emitted: bool, saved: bool })

  type State = {
    // the bucket
    store: int -> Slot,
    clock: int,
    // the subscription: what is held for the consumer, and everything ever sent to it
    net: Set[Note],
    sent: Set[Note],
    // the consumer's memory
    up: bool,
    seen: int -> Obs,
    startScan: Set[int],
    pend: Pending,
    // what survives a crash: the cursor, and the lines already written
    cursor: int -> Obs,
    dirty: Set[int],
    out: List[Ev],
  }

  pure val INITIAL: State = {
    store: KEYS.mapBy(k => { ver: 0, live: false }),
    clock: 0,
    net: Set(),
    sent: Set(),
    up: true,
    seen: KEYS.mapBy(k => Gone),
    startScan: Set(),
    pend: NoPend,
    cursor: KEYS.mapBy(k => Gone),
    dirty: Set(),
    out: [],
  }

  pure def obsOf(r: Slot): Obs = if (r.live) Live(r.ver) else Gone
  pure def storeObs(s: State, k: int): Obs = obsOf(s.store.get(k))

  pure def isPend(p: Pending): bool = match p { | NoPend => false | Pend(_) => true }

  // ---------- the world ----------

  pure def putCan(s: State, k: int): bool = s.clock < MAX_V
  pure def putNext(s: State, k: int): State = {
    val v = s.clock + 1
    val n: Note = { key: k, obs: Live(v), g: v }
    s.with("store", s.store.set(k, { ver: v, live: true })).with("clock", v)
      .with("net", if (PUSH) s.net.union(Set(n)) else s.net)
      .with("sent", if (PUSH) s.sent.union(Set(n)) else s.sent)
  }

  pure def removeCan(s: State, k: int): bool = s.clock < MAX_V and s.store.get(k).live
  pure def removeNext(s: State, k: int): State = {
    val v = s.clock + 1
    val n: Note = { key: k, obs: Gone, g: v }
    s.with("store", s.store.set(k, { ver: v, live: false })).with("clock", v)
      .with("net", if (PUSH) s.net.union(Set(n)) else s.net)
      .with("sent", if (PUSH) s.sent.union(Set(n)) else s.sent)
  }

  /// A notification is lost.
  pure def dropCan(s: State, n: Note): bool = LOSSY and s.net.contains(n)
  pure def dropNext(s: State, n: Note): State = s.with("net", s.net.exclude(Set(n)))

  /// A notification is delivered again (an acknowledgement is lost, or the service repeats).
  pure def redeliverCan(s: State, n: Note): bool = s.sent.contains(n) and not(s.net.contains(n))
  pure def redeliverNext(s: State, n: Note): State = s.with("net", s.net.union(Set(n)))

  // ---------- the consumer ----------

  pure def readFree(s: State): bool = s.up and not(isPend(s.pend))

  /// What a read of `k` makes of the consumer's state, answering `note` (if any).
  pure def readNext(s: State, k: int, note: Set[Note]): State = {
    val fresh = storeObs(s, k)
    val g = s.store.get(k).ver
    // trusting the notification: the state it carries, not the store's
    val carried = note.fold(Gone, (acc, n) => n.obs)
    val gc = note.fold(0, (acc, n) => n.g)
    val obs = if (RE_READ or note.size() == 0) fresh else carried
    val gg = if (RE_READ or note.size() == 0) g else gc
    if (not(DEDUPE) or obs != s.seen.get(k))
      s.with("pend", Pend({ key: k, obs: obs, g: gg, note: note, marked: false, emitted: false, saved: false }))
    else
      s.with("net", s.net.exclude(note)).with("startScan", s.startScan.exclude(Set(k)))
  }

  /// A notification names the key to re-read.
  pure def noteCan(s: State, n: Note): bool = readFree(s) and s.net.contains(n)
  pure def noteNext(s: State, n: Note): State = readNext(s, n.key, Set(n))

  /// The safety poll re-reads a key.
  pure def scanCan(s: State, k: int): bool = readFree(s) and SAFETY_POLL
  pure def scanNext(s: State, k: int): State = readNext(s, k, Set())

  /// The start-up diff re-reads a key it has not yet.
  pure def startCan(s: State, k: int): bool = readFree(s) and s.startScan.contains(k)
  pure def startNext(s: State, k: int): State = readNext(s, k, Set())

  pure def markCan(s: State): bool = s.up and WRITE_AHEAD and CURSOR_AFTER_EMIT and match s.pend {
    | Pend(p) => not(p.marked)
    | NoPend => false
  }
  pure def markNext(s: State): State = match s.pend {
    | Pend(p) => s.with("dirty", s.dirty.union(Set(p.key))).with("pend", Pend({ ...p, marked: true }))
    | NoPend => s
  }

  pure def emitCan(s: State): bool = s.up and match s.pend {
    | Pend(p) => not(p.emitted) and (if (CURSOR_AFTER_EMIT) (p.marked or not(WRITE_AHEAD)) else p.saved)
    | NoPend => false
  }
  pure def emitNext(s: State): State = match s.pend {
    | Pend(p) => s.with("out", s.out.append({ key: p.key, obs: p.obs }))
      .with("seen", s.seen.set(p.key, p.obs))
      .with("pend", Pend({ ...p, emitted: true }))
    | NoPend => s
  }

  pure def saveCan(s: State): bool = s.up and match s.pend {
    | Pend(p) => not(p.saved) and (not(CURSOR_AFTER_EMIT) or p.emitted)
    | NoPend => false
  }
  pure def saveNext(s: State): State = match s.pend {
    | Pend(p) => s.with("cursor", s.cursor.set(p.key, p.obs)).with("dirty", s.dirty.exclude(Set(p.key)))
      .with("pend", Pend({ ...p, saved: true }))
    | NoPend => s
  }

  pure def finishCan(s: State): bool = s.up and match s.pend {
    | Pend(p) => p.emitted and p.saved
    | NoPend => false
  }
  pure def finishNext(s: State): State = match s.pend {
    | Pend(p) => s.with("pend", NoPend).with("net", s.net.exclude(p.note))
      .with("startScan", s.startScan.exclude(Set(p.key)))
    | NoPend => s
  }

  pure def crashCan(s: State): bool = s.up
  pure def crashNext(s: State): State =
    s.with("up", false).with("pend", NoPend).with("startScan", Set()).with("seen", KEYS.mapBy(k => Gone))

  pure def restartCan(s: State): bool = not(s.up)
  pure def restartNext(s: State): State = {
    val now = KEYS.mapBy(k => storeObs(s, k))
    s.with("up", true)
      .with("seen", if (RESCAN_ON_START) KEYS.mapBy(k => if (s.dirty.contains(k)) Unsure else s.cursor.get(k)) else now)
      .with("startScan", if (RESCAN_ON_START) KEYS else Set())
  }

  // ---------- every step, as data ----------

  type Act =
    | APut(int)
    | ARemove(int)
    | ADrop(Note)
    | ARedeliver(Note)
    | ANote(Note)
    | AScan(int)
    | AStart(int)
    | AMark
    | AEmit
    | ASave
    | AFinish
    | ACrash
    | ARestart

  pure def acts(s: State): Set[Act] =
    KEYS.filter(k => putCan(s, k)).map(k => APut(k))
      .union(KEYS.filter(k => removeCan(s, k)).map(k => ARemove(k)))
      .union(s.net.filter(n => dropCan(s, n)).map(n => ADrop(n)))
      .union(s.sent.filter(n => redeliverCan(s, n)).map(n => ARedeliver(n)))
      .union(s.net.filter(n => noteCan(s, n)).map(n => ANote(n)))
      .union(KEYS.filter(k => scanCan(s, k)).map(k => AScan(k)))
      .union(KEYS.filter(k => startCan(s, k)).map(k => AStart(k)))
      .union(Set(AMark).filter(a => markCan(s)))
      .union(Set(AEmit).filter(a => emitCan(s)))
      .union(Set(ASave).filter(a => saveCan(s)))
      .union(Set(AFinish).filter(a => finishCan(s)))
      .union(Set(ACrash).filter(a => crashCan(s)))
      .union(Set(ARestart).filter(a => restartCan(s)))

  pure def apply(s: State, a: Act): State = match a {
    | APut(k) => putNext(s, k)
    | ARemove(k) => removeNext(s, k)
    | ADrop(n) => dropNext(s, n)
    | ARedeliver(n) => redeliverNext(s, n)
    | ANote(n) => noteNext(s, n)
    | AScan(k) => scanNext(s, k)
    | AStart(k) => startNext(s, k)
    | AMark => markNext(s)
    | AEmit => emitNext(s)
    | ASave => saveNext(s)
    | AFinish => finishNext(s)
    | ACrash => crashNext(s)
    | ARestart => restartNext(s)
  }

  // ---------- actions ----------

  var state: State

  action init = state' = INITIAL

  action write(k: int): bool = all { putCan(state, k), state' = putNext(state, k) }
  action remove(k: int): bool = all { removeCan(state, k), state' = removeNext(state, k) }
  action drop(n: Note): bool = all { dropCan(state, n), state' = dropNext(state, n) }
  action redeliver(n: Note): bool = all { redeliverCan(state, n), state' = redeliverNext(state, n) }
  action note(n: Note): bool = all { noteCan(state, n), state' = noteNext(state, n) }
  action scan(k: int): bool = all { scanCan(state, k), state' = scanNext(state, k) }
  action start(k: int): bool = all { startCan(state, k), state' = startNext(state, k) }
  action mark: bool = all { markCan(state), state' = markNext(state) }
  action emit: bool = all { emitCan(state), state' = emitNext(state) }
  action save: bool = all { saveCan(state), state' = saveNext(state) }
  action finish: bool = all { finishCan(state), state' = finishNext(state) }
  action crash: bool = all { crashCan(state), state' = crashNext(state) }
  action restart: bool = all { restartCan(state), state' = restartNext(state) }

  /// Any interleaving: one enabled step, uniformly.
  action step = {
    nondet a = acts(state).oneOf()
    state' = apply(state, a)
  }

  // ---------- properties ----------

  /// The last state written out for a key (nothing: Gone).
  pure def lastOut(s: State, k: int): Obs =
    s.out.foldl(Gone, (acc, e) => if (e.key == k) e.obs else acc)

  /// What the consumer will have emitted for `k` once it has done what it is bound to: every
  /// key it was told of, or must re-read at start, or re-reads in the safety poll (it is
  /// fair: one poll will come), is read once more, and emitted when it differs from what
  /// the consumer believes it emitted.
  pure def settled(s: State, k: int): Obs = {
    val pendKey = match s.pend { | Pend(p) => p.key == k | NoPend => false }
    val pendObs = match s.pend { | Pend(p) => p.obs | NoPend => Gone }
    val believed = if (pendKey) pendObs else s.seen.get(k)
    val reread = SAFETY_POLL or s.startScan.contains(k) or s.net.exists(n => n.key == k) or pendKey
    if (not(reread)) lastOut(s, k)
    else if (storeObs(s, k) != believed) storeObs(s, k)
    else if (pendKey) pendObs
    else lastOut(s, k)
  }

  /// noLoss. Every change is emitted, or superseded by a later emitted state of its key:
  /// once the consumer is up and nothing more is written, one scan later what it emitted
  /// last for each key is the store's state.
  val noLoss: bool = not(state.up) or KEYS.forall(k => settled(state, k) == storeObs(state, k))

  /// cursor <= emitted: the cursor never names a state that was not emitted (never skip).
  val cursorBehindOut: bool = KEYS.forall(k =>
    state.cursor.get(k) == Gone or state.out.indices().exists(i => state.out[i].key == k and state.out[i].obs == state.cursor.get(k)))

  /// Only a real change is emitted: what is pending differs from what was last emitted
  /// (by this run of the consumer).
  val noRedundant: bool = match state.pend {
    | Pend(p) => p.emitted or p.obs != state.seen.get(p.key)
    | NoPend => true
  }

  /// The intent is on record: if a key's last line is not what the cursor holds, the cursor
  /// says the key is being emitted. (A crash then leaves the consumer unsure of the key.)
  val intentRecorded: bool = KEYS.forall(k => lastOut(state, k) == state.cursor.get(k) or state.dirty.contains(k))

  val safety: bool = all { noLoss, cursorBehindOut, noRedundant, intentRecorded }
}
