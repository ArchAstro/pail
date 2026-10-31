// -*- mode: Bluespec; -*-
/// Interleavings the real `pail events` watcher must follow (`events.qnt`), step by step:
/// `tests/events_refinement.rs` runs each against a real store and cursor, once for refs with
/// a cursor file and once for objects of a sibling area with a cursor in the bucket. A key is
/// `refs/b<k>` or `<repo>.actions/k<k>`; a version is its generation. The harness stands in
/// for the subscription (it holds the notifications) and for a crash (it drops the watcher).
module eventsScenarios {
  import events.* from "./events"

  pure def wrote(k: int, v: int): Note = { key: k, obs: Live(v), g: v }
  pure def removed(k: int, v: int): Note = { key: k, obs: Gone, g: v }
  pure def line(k: int, v: int): Ev = { key: k, obs: Live(v) }
  pure def goneLine(k: int): Ev = { key: k, obs: Gone }

  /// A notification is a hint: the key is re-read, the line written, the cursor moved only
  /// after it. A duplicate of the notification, delivered again, finds nothing new.
  /// @conformance
  run hintRun = init
    .then(write(1))
    .then(note(wrote(1, 1)))
    .then(mark)
    .then(all {
      assert(all {
        state.cursor == Map(1 -> Gone, 2 -> Gone),
        state.dirty == Set(1),
        state.out == [],
      }),
      state' = state,
    })
    .then(emit)
    .then(all {
      assert(all {
        state.out == [line(1, 1)],
        // the line is out, the cursor is not yet there: a crash now repeats the line
        state.cursor == Map(1 -> Gone, 2 -> Gone),
        state.seen.get(1) == Live(1),
      }),
      state' = state,
    })
    .then(save)
    .then(finish)
    .then(all {
      assert(all {
        state.cursor == Map(1 -> Live(1), 2 -> Gone),
        state.dirty == Set(),
        state.net == Set(),
        state.pend == NoPend,
      }),
      state' = state,
    })
    .then(redeliver(wrote(1, 1)))
    .then(note(wrote(1, 1)))
    .then(all {
      assert(all {
        // nothing differs: acknowledged, nothing emitted
        state.pend == NoPend,
        state.net == Set(),
        state.out == [line(1, 1)],
      }),
      state' = state,
    })

  /// Notifications arrive out of order: the older one is answered with the key's present
  /// state, which was emitted already. Only the newest state is ever a line.
  /// @conformance
  run reorderRun = init
    .then(write(1))
    .then(write(1))
    .then(note(wrote(1, 2)))
    .then(mark)
    .then(emit)
    .then(save)
    .then(finish)
    .then(note(wrote(1, 1)))
    .then(all {
      assert(all {
        state.pend == NoPend,
        state.net == Set(),
        state.out == [line(1, 2)],
        state.cursor.get(1) == Live(2),
      }),
      state' = state,
    })

  /// A key that is removed is a line too, and the cursor keeps it as gone.
  /// @conformance
  run deleteRun = init
    .then(write(2))
    .then(note(wrote(2, 1)))
    .then(mark)
    .then(emit)
    .then(save)
    .then(finish)
    .then(remove(2))
    .then(note(removed(2, 2)))
    .then(mark)
    .then(emit)
    .then(save)
    .then(finish)
    .then(all {
      assert(all {
        state.out == [line(2, 1), goneLine(2)],
        state.cursor == Map(1 -> Gone, 2 -> Gone),
        state.seen.get(2) == Gone,
      }),
      state' = state,
    })

  /// A lost notification costs latency: the safety poll finds the key.
  /// @conformance
  run safetyPollRun = init
    .then(write(1))
    .then(drop(wrote(1, 1)))
    .then(all { assert(state.net == Set()), state' = state })
    .then(scan(1))
    .then(mark)
    .then(emit)
    .then(save)
    .then(finish)
    .then(all {
      assert(all {
        state.out == [line(1, 1)],
        state.cursor.get(1) == Live(1),
      }),
      state' = state,
    })

  /// The consumer died; keys changed, and one was removed, meanwhile. It resumes from its
  /// cursor: the start-up diff emits what changed (the deletion too).
  /// @conformance
  run resumeRun = init
    .then(write(1))
    .then(note(wrote(1, 1)))
    .then(mark)
    .then(emit)
    .then(save)
    .then(finish)
    .then(crash)
    .then(write(2))
    .then(remove(1))
    .then(restart)
    .then(all {
      assert(all {
        state.seen == Map(1 -> Live(1), 2 -> Gone),
        state.startScan == Set(1, 2),
        // the subscription kept what was said while it was down
        state.net == Set(wrote(2, 2), removed(1, 3)),
      }),
      state' = state,
    })
    .then(start(1))
    .then(mark)
    .then(emit)
    .then(save)
    .then(finish)
    .then(start(2))
    .then(mark)
    .then(emit)
    .then(save)
    .then(finish)
    .then(all {
      assert(all {
        state.out == [line(1, 1), goneLine(1), line(2, 2)],
        state.cursor == Map(1 -> Gone, 2 -> Live(2)),
        state.startScan == Set(),
      }),
      state' = state,
    })
    // the notifications that were held are answered by finding nothing new
    .then(note(wrote(2, 2)))
    .then(note(removed(1, 3)))
    .then(all {
      assert(all { state.net == Set(), state.out.length() == 3 }),
      state' = state,
    })

  /// A crash between the line and the cursor repeats the line (at-least-once), and the
  /// notification, unacknowledged, comes again and finds nothing new.
  /// @conformance
  run crashBeforeSaveRun = init
    .then(write(1))
    .then(note(wrote(1, 1)))
    .then(mark)
    .then(emit)
    .then(crash)
    .then(restart)
    .then(all {
      assert(all {
        state.out == [line(1, 1)],
        state.cursor.get(1) == Gone,
        // the cursor knows the key was being emitted
        state.dirty == Set(1),
        state.seen.get(1) == Unsure,
      }),
      state' = state,
    })
    .then(start(1))
    .then(mark)
    .then(emit)
    .then(save)
    .then(finish)
    .then(all {
      assert(all {
        state.out == [line(1, 1), line(1, 1)],
        state.cursor.get(1) == Live(1),
        state.dirty == Set(),
      }),
      state' = state,
    })
    .then(note(wrote(1, 1)))
    .then(all { assert(all { state.out.length() == 2, state.net == Set() }), state' = state })

  /// The reason for the mark. The line for a put is out, the consumer dies before the
  /// cursor moves, and the key is removed: the cursor holds "gone" and the store holds
  /// "gone", yet the readers saw a put. Only the mark makes the restart emit the deletion.
  /// @conformance
  run writeAheadRun = init
    .then(write(1))
    .then(note(wrote(1, 1)))
    .then(mark)
    .then(emit)
    .then(crash)
    .then(remove(1))
    .then(restart)
    .then(start(1))
    .then(mark)
    .then(emit)
    .then(save)
    .then(finish)
    .then(all {
      assert(all {
        state.out == [line(1, 1), goneLine(1)],
        state.cursor.get(1) == Gone,
        state.seen.get(1) == Gone,
      }),
      state' = state,
    })

  /// A crash after the intent but before the line: nothing was emitted, and the restart
  /// emits the state once.
  /// @conformance
  run crashBeforeEmitRun = init
    .then(write(1))
    .then(note(wrote(1, 1)))
    .then(mark)
    .then(crash)
    .then(restart)
    .then(start(1))
    .then(mark)
    .then(emit)
    .then(save)
    .then(finish)
    .then(all {
      assert(all {
        state.out == [line(1, 1)],
        state.cursor.get(1) == Live(1),
      }),
      state' = state,
    })
}
