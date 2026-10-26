// -*- mode: Bluespec; -*-
/// Pail's layout in the store (`store.qnt`): what each space holds, reduced to what
/// publishing depends on.
///
/// ```text
///   commits/<hash>   immutable, created once, named by content    Space[CommitId, Commit]
///   refs/<branch>    the one mutable object, moved by CAS         Space[str, RefBody]
/// ```
///
/// Not laid out yet: pages and packs (a commit's tree is inlined), `intents/`, `reflog/`.
module bucket {
  import store.* from "./store"

  /// A snapshot: every path in `PATHS` maps to its content.
  type Tree = str -> str

  /// Paths a commit writes, each to its new content.
  type Delta = str -> str

  /// A commit, named as its hash names it. On a branch whose history only moves forward,
  /// a writer and a generation fix the parent, so `(by, gen)` is as unique as the hash.
  type CommitId = { by: str, gen: int }

  /// A stored commit: its snapshot and every commit below it (linear history).
  type Commit = { tree: Tree, ancestors: Set[CommitId] }

  /// A ref object's body. (pail's also carries `recent` and its lineage.)
  type RefBody = { commit: CommitId }

  /// A ref as a client reads it: the commit, and the version a swap must match.
  type Ref = { commit: CommitId, version: int }

  type Bucket = {
    commits: Space[CommitId, Commit],
    refs: Space[str, RefBody],
  }

  pure val BRANCH: str = "main"
  pure val PATHS: Set[str] = Set("a", "b")
  pure val ROOT: CommitId = { by: "root", gen: 0 }
  pure val ROOT_TREE: Tree = Map("a" -> "0", "b" -> "0")

  /// A branch created with one commit holding every path.
  pure val ROOT_BUCKET: Bucket = {
    commits: created(Map(), ROOT, { tree: ROOT_TREE, ancestors: Set() }),
    refs: created(Map(), BRANCH, { commit: ROOT }),
  }

  /// The branch ref as a client reads it.
  pure def tip(b: Bucket): Ref = {
    commit: body(b.refs, BRANCH).commit,
    version: versionOf(b.refs, BRANCH),
  }

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

  /// Commit `id`, `d` on top of `parent`, uploaded: a create, which changes nothing if the
  /// object is there already (`Store::put_new`).
  pure def uploaded(b: Bucket, id: CommitId, parent: CommitId, d: Delta): Bucket = {
    ...b,
    commits: created(b.commits, id, {
      tree: overlay(treeOf(b, parent), d),
      ancestors: ancestorsOf(b, parent).union(Set(parent)),
    }),
  }

  /// Whether a swap of the ref expecting `version` applies.
  pure def refSwapApplies(b: Bucket, version: int): bool = casApplies(b.refs, BRANCH, version)

  /// The ref swapped to `target`, if it is still at `version`.
  pure def refSwapped(b: Bucket, version: int, target: CommitId): Bucket = {
    ...b,
    refs: swapped(b.refs, BRANCH, version, { commit: target }),
  }
}
