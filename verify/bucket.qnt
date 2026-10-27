// -*- mode: Bluespec; -*-
/// Pail's layout in the store (`store.qnt`): what each space holds, reduced to what the
/// modelled protocols depend on.
///
/// ```text
///   pages/<hash>     a tree's page, named by its content          Space[Tree, bool]
///   commits/<hash>   immutable, created once, named by content    Space[CommitId, Commit]
///   refs/<branch>    the one mutable object, moved by CAS         Space[str, RefBody]
///   locks/<holder>   a lease: writers share, gc holds alone       Space[str, LeaseKind]
///   intents/<b>/<id> a combining writer's change, until it lands   Space[str, Intent]
/// ```
///
/// A tree here is small enough for one page (as a real one of a few files is). Not laid
/// out yet: packs and chunks (file contents inline), `reflog/`.
module bucket {
  import store.* from "./store"

  /// A snapshot: every path in `PATHS` maps to its content. Also the name of its page:
  /// pages are named by their content, so equal trees share one.
  type Tree = str -> str

  /// Paths a commit writes, each to its new content.
  type Delta = str -> str

  /// A commit, named as its hash names it: by the writers of every commit from the root
  /// to it. A writer's commit on a given parent always has the same content (the same
  /// delta, fields and dates), so the chain fixes the hash.
  type CommitId = List[str]

  /// A stored commit: its tree (the key of its root page) and every commit below it.
  type Commit = { tree: Tree, ancestors: Set[CommitId] }

  /// The intents a ref has landed, each to the commit that carries it. (pail's keeps the
  /// last `RECENT_MAX`; never reached here.)
  type Recent = str -> CommitId

  /// A ref object's body. (pail's also carries its lineage.)
  type RefBody = { commit: CommitId, recent: Recent }

  /// A ref as a client reads it: the commit, and the version a swap must match.
  type Ref = { commit: CommitId, version: int }

  /// A combining writer's change (`intents/<branch>/<id>.json`), keyed here by its writer
  /// (a writer makes one per commit). Ids sort by creation time, `at`; clocks differ
  /// between machines, so `at` is any time, ties broken by `RANK`.
  type Intent = { base: CommitId, delta: Delta, at: int }

  /// A commit a combining round builds for an intent: its tree and ancestors, known before
  /// it is stored.
  type Entry = { intent: str, commit: CommitId, tree: Tree, ancestors: Set[CommitId] }

  /// A lease's kind: writers hold shared leases together; gc holds an exclusive one alone.
  type LeaseKind = Shared | Exclusive

  /// Objects written since the gc grace period last elapsed (by their LastModified).
  /// Creating an object that exists already does not write it, so does not make it young.
  type Young = { pages: Set[Tree], commits: Set[CommitId] }

  type Bucket = {
    pages: Space[Tree, bool],
    commits: Space[CommitId, Commit],
    refs: Space[str, RefBody],
    locks: Space[str, LeaseKind],
    intents: Space[str, Intent],
    young: Young,
  }

  pure val BRANCH: str = "main"
  pure val PATHS: Set[str] = Set("a", "b")
  pure val ROOT: CommitId = ["root"]
  pure val ROOT_TREE: Tree = Map("a" -> "0", "b" -> "0")

  /// A branch created with one commit holding every path, long enough ago to be old.
  pure val ROOT_BUCKET: Bucket = {
    pages: created(Map(), ROOT_TREE, true),
    commits: created(Map(), ROOT, { tree: ROOT_TREE, ancestors: Set() }),
    refs: created(Map(), BRANCH, { commit: ROOT, recent: Map() }),
    locks: Map(),
    intents: Map(),
    young: { pages: Set(), commits: Set() },
  }

  /// The branch ref as a client reads it.
  pure def tip(b: Bucket): Ref = {
    commit: body(b.refs, BRANCH).commit,
    version: versionOf(b.refs, BRANCH),
  }

  /// The intents the ref has landed.
  pure def recentOf(b: Bucket): Recent = body(b.refs, BRANCH).recent

  pure def treeOf(b: Bucket, id: CommitId): Tree = body(b.commits, id).tree

  pure def ancestorsOf(b: Bucket, id: CommitId): Set[CommitId] = body(b.commits, id).ancestors

  /// Whether the branch's history contains `id`.
  pure def onBranch(b: Bucket, id: CommitId): bool =
    tip(b).commit == id or ancestorsOf(b, tip(b).commit).contains(id)

  /// Every commit in the branch's history, head included.
  pure def branch(b: Bucket): Set[CommitId] =
    ancestorsOf(b, tip(b).commit).union(Set(tip(b).commit))

  /// `t` with `d` written over it.
  pure def overlay(t: Tree, d: Delta): Tree =
    d.keys().fold(t, (acc, p) => acc.put(p, d.get(p)))

  /// Whether `d` writes a path that differs between `from` and `onto`.
  pure def overlaps(d: Delta, from: Tree, onto: Tree): bool =
    d.keys().exists(p => from.get(p) != onto.get(p))

  /// Commit `id`, `d` on top of `parent`, uploaded with its page: two creates, each a
  /// no-op if the object is there already (`Store::put_new`), and so not making it young.
  pure def uploaded(b: Bucket, id: CommitId, parent: CommitId, d: Delta): Bucket =
    stored(b, id, overlay(treeOf(b, parent), d), ancestorsOf(b, parent).union(Set(parent)))

  /// Commit `id` with its page, created as `uploaded` does.
  pure def stored(b: Bucket, id: CommitId, tree: Tree, ancestors: Set[CommitId]): Bucket = {
    ...b,
    pages: created(b.pages, tree, true),
    commits: created(b.commits, id, { tree: tree, ancestors: ancestors }),
    young: {
      pages:
        if (createApplies(b.pages, tree)) b.young.pages.union(Set(tree))
        else b.young.pages,
      commits:
        if (createApplies(b.commits, id)) b.young.commits.union(Set(id))
        else b.young.commits,
    },
  }

  /// Every commit of a combining round, stored in one wave.
  pure def storedBatch(b: Bucket, batch: Set[Entry]): Bucket =
    batch.fold(b, (acc, e) => stored(acc, e.commit, e.tree, e.ancestors))

  /// Whether a swap of the ref expecting `version` applies.
  pure def refSwapApplies(b: Bucket, version: int): bool = casApplies(b.refs, BRANCH, version)

  /// The ref swapped to `target` with `recent`, if it is still at `version`.
  pure def refSwapped(b: Bucket, version: int, target: CommitId, recent: Recent): Bucket = {
    ...b,
    refs: swapped(b.refs, BRANCH, version, { commit: target, recent: recent }),
  }

  pure def intentPut(b: Bucket, w: str, i: Intent): Bucket =
    { ...b, intents: written(b.intents, w, i) }

  pure def intentsDeleted(b: Bucket, ws: Set[str]): Bucket =
    { ...b, intents: ws.fold(b.intents, (acc, w) => deleted(acc, w)) }

  /// Whether a lease other than `holder`'s is live (of `kinds`).
  pure def leasedByOthers(b: Bucket, holder: str, kinds: Set[LeaseKind]): bool =
    liveKeys(b.locks).exists(k => k != holder and kinds.contains(body(b.locks, k)))

  pure def leased(b: Bucket, holder: str, kind: LeaseKind): Bucket =
    { ...b, locks: written(b.locks, holder, kind) }

  pure def unleased(b: Bucket, holder: str): Bucket = { ...b, locks: deleted(b.locks, holder) }

  /// Whether every commit the branch reaches, and its page, is stored.
  pure def branchIsStored(b: Bucket): bool =
    branch(b).forall(id => isLive(b.commits, id) and isLive(b.pages, treeOf(b, id)))
}
