// -*- mode: Bluespec; -*-
/// Pail's layout in the store (`store.qnt`): what each space holds, reduced to what the
/// modelled protocols depend on.
///
/// ```text
///   pages/<hash>     a tree's page, named by its content          Space[Page, bool]
///   commits/<hash>   immutable, created once, named by content    Space[CommitId, Commit]
///   refs/<branch>    the one mutable object, moved by CAS         Space[str, RefBody]
///   locks/<holder>   a lease: writers share, gc holds alone       Space[str, Lease]
///   intents/<b>/<id> a combining writer's change, until it lands   Space[str, Intent]
///   reflog/<b>/<id>  a rewrite's record, created once, kept         Space[int, Rewrite]
///   packs/<hash>     file contents, created once, named by content   Space[PackId, Set[str]]
///   chunks/<hash>    a piece of a huge file, created once, named by content   Space[str, str]
/// ```
///
/// A tree here is small enough for one page (as a real one of a few files is). File
/// contents are inline, except the `BIG` ones, which live in a pack: a writer prepares one
/// pack for the big files it writes (or relocates), stores it with its first commit and
/// reuses it in any retry, and the commit's page names it per path (`Commit.locs`, the
/// page's `Loc::Pack`). The `HUGE` ones are split into chunks, each its own object named by
/// its content, so equal pieces of two files (or two versions of one) are one object; the
/// page lists a file's chunks (`Loc::Chunks`), which the file's content fixes (`CHUNKS`), so
/// a page's identity is not changed by them.
module bucket {
  import store.* from "./store"

  /// A snapshot: every path in `PATHS` maps to its content. Also the name of its page:
  /// pages are named by their content, so equal trees share one.
  type Tree = str -> str

  /// A page: the tree and where it puts each big file (a page hashes its locations, so
  /// the same tree in other packs is another page).
  type Page = { tree: Tree, locs: str -> PackId }

  /// Paths a commit writes, each to its new content.
  type Delta = str -> str

  /// A commit, named as its hash names it: by the writers of every commit from the root
  /// to it. A writer's commit on a given parent always has the same content (the same
  /// delta, fields and dates), so the chain fixes the hash.
  type CommitId = List[str]

  /// A pack, named here by its writer: the commit's last name if a client's own commit
  /// (`packOf`), else the commit's.
  type PackId = CommitId

  /// A stored commit: its tree (the key of its root page), every commit below it, and
  /// where its page puts each big file: the pack that holds it.
  type Commit = { tree: Tree, ancestors: Set[CommitId], locs: str -> PackId }

  /// A commit about to be stored: its name, tree and ancestors.
  type Made = { id: CommitId, tree: Tree, ancestors: Set[CommitId] }

  /// The intents a ref has landed, each to the commit that carries it. (pail's keeps the
  /// last `RECENT_MAX`; never reached here.)
  type Recent = str -> CommitId

  /// Which rewrite a ref's history descends from: how many rewrites it has been through,
  /// and the record of the last (0: none). Moving the ref forward keeps it.
  type Lineage = { epoch: int, rewrite: int }

  /// A rewrite's record (`reflog/<branch>/<id>`, ids here numbered in creation order):
  /// what rewrote it (`rebase`, `undo` or `reset`), the tip it replaced and the one it
  /// moved to, the epoch the branch entered, the previous record (0: none), who rewrote
  /// it, each rewritten commit's successor, and the commits it dropped (pail's `map`, its
  /// `None` entries apart).
  type Rewrite = {
    op: str,
    old: CommitId,
    new: CommitId,
    epoch: int,
    prev: int,
    by: str,
    successors: CommitId -> CommitId,
    dropped: Set[CommitId],
  }

  /// A ref object's body.
  type RefBody = { commit: CommitId, recent: Recent, lineage: Lineage }

  /// A ref as a client reads it: the commit, and the version a swap must match.
  type Ref = { commit: CommitId, version: int }

  /// A combining writer's change (`intents/<branch>/<id>.json`), keyed here by its writer
  /// (a writer makes one per commit). Ids sort by creation time, `at`; clocks differ
  /// between machines, so `at` is any time, ties broken by `RANK`.
  type Intent = { base: CommitId, delta: Delta, reloc: Delta, at: int }

  /// A commit a combining round builds for an intent: its tree and ancestors, known before
  /// it is stored.
  type Entry = {
    intent: str,
    commit: CommitId,
    tree: Tree,
    ancestors: Set[CommitId],
    // The big files whose page entry the commit points at its writer's pack (the intent's
    // delta, and the soft relocations that still applied).
    wrote: Set[str],
  }

  /// A lease's kind: writers hold shared leases together; gc holds an exclusive one alone.
  type LeaseKind = Shared | Exclusive

  /// A lease object (`locks/<holder>`): its kind, and the time it was last written, which
  /// is its LastModified in the store. Others ignore it once it is older than `TTL`.
  type Lease = { kind: LeaseKind, at: int }

  /// Objects written since the gc grace period last elapsed (by their LastModified).
  /// Creating an object that exists already does not write it, so does not make it young.
  type Young = { pages: Set[Page], commits: Set[CommitId], packs: Set[PackId], chunks: Set[str] }

  type Bucket = {
    pages: Space[Page, bool],
    commits: Space[CommitId, Commit],
    refs: Space[str, RefBody],
    locks: Space[str, Lease],
    intents: Space[str, Intent],
    reflog: Space[int, Rewrite],
    packs: Space[PackId, Set[str]],
    // A chunk's name (its hash) to its bytes, named by what they hash to: a chunk stored
    // without its bytes has none.
    chunks: Space[str, str],
    young: Young,
  }

  // ---------- time ----------
  //
  // Time is a count of ticks, a sixth of `LEASE_TTL` each (100 s). The constants mirror
  // lease.rs; `LATENCY` and `SKEW` are the assumptions the lease margin rests on
  // (`world.qnt`'s `tickCan`, README "Lease timing").

  /// `LEASE_TTL`: a lease not rewritten for longer is ignored.
  pure val TTL: int = 6
  /// `LEASE_TTL / 3`: how often the refresher rewrites a lease.
  pure val REFRESH: int = 2
  /// `LEASE_TTL / 2`: `Lease::check` refuses a ref swap once the lease is older than this.
  pure val MAX_AGE: int = 3
  /// The most ticks a request can take to land after the check that allowed it: a
  /// process may stall between `Lease::check` and the store applying the swap, but not
  /// for long (assumption D).
  pure val LATENCY: int = 1
  /// How far the lister's clock may run ahead of the store's, which stamps LastModified
  /// (assumption S).
  pure val SKEW: int = 1

  pure val BRANCH: str = "main"
  pure val PATHS: Set[str] = Set("a", "b")
  pure val ROOT: CommitId = ["root"]
  pure val ROOT_TREE: Tree = Map("a" -> "0", "b" -> "0")
  pure val ROOT_PAGE: Page = { tree: ROOT_TREE, locs: Map() }
  pure val NO_LINEAGE: Lineage = { epoch: 0, rewrite: 0 }

  /// Contents too big to inline (`INLINE_MAX`): stored in packs.
  pure val BIG: Set[str] = Set("p1", "p2", "p3")
  pure def isBig(content: str): bool = BIG.contains(content)

  /// Contents of `CHUNK_THRESHOLD` and more: stored as chunks, not in a pack.
  pure val HUGE: Set[str] = Set("h1", "h2")
  pure def isHuge(content: str): bool = HUGE.contains(content)

  /// The chunks each huge content splits into (`chunk_spans`: content-defined, so a
  /// content's chunks are fixed by it). `h1` and `h2` start the same, and share `k1`.
  pure val CHUNKS: str -> Set[str] = Map("h1" -> Set("k1", "k2"), "h2" -> Set("k1", "k3"))

  /// A branch created with one commit holding every path, long enough ago to be old.
  pure val ROOT_BUCKET: Bucket = {
    pages: created(Map(), ROOT_PAGE, true),
    commits: created(Map(), ROOT, { tree: ROOT_TREE, ancestors: Set(), locs: Map() }),
    refs: created(Map(), BRANCH, { commit: ROOT, recent: Map(), lineage: NO_LINEAGE }),
    locks: Map(),
    intents: Map(),
    reflog: Map(),
    packs: Map(),
    chunks: Map(),
    young: { pages: Set(), commits: Set(), packs: Set(), chunks: Set() },
  }

  /// The branch ref as a client reads it.
  pure def tip(b: Bucket): Ref = {
    commit: body(b.refs, BRANCH).commit,
    version: versionOf(b.refs, BRANCH),
  }

  /// The intents the ref has landed.
  pure def recentOf(b: Bucket): Recent = body(b.refs, BRANCH).recent

  pure def lineageOf(b: Bucket): Lineage = body(b.refs, BRANCH).lineage

  pure def recordOf(b: Bucket, id: int): Rewrite = body(b.reflog, id)

  pure def treeOf(b: Bucket, id: CommitId): Tree = body(b.commits, id).tree

  /// The page of commit `id`'s root.
  pure def pageOf(b: Bucket, id: CommitId): Page = { tree: treeOf(b, id), locs: body(b.commits, id).locs }

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

  /// The paths commit `id` changes from its parent `p`, each to its new content (pail's
  /// change list: no-op edits are not in it).
  pure def deltaOf(b: Bucket, id: CommitId, p: CommitId): Delta =
    PATHS.filter(q => treeOf(b, id).get(q) != treeOf(b, p).get(q)).mapBy(q => treeOf(b, id).get(q))

  /// Whether `d`, made on `from`, does not apply on `onto` (`Repo::check_applies`): a path
  /// it writes is neither as `from` had it nor as `d` leaves it.
  pure def conflicts(d: Delta, from: Tree, onto: Tree): bool =
    d.keys().exists(p => from.get(p) != onto.get(p) and onto.get(p) != d.get(p))

  /// Whether `d` writes a path that differs between `from` and `onto`.
  pure def overlaps(d: Delta, from: Tree, onto: Tree): bool =
    d.keys().exists(p => from.get(p) != onto.get(p))

  /// Commit `id`, `d` on top of `parent`, uploaded with its page and the pack of the big
  /// files it writes: creates, each a no-op if the object is there already
  /// (`Store::put_new`), and so not making it young. Every path of `d` is written, even
  /// to the content it has: that is a relocation (`pail maintenance repack`), which moves
  /// the file into this commit's pack and changes no content. The paths of `reuse` are
  /// written as an object already stored (the writer's base has the content: the loc is
  /// that one, and no pack or chunk is stored for it).
  pure def uploaded(b: Bucket, id: CommitId, parent: CommitId, d: Delta, reuse: str -> PackId): Bucket =
    storedWith(b, id, overlay(treeOf(b, parent), d), ancestorsOf(b, parent).union(Set(parent)), d.keys(), reuse)

  /// What a writer with base `base` reuses for `d` (`Worktree::commit`'s `reuse`: content
  /// the base's tree has at another path, found by its hash): for each big or huge path
  /// `d` changes to content another path of `base` holds, the pack that path's page names
  /// (a huge file's chunks are named by its content: its entry is the other's). A base
  /// the branch's history (or a record) does not reach is not one a working copy has read,
  /// so nothing is reused from it. A relocation (a path written to the content it has) is
  /// not a reuse: the repack writes with nothing to reuse.
  pure def reuseOf(b: Bucket, base: CommitId, d: Delta): str -> PackId =
    Set(d.keys().filter(p => and {
      isBig(d.get(p)) or isHuge(d.get(p)),
      PATHS.exists(q => q != p and treeOf(b, base).get(q) == d.get(p)),
      d.get(p) != treeOf(b, base).get(p),
    })).fold(Map(), (none, reusing) =>
      if (reusing == Set() or not(refsReach(b).union(reflogReaches(b)).contains(base))) Map()
      else reusing.mapBy(p =>
        if (isHuge(d.get(p))) ROOT
        else body(b.commits, base).locs.get(PATHS.filter(q => q != p and treeOf(b, base).get(q) == d.get(p)).fold("", (a, q) => q))
      )
    )

  /// The pack commit `id` stores for the big files it writes. A client's own commit uses
  /// the pack the client prepared once, whichever head its retries land on; a commit
  /// made of one (a combined intent, a rewrite's copy, a landed stack) has its own.
  pure def packOf(id: CommitId): PackId =
    if (Set("alice", "bob", "carol", "dave").contains(id[length(id) - 1])) [id[length(id) - 1]]
    // A combined intent's commit points at the pack its writer uploaded with its first attempt.
    else if (Set("alice+", "bob+", "carol+", "dave+").contains(id[length(id) - 1]))
      [Map("alice+" -> "alice", "bob+" -> "bob", "carol+" -> "carol", "dave+" -> "dave").get(id[length(id) - 1])]
    else id

  /// The newest of `ancestors`: the parent (ids lengthen along a history).
  pure def newest(ancestors: Set[CommitId]): CommitId =
    ancestors.fold([], (best, x) => if (length(x) > length(best)) x else best)

  /// Commit `id` with its page, created as `uploaded` does, writing the paths that differ
  /// from its parent's. The parent is stored already.
  pure def stored(b: Bucket, id: CommitId, tree: Tree, ancestors: Set[CommitId]): Bucket =
    storedWith(
      b, id, tree, ancestors,
      if (ancestors == Set()) PATHS
      else PATHS.filter(p => tree.get(p) != treeOf(b, newest(ancestors)).get(p)),
      Map(),
    )

  /// Commit `id` with its page, whose big files at `wrote` go in its pack (stored with
  /// it, in the real upload's one wave), the others where its parent's page has them, and
  /// the huge ones at `wrote` as chunks (each stored with it unless it exists: content
  /// addressed, so shared by every file and commit that has it). Paths in `reuse` write
  /// nothing: their loc is the one given.
  pure def storedWith(
    b: Bucket, id: CommitId, tree: Tree, ancestors: Set[CommitId], wrote: Set[str], reuse: str -> PackId
  ): Bucket = storedSoft(b, id, tree, ancestors, wrote, reuse, Set())

  /// `storedWith`, the pack also holding `extra`: the contents of the soft relocations a
  /// writer planned, kept or dropped on replay (its pack is uploaded once, whole).
  pure def storedSoft(
    b: Bucket, id: CommitId, tree: Tree, ancestors: Set[CommitId], wrote: Set[str], reuse: str -> PackId,
    extra: Set[str]
  ): Bucket =
    Set(if (ancestors == Set()) Map() else body(b.commits, newest(ancestors)).locs).fold(b, (n0, inherited) =>
      Set(PATHS.filter(p => isBig(tree.get(p)))).fold(b, (n1, bigs) =>
        Set(wrote.filter(p => not(reuse.keys().contains(p)))).fold(b, (nw, stores) =>
          Set(bigs.mapBy(p => if (reuse.keys().contains(p)) reuse.get(p) else if (wrote.contains(p)) packOf(id) else inherited.get(p))).fold(b, (n2, locs) =>
            Set(bigs.filter(p => stores.contains(p)).map(p => tree.get(p)).union(extra)).fold(b, (n3, contents) =>
              Set(stores.filter(p => isHuge(tree.get(p))).fold(Set(), (acc, p) => acc.union(CHUNKS.get(tree.get(p))))).fold(b, (nc, pieces) =>
                Set({ tree: tree, locs: locs }).fold(b, (n4, page) => {
                  ...b,
                  pages: created(b.pages, page, true),
                  commits: created(b.commits, id, { tree: tree, ancestors: ancestors, locs: locs }),
                  packs: if (contents == Set()) b.packs else created(b.packs, packOf(id), contents),
                  chunks: pieces.fold(b.chunks, (acc, k) => created(acc, k, k)),
                  young: {
                    pages: if (createApplies(b.pages, page)) b.young.pages.union(Set(page)) else b.young.pages,
                    commits:
                      if (createApplies(b.commits, id)) b.young.commits.union(Set(id))
                      else b.young.commits,
                    packs:
                      if (contents != Set() and createApplies(b.packs, packOf(id)))
                        b.young.packs.union(Set(packOf(id)))
                      else b.young.packs,
                    chunks: b.young.chunks.union(pieces.filter(k => createApplies(b.chunks, k))),
                  },
                })
              )
            )
          )
        )
      )
    )

  /// Where commit `id`'s page puts the big file at `p` (nowhere: the root).
  pure def locOf(b: Bucket, id: CommitId, p: str): PackId =
    if (body(b.commits, id).locs.keys().contains(p)) body(b.commits, id).locs.get(p) else ROOT

  /// The soft relocations of `soft` (planned against a `base` that had them in `baseTree`
  /// and `baseLocs`) that still apply on a `parent` of `parentTree` and `parentLocs`: not a
  /// path the delta `d` writes, and a path the parent still has as the base did (the same
  /// content, in the same pack). A changed or already relocated path is dropped, silently.
  pure def keptSoft(
    baseTree: Tree, baseLocs: str -> PackId, parentTree: Tree, parentLocs: str -> PackId, d: Delta, soft: Delta
  ): Set[str] =
    soft.keys().filter(p => and {
      not(d.keys().contains(p)),
      parentTree.get(p) == baseTree.get(p),
      parentTree.get(p) == soft.get(p),
      baseLocs.keys().contains(p) == parentLocs.keys().contains(p),
      or { not(baseLocs.keys().contains(p)), baseLocs.get(p) == parentLocs.get(p) },
    })

  /// The delta `d` with the relocations `kept` written over it: what the commit's page has.
  pure def withSoft(d: Delta, soft: Delta, kept: Set[str]): Delta =
    d.keys().union(kept).mapBy(p => if (d.keys().contains(p)) d.get(p) else soft.get(p))

  /// The contents of the big files `soft` plans to move: what a writer's pack holds besides
  /// its delta's, whether or not the relocation survives a replay.
  pure def softContents(soft: Delta): Set[str] =
    soft.keys().filter(p => isBig(soft.get(p))).map(p => soft.get(p))

  /// `uploaded` for a commit planned on `base` with soft relocations `soft`.
  pure def uploadedSoft(
    b: Bucket, id: CommitId, base: CommitId, parent: CommitId, d: Delta, soft: Delta, reuse: str -> PackId
  ): Bucket =
    Set(keptSoft(treeOf(b, base), body(b.commits, base).locs, treeOf(b, parent), body(b.commits, parent).locs, d, soft)).fold(b, (n, kept) =>
      storedSoft(
        b, id,
        overlay(treeOf(b, parent), withSoft(d, soft, kept)),
        ancestorsOf(b, parent).union(Set(parent)),
        d.keys().union(kept),
        reuse,
        softContents(soft),
      )
    )

  /// Every commit of a combining round, stored in one wave.
  pure def storedBatch(b: Bucket, batch: Set[Entry]): Bucket =
    Set(batch.fold(1000, (m, e) => if (e.ancestors.size() < m) e.ancestors.size() else m)).fold(b, (nl, lo) =>
      Set(storedBatchLevel(b, batch, lo)).fold(b, (n0, b0) =>
        Set(storedBatchLevel(b0, batch, lo + 1)).fold(b0, (n1, b1) =>
          Set(storedBatchLevel(b1, batch, lo + 2)).fold(b1, (n2, b2) => storedBatchLevel(b2, batch, lo + 3))
        )
      )
    )

  /// The batch's commits with `n` ancestors: each in its writer's pack, uploaded already
  /// with the writer's first attempt (a reused file is not modelled through combining).
  pure def storedBatchLevel(b: Bucket, batch: Set[Entry], n: int): Bucket =
    batch.filter(e => e.ancestors.size() == n).fold(b, (acc, e) =>
      storedWith(acc, e.commit, e.tree, e.ancestors, e.wrote, Map())
    )

  /// Commits stored together (at most four: a writer each), each after its parent (a
  /// page's locations come from the parent's), a level of ancestors at a time.
  pure def storedAll(b: Bucket, items: Set[Made]): Bucket =
    Set(items).fold(b, (n, xs) =>
      Set(xs.fold(1000, (m, x) => if (x.ancestors.size() < m) x.ancestors.size() else m)).fold(b, (nl, lo) =>
        Set(storedLevel(b, xs, lo)).fold(b, (n0, b0) =>
          Set(storedLevel(b0, xs, lo + 1)).fold(b0, (n1, b1) =>
            Set(storedLevel(b1, xs, lo + 2)).fold(b1, (n2, b2) => storedLevel(b2, xs, lo + 3))
          )
        )
      )
    )

  /// The commits of `items` with `n` ancestors, stored.
  pure def storedLevel(b: Bucket, items: Set[Made], n: int): Bucket =
    items.filter(m => m.ancestors.size() == n).fold(b, (acc, m) => stored(acc, m.id, m.tree, m.ancestors))

  /// Whether a swap of the ref expecting `version` applies.
  pure def refSwapApplies(b: Bucket, version: int): bool = casApplies(b.refs, BRANCH, version)

  /// The ref swapped to `target` with `recent`, if it is still at `version`: a move
  /// forward, keeping the lineage (which, at that version, is the one the writer read).
  pure def refSwapped(b: Bucket, version: int, target: CommitId, recent: Recent): Bucket = {
    ...b,
    refs: swapped(b.refs, BRANCH, version, { commit: target, recent: recent, lineage: lineageOf(b) }),
  }

  /// The ref swapped to `body`, if it is still at `version`: a rewrite.
  pure def refRewritten(b: Bucket, version: int, body: RefBody): Bucket =
    { ...b, refs: swapped(b.refs, BRANCH, version, body) }

  /// Record `r` under the next id.
  pure def recorded(b: Bucket, r: Rewrite): Bucket =
    { ...b, reflog: created(b.reflog, nextRecord(b), r) }

  pure def nextRecord(b: Bucket): int = b.reflog.keys().size() + 1

  /// Every commit a record names, and every commit below them: what walking a rewrite's
  /// map (`Repo::successor`) or undoing it reads.
  pure def recordReaches(b: Bucket, r: Rewrite): Set[CommitId] =
    ancestorsOf(b, r.old).union(ancestorsOf(b, r.new)).union(Set(r.old, r.new))

  /// The records from the ref's back, by `prev` (each client rewrites once: four at most).
  pure def chain(b: Bucket): Set[int] = {
    val r0 = lineageOf(b).rewrite
    val r1 = prevOf(b, r0)
    val r2 = prevOf(b, r1)
    Set(r0, r1, r2, prevOf(b, r2)).exclude(Set(0))
  }

  /// The record before record `id` (0: none).
  pure def prevOf(b: Bucket, id: int): int = if (id == 0) 0 else recordOf(b, id).prev

  /// Whether commit `id`, made on the branch at lineage `from`, was published: the branch
  /// has it, or the first rewrite since replaced a tip that had it.
  pure def landedSince(b: Bucket, id: CommitId, from: Lineage): bool = or {
    onBranch(b, id),
    chain(b).exists(r => and {
      recordOf(b, r).epoch == from.epoch + 1,
      recordOf(b, r).old == id or ancestorsOf(b, recordOf(b, r).old).contains(id),
    }),
  }

  /// Every commit some record reaches: gc's roots besides the refs.
  pure def reflogReaches(b: Bucket): Set[CommitId] =
    b.reflog.keys().fold(Set(), (acc, id) => acc.union(recordReaches(b, recordOf(b, id))))

  pure def intentPut(b: Bucket, w: str, i: Intent): Bucket =
    { ...b, intents: written(b.intents, w, i) }

  pure def intentsDeleted(b: Bucket, ws: Set[str]): Bucket =
    { ...b, intents: ws.fold(b.intents, (acc, w) => deleted(acc, w)) }

  /// Whether a lease other than `holder`'s is live (of `kinds`) at time `now` (`others`
  /// in lease.rs): not deleted, and not older than `TTL` by the lister's clock, which is
  /// `SKEW` ahead of the store's.
  pure def leasedByOthers(b: Bucket, holder: str, kinds: Set[LeaseKind], now: int): bool =
    liveKeys(b.locks).exists(k => and {
      k != holder,
      kinds.contains(body(b.locks, k).kind),
      now + SKEW - body(b.locks, k).at <= TTL,
    })

  /// `holder`'s lease written (or rewritten) at `now`.
  pure def leased(b: Bucket, holder: str, kind: LeaseKind, now: int): Bucket =
    { ...b, locks: written(b.locks, holder, { kind: kind, at: now }) }

  pure def unleased(b: Bucket, holder: str): Bucket = { ...b, locks: deleted(b.locks, holder) }

  /// When `holder`'s lease was last written: its LastModified.
  pure def leasedAt(b: Bucket, holder: str): int = body(b.locks, holder).at

  /// Whether each of `ids`, and its page, is stored.
  pure def allStored(b: Bucket, ids: Set[CommitId]): bool =
    ids.forall(id => isLive(b.commits, id) and isLive(b.pages, pageOf(b, id)))

  /// Every commit in the history at `id`, `id` included.
  pure def historyAt(b: Bucket, id: CommitId): Set[CommitId] = ancestorsOf(b, id).union(Set(id))

  /// Every commit some ref reaches: the branch and each stack (`land.qnt`).
  pure def refsReach(b: Bucket): Set[CommitId] =
    liveKeys(b.refs).fold(Set(), (acc, r) => acc.union(historyAt(b, body(b.refs, r).commit)))

  /// Whether every commit a ref reaches, and its page, is stored.
  pure def branchIsStored(b: Bucket): bool = allStored(b, refsReach(b))

  /// The packs commit `id`'s page names.
  pure def packsOf(b: Bucket, id: CommitId): Set[PackId] =
    body(b.commits, id).locs.keys().map(p => body(b.commits, id).locs.get(p))

  /// Every pack some commit in `ids` names.
  pure def packsNamedBy(b: Bucket, ids: Set[CommitId]): Set[PackId] =
    ids.fold(Set(), (acc, id) => acc.union(packsOf(b, id)))

  /// Whether each pack the pages of `ids` name is stored.
  pure def packsStored(b: Bucket, ids: Set[CommitId]): bool =
    packsNamedBy(b, ids).forall(k => isLive(b.packs, k))

  /// Whether each big file of `ids` reads back its content through its page's pack.
  pure def packsHold(b: Bucket, ids: Set[CommitId]): bool =
    ids.forall(id => body(b.commits, id).locs.keys().forall(p =>
      isLive(b.packs, body(b.commits, id).locs.get(p))
        and body(b.packs, body(b.commits, id).locs.get(p)).contains(treeOf(b, id).get(p))
    ))

  /// The chunks commit `id`'s page names: those of each huge file of its tree.
  pure def chunksOf(b: Bucket, id: CommitId): Set[str] =
    PATHS.filter(p => isHuge(treeOf(b, id).get(p))).fold(Set(), (acc, p) => acc.union(CHUNKS.get(treeOf(b, id).get(p))))

  /// Every chunk some commit in `ids` names.
  pure def chunksNamedBy(b: Bucket, ids: Set[CommitId]): Set[str] =
    ids.fold(Set(), (acc, id) => acc.union(chunksOf(b, id)))

  /// Whether each chunk the pages of `ids` name is stored.
  pure def chunksStored(b: Bucket, ids: Set[CommitId]): bool =
    chunksNamedBy(b, ids).forall(k => isLive(b.chunks, k))

  /// Whether each huge file of `ids` reads back its content: every chunk it names is
  /// stored with its bytes (a chunk's body is the bytes its name is the hash of).
  pure def chunksHold(b: Bucket, ids: Set[CommitId]): bool =
    chunksNamedBy(b, ids).forall(k => isLive(b.chunks, k) and body(b.chunks, k) == k)
}
