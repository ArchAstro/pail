// -*- mode: Bluespec; -*-
/// A client of one store (`store.qnt`), one action per `Store` method, each recording the
/// method's reply. `tests/store_refinement.rs` checks every pail backend against it.
module storeSpec {
  import store.* from "./store"

  /// What a `Store` method returned.
  type Reply =
    | NoReply
    // `put`, `delete`.
    | Done
    // `put_new`: whether it created the object.
    | Created(bool)
    // `cas`: applied, giving the object this version.
    | Swapped(int)
    | Conflict
    // `get_opt`: the body and its version, or nothing.
    | Found({ body: str, version: int })
    | Missing
    // `list`: the keys of live objects.
    | Listed(Set[str])

  type State = { objects: Space[str, str], reply: Reply }

  var state: State

  action init = all {
    state' = { objects: Map(), reply: NoReply },
  }

  /// `get_opt(key)`.
  action getOpt(key: str): bool = all {
    state' = {
      ...state,
      reply:
        if (isLive(state.objects, key))
          Found({ body: body(state.objects, key), version: versionOf(state.objects, key) })
        else Missing,
    },
  }

  /// `put_new(key, data)`: create if absent.
  action putNew(key: str, data: str): bool = all {
    state' = {
      objects: created(state.objects, key, data),
      reply: Created(createApplies(state.objects, key)),
    },
  }

  /// `put(key, data)`: unconditional.
  action putAny(key: str, data: str): bool = all {
    state' = { objects: written(state.objects, key, data), reply: Done },
  }

  /// `cas(key, data, None)`: create only if absent.
  action casNew(key: str, data: str): bool = all {
    state' = {
      objects: created(state.objects, key, data),
      reply:
        if (createApplies(state.objects, key)) Swapped(versionOf(state.objects, key) + 1)
        else Conflict,
    },
  }

  /// `cas(key, data, Some(version))`.
  action cas(key: str, version: int, data: str): bool = all {
    state' = {
      objects: swapped(state.objects, key, version, data),
      reply:
        if (casApplies(state.objects, key, version)) Swapped(version + 1)
        else Conflict,
    },
  }

  /// `delete(key)`: done whether or not the object existed.
  action remove(key: str): bool = all {
    state' = { objects: deleted(state.objects, key), reply: Done },
  }

  /// `list(prefix)` over the scenario's prefix.
  action listKeys: bool = all {
    state' = { ...state, reply: Listed(liveKeys(state.objects)) },
  }
}
