//! Amortized compaction: ordinary commits relocate a bounded part of the tip's small packs.
//!
//! Each commit puts its new files in one new pack, so a busy branch's tip ends up spread over
//! many small packs and a cold checkout needs a request per pack. `pail maintenance repack`
//! fixes that in one separate commit; this spreads the same work over real commits.
//!
//! A commit whose base has at least [`Settings::min_packs`] small packs appends a bounded
//! relocation of old files to the pack it uploads anyway: no extra request, commit or
//! download. The client re-encodes *clean local* files whose content hash matches the tip's
//! entry (a missing, edited or sparse-excluded file is skipped; nothing is fetched), and the
//! entries ride along as soft relocations ([`crate::repo::Reloc`]): on a replay, combine or
//! intent round a relocation whose path the head changed or moved is dropped silently, so
//! relocations never make a user's commit conflict, fail or retry more
//! (`verify/README.md`, "Amortized compaction").
//!
//! Heuristic:
//! 1. Only when the base names at least `min_packs` small packs (live bytes under
//!    [`SMALL_PACK_BYTES`]; a pack's size is not in the manifest, so the tip's own bytes in it
//!    stand in, which also counts a mostly dead big pack, whose checkout request is as wasted).
//! 2. Budget: `extra <= clamp(alpha * new_bytes, FLOOR_BYTES, CAP_BYTES)`, alpha = 1, so the
//!    bytes relocated keep pace with what commits add and fragmentation stops growing. The
//!    old packs stay (history names them): the extra storage per commit is at most the budget.
//! 3. Whole packs, smallest first (emptying a pack removes a checkout request); among packs of
//!    similar size, those holding paths near the commit's own (a shared directory) first.
//! 4. Moving a path rewrites the manifest leaf page that holds it, and that costs more upload
//!    than the blob when the paths are scattered (measured on `firstlanding`: a third of the
//!    extra bytes were pages). So each leaf the relocation touches beyond the ones the commit
//!    touches anyway is charged its size ([`ENTRY_BYTES`] per entry) against the same budget.
//!
//! Opt out with `maintenance.amortize = false`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use bytes::Bytes;

use crate::config::Config;
use crate::hash::Hash;
use crate::manifest::{Entry, Loc};
use crate::pack::INLINE_MAX;

/// Packs with less of the tip than this count as small (`repack`'s `DEFAULT_MIN_PACK_BYTES`).
pub const SMALL_PACK_BYTES: u64 = crate::repack::DEFAULT_MIN_PACK_BYTES;
/// Small packs at the base from which a commit relocates.
pub const MIN_PACKS: usize = 16;
/// The budget is this many times the commit's new bytes ...
pub const ALPHA: f64 = 1.0;
/// ... but at least this much (a commit that adds little still helps) ...
pub const FLOOR_BYTES: u64 = 256 << 10;
/// ... and at most this much (bounds a commit's extra upload).
pub const CAP_BYTES: u64 = 4 << 20;
/// Stored bytes of a manifest entry, about: what a leaf page the commit would not rewrite
/// anyway costs against the budget is its entries times this (measured on `firstlanding`: a
/// 256-entry leaf is 37 KB).
pub const ENTRY_BYTES: u64 = 140;
/// Candidate packs tried at most (a pack with a dirty file is skipped).
const MAX_TRIES: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub min_packs: usize,
    pub alpha: f64,
    pub floor: u64,
    pub cap: u64,
    pub small_bytes: u64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            enabled: true,
            min_packs: MIN_PACKS,
            alpha: ALPHA,
            floor: FLOOR_BYTES,
            cap: CAP_BYTES,
            small_bytes: SMALL_PACK_BYTES,
        }
    }
}

impl Settings {
    /// `maintenance.amortize` (bool, default on), `maintenance.amortizeSmallPacks` (the
    /// trigger, default 16; 0 turns it off), `maintenance.amortizeFactor` (alpha, default 1).
    pub fn from_config(cfg: &Config) -> Settings {
        let mut s = Settings::default();
        if let Some(on) = cfg.get_bool("maintenance.amortize") {
            s.enabled = on;
        }
        if let Some(n) = cfg
            .get("maintenance.amortizeSmallPacks")
            .and_then(|v| v.trim().parse::<usize>().ok())
        {
            s.min_packs = n;
            s.enabled &= n > 0;
        }
        if let Some(a) = cfg
            .get("maintenance.amortizeFactor")
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|a| a.is_finite() && *a >= 0.0)
        {
            s.alpha = a;
        }
        s
    }

    /// Bytes of old files a commit adding `new_bytes` may relocate.
    pub fn budget(&self, new_bytes: u64) -> u64 {
        let scaled = (new_bytes as f64 * self.alpha) as u64;
        scaled.clamp(self.floor.min(self.cap), self.cap)
    }
}

/// A small pack of the base and the tip's files in it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub pack: Hash,
    /// The stored bytes of the distinct blobs the tip keeps in it.
    pub live: u64,
    pub entries: Vec<Entry>,
    /// The manifest leaves (indices into the base's leaf list) holding those entries.
    pub leaves: Vec<usize>,
}

/// The leaves of the base's manifest: each one's largest key and what rewriting it costs.
#[derive(Debug, Clone, Default)]
pub struct Leaves {
    pub keys: Vec<String>,
    pub cost: Vec<u64>,
}

impl Leaves {
    /// From the base's leaf list (`Repo::leaf_refs`).
    pub fn of(children: &[crate::manifest::Child]) -> Leaves {
        Leaves {
            keys: children.iter().map(|c| c.key.clone()).collect(),
            cost: children.iter().map(|c| c.files * ENTRY_BYTES).collect(),
        }
    }

    /// The leaf holding `path`.
    pub fn of_path(&self, path: &str) -> usize {
        self.keys
            .partition_point(|k| k.as_str() < path)
            .min(self.keys.len().saturating_sub(1))
    }

    fn cost_of(&self, leaf: usize) -> u64 {
        self.cost.get(leaf).copied().unwrap_or(0)
    }
}

fn packed(e: &Entry) -> Option<(Hash, u64, u64)> {
    match &e.loc {
        Loc::Pack { pack, off, len } => Some((*pack, *off, *len)),
        _ => None,
    }
}

/// How many directory levels `path` shares with the deepest of `dirs` (the directories of the
/// commit's own paths and their ancestors, `""` the root).
fn nearness(path: &str, dirs: &HashSet<&str>) -> usize {
    let mut best = 0;
    let mut depth = 0;
    for (i, c) in path.char_indices() {
        if c == '/' {
            depth += 1;
            if dirs.contains(&path[..i]) {
                best = depth;
            }
        }
    }
    best
}

/// The small packs of `base` worth relocating from, best first, or none when the base has
/// fewer than `min_packs` of them. `own` are the paths the commit writes or deletes (their old
/// blobs are about to be dead, so they are not candidates).
pub fn candidates<'a>(
    s: &Settings,
    base: impl Iterator<Item = &'a Entry>,
    own: &[&str],
    leaves: &Leaves,
) -> Vec<Candidate> {
    if !s.enabled || s.min_packs == 0 {
        return vec![];
    }
    let own_set: HashSet<&str> = own.iter().copied().collect();
    let mut by_pack: HashMap<Hash, (HashSet<u64>, u64, Vec<&Entry>)> = HashMap::new();
    for e in base {
        if own_set.contains(e.path.as_str()) {
            continue;
        }
        if let Some((pack, off, len)) = packed(e) {
            let p = by_pack.entry(pack).or_default();
            if p.0.insert(off) {
                p.1 += len;
            }
            p.2.push(e);
        }
    }
    let mut small: Vec<(Hash, u64, Vec<&Entry>)> = by_pack
        .into_iter()
        .filter(|(_, (_, live, _))| *live < s.small_bytes)
        .map(|(h, (_, live, es))| (h, live, es))
        .collect();
    if small.len() < s.min_packs {
        return vec![];
    }
    // The commit's directories and their ancestors.
    let mut dirs: HashSet<&str> = HashSet::new();
    for p in own {
        let mut d = *p;
        while let Some((parent, _)) = d.rsplit_once('/') {
            dirs.insert(parent);
            d = parent;
        }
    }
    let near = |es: &[&Entry]| {
        es.iter()
            .map(|e| nearness(&e.path, &dirs))
            .max()
            .unwrap_or(0)
    };
    small.sort_by(|a, b| {
        // Size classes of 16 KiB, so ties are real; then nearer first; then exact, then name.
        (a.1 >> 14)
            .cmp(&(b.1 >> 14))
            .then(near(&b.2).cmp(&near(&a.2)))
            .then(a.1.cmp(&b.1))
            .then(a.0.cmp(&b.0))
    });
    small
        .into_iter()
        .map(|(pack, live, es)| {
            let mut leaves: Vec<usize> = es.iter().map(|e| leaves.of_path(&e.path)).collect();
            leaves.sort_unstable();
            leaves.dedup();
            Candidate {
                pack,
                live,
                entries: es.into_iter().cloned().collect(),
                leaves,
            }
        })
        .collect()
}

/// A local file as the entry has it: read, and checked against the entry's hash.
fn clean(root: &Path, e: &Entry) -> Option<Bytes> {
    let (kind, data) = crate::worktree::read_disk(&root.join(&e.path)).ok()??;
    (kind == e.kind && data.len() as u64 == e.size && Hash::of(&data) == e.hash)
        .then(|| Bytes::from(data))
}

/// The files to relocate, read from the working copy at `root`: whole candidate packs in
/// order while the budget lasts, skipping a pack with a file that is missing, edited or
/// excluded. Nothing is fetched.
pub fn gather(
    root: &Path,
    cands: &[Candidate],
    leaves: &Leaves,
    budget: u64,
    touched: HashSet<usize>,
) -> Vec<(Entry, Bytes)> {
    gather_with(cands, leaves, budget, touched, |e| clean(root, e))
}

/// The manifest leaves holding the commit's own paths: it rewrites them anyway.
pub fn touched_leaves<'a>(leaves: &Leaves, own: impl Iterator<Item = &'a str>) -> HashSet<usize> {
    own.map(|p| leaves.of_path(p)).collect()
}

/// [`gather`] with the content of an entry supplied by `read` (None: not at hand). The caller
/// vouches that what it returns hashes to the entry's `hash`.
pub fn gather_with(
    cands: &[Candidate],
    leaves: &Leaves,
    budget: u64,
    mut touched: HashSet<usize>,
    mut read: impl FnMut(&Entry) -> Option<Bytes>,
) -> Vec<(Entry, Bytes)> {
    let mut left = budget;
    let mut out: Vec<(Entry, Bytes)> = vec![];
    for c in cands.iter().take(MAX_TRIES) {
        let pages: u64 = c
            .leaves
            .iter()
            .filter(|l| !touched.contains(l))
            .map(|l| leaves.cost_of(*l))
            .sum();
        let cost = c.live + pages;
        if cost > left {
            // Packs are sorted by size class: a later one may still fit.
            continue;
        }
        let files: Option<Vec<(Entry, Bytes)>> = c
            .entries
            .iter()
            .map(|e| read(e).map(|d| (e.clone(), d)))
            .collect();
        let Some(files) = files else { continue };
        left -= cost;
        touched.extend(c.leaves.iter().copied());
        out.extend(files);
        if left == 0 {
            break;
        }
    }
    out
}

/// Bytes a commit adds: the packed files among `sizes` (inline ones add nothing to a pack).
pub fn new_bytes(sizes: impl Iterator<Item = u64>) -> u64 {
    sizes.filter(|s| *s as usize > INLINE_MAX).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Kind;

    fn pack(n: u8) -> Hash {
        Hash::of(&[n])
    }

    fn entry(path: &str, p: u8, off: u64, len: u64) -> Entry {
        Entry {
            path: path.into(),
            kind: Kind::File,
            size: len * 2,
            hash: Hash::of(path.as_bytes()),
            loc: Loc::Pack {
                pack: pack(p),
                off,
                len,
            },
        }
    }

    /// `n` packs of one file of `len` bytes each, in directory `d<i>`.
    fn tip(n: u8, len: u64) -> BTreeMap<String, Entry> {
        (0..n)
            .map(|i| {
                let path = format!("d{i}/f");
                (path.clone(), entry(&path, i, 0, len))
            })
            .collect()
    }

    #[test]
    fn budget_is_clamped() {
        let s = Settings::default();
        assert_eq!(s.budget(0), FLOOR_BYTES);
        assert_eq!(s.budget(1 << 20), 1 << 20);
        assert_eq!(s.budget(100 << 20), CAP_BYTES);
        let half = Settings {
            alpha: 0.5,
            ..Settings::default()
        };
        assert_eq!(half.budget(2 << 20), 1 << 20);
        assert_eq!(half.budget(10), FLOOR_BYTES);
    }

    #[test]
    fn nothing_below_the_trigger() {
        let s = Settings::default();
        assert!(candidates(&s, tip(15, 100).values(), &[], &Leaves::default()).is_empty());
        assert_eq!(
            candidates(&s, tip(16, 100).values(), &[], &Leaves::default()).len(),
            16
        );
    }

    #[test]
    fn big_packs_are_not_small() {
        let s = Settings::default();
        let mut base = tip(16, 100);
        base.insert("big".into(), entry("big", 99, 0, 2 << 20));
        let c = candidates(&s, base.values(), &[], &Leaves::default());
        assert_eq!(c.len(), 16);
        assert!(c.iter().all(|c| c.live < SMALL_PACK_BYTES));
    }

    #[test]
    fn disabled_selects_nothing() {
        let s = Settings {
            enabled: false,
            ..Settings::default()
        };
        assert!(candidates(&s, tip(40, 100).values(), &[], &Leaves::default()).is_empty());
    }

    #[test]
    fn smallest_first_and_ties_go_near() {
        let s = Settings::default();
        let mut base = tip(16, 100_000);
        // One pack much smaller, one the same size but next to the commit's own path.
        base.insert("d3/f".into(), entry("d3/f", 3, 0, 1_000));
        base.insert("d9/x/f".into(), entry("d9/x/f", 9, 0, 100_000));
        base.remove("d9/f");
        base.insert("d9/f".into(), entry("d9/f", 9, 0, 100_000));
        let c = candidates(&s, base.values(), &["d9/x/new"], &Leaves::default());
        assert_eq!(c[0].pack, pack(3), "smallest first");
        // Among the 100_000-byte packs, the one holding d9/x/f (shares d9/x) comes first.
        assert_eq!(c[1].pack, pack(9));
    }

    #[test]
    fn own_paths_are_not_candidates() {
        let s = Settings::default();
        let base = tip(16, 100);
        let c = candidates(&s, base.values(), &["d0/f"], &Leaves::default());
        assert!(c.is_empty(), "15 small packs are left: below the trigger");
    }

    #[test]
    fn whole_packs_within_the_budget_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut base = BTreeMap::new();
        for i in 0..16u8 {
            let path = format!("d{i}/f");
            let data = vec![i + 1; 4000];
            std::fs::create_dir_all(dir.path().join(format!("d{i}"))).unwrap();
            std::fs::write(dir.path().join(&path), &data).unwrap();
            let e = Entry {
                path: path.clone(),
                kind: Kind::File,
                size: data.len() as u64,
                hash: Hash::of(&data),
                loc: Loc::Pack {
                    pack: pack(i),
                    off: 0,
                    len: 3000,
                },
            };
            base.insert(path, e);
        }
        let s = Settings::default();
        let c = candidates(&s, base.values(), &[], &Leaves::default());
        // 10_000 bytes of budget fits three whole 3000-byte packs, not a fourth.
        let got = gather(
            dir.path(),
            &c,
            &Leaves::default(),
            10_000,
            HashSet::from([0]),
        );
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|(e, d)| Hash::of(d) == e.hash));
        // An edited file skips its pack; a missing one too.
        let victim = &got[0].0.path.clone();
        std::fs::write(dir.path().join(victim), b"edited").unwrap();
        let got2 = gather(
            dir.path(),
            &c,
            &Leaves::default(),
            10_000,
            HashSet::from([0]),
        );
        assert_eq!(got2.len(), 3);
        assert!(got2.iter().all(|(e, _)| &e.path != victim));
        std::fs::remove_file(dir.path().join(&got2[0].0.path)).unwrap();
        let got3 = gather(
            dir.path(),
            &c,
            &Leaves::default(),
            10_000,
            HashSet::from([0]),
        );
        assert!(
            got3.iter()
                .all(|(e, _)| e.path != got2[0].0.path && &e.path != victim)
        );
    }

    #[test]
    fn scattered_leaves_are_charged() {
        let dir = tempfile::tempdir().unwrap();
        let mut base = BTreeMap::new();
        // One leaf of 100 entries (14_000 bytes) per directory.
        let leaves = Leaves {
            keys: (0..16).map(|i| format!("d{i:02}/z")).collect(),
            cost: vec![100 * ENTRY_BYTES; 16],
        };
        for i in 0..16u8 {
            let path = format!("d{i:02}/f");
            let data = vec![i + 1; 4000];
            std::fs::create_dir_all(dir.path().join(format!("d{i:02}"))).unwrap();
            std::fs::write(dir.path().join(&path), &data).unwrap();
            base.insert(
                path.clone(),
                Entry {
                    path,
                    kind: Kind::File,
                    size: 4000,
                    hash: Hash::of(&data),
                    loc: Loc::Pack {
                        pack: pack(i),
                        off: 0,
                        len: 3000,
                    },
                },
            );
        }
        let s = Settings::default();
        // Each pack costs its 3000 bytes and a 14_000-byte leaf.
        let c = candidates(&s, base.values(), &[], &leaves);
        let got = gather(dir.path(), &c, &leaves, 40_000, HashSet::new());
        assert_eq!(got.len(), 2, "40_000 / (3000 + 14_000) = 2 packs");
        // Leaves the commit rewrites anyway are free.
        let all: HashSet<usize> = (0..16).collect();
        let got = gather(dir.path(), &c, &leaves, 40_000, all);
        assert_eq!(got.len(), 13, "40_000 / 3000 = 13 packs");
    }

    #[test]
    fn config_keys() {
        let mut f = crate::config::File::default();
        f.set("maintenance.amortize", "false").unwrap();
        let cfg = Config {
            global: f,
            local: None,
        };
        assert!(!Settings::from_config(&cfg).enabled);
        let mut f = crate::config::File::default();
        f.set("maintenance.amortizeSmallPacks", "4").unwrap();
        f.set("maintenance.amortizeFactor", "2").unwrap();
        let s = Settings::from_config(&Config {
            global: f,
            local: None,
        });
        assert!(s.enabled);
        assert_eq!((s.min_packs, s.alpha), (4, 2.0));
        let mut f = crate::config::File::default();
        f.set("maintenance.amortizeSmallPacks", "0").unwrap();
        assert!(
            !Settings::from_config(&Config {
                global: f,
                local: None
            })
            .enabled
        );
    }
}
