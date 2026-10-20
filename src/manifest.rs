//! The manifest: a commit's table of contents, `path → entry`, stored as a prolly tree.
//!
//! * Leaves hold sorted entries; internal nodes hold `(last_key, child_hash, counts)`.
//! * A page ends after any key whose salted hash falls under the level's threshold.
//!   Boundaries depend on keys only, so editing file contents never moves a boundary and
//!   the same set of paths always produces the same pages (canonical root hash).
//! * Page hash = BLAKE3 of the uncompressed canonical encoding; stored bytes are zstd.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use bytes::Bytes;

use crate::enc::{Reader, Writer};
use crate::hash::Hash;

/// Average entries per leaf page (~40KB uncompressed).
pub const LEAF_TARGET: u32 = 512;
/// Average children per internal page.
pub const NODE_TARGET: u32 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File = 0,
    Exec = 1,
    Symlink = 2,
}

impl Kind {
    pub fn from_u8(v: u8) -> Result<Kind> {
        Ok(match v {
            0 => Kind::File,
            1 => Kind::Exec,
            2 => Kind::Symlink,
            _ => bail!("bad kind {v}"),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkRef {
    pub hash: Hash,
    pub len: u64,
}

/// Where a file's bytes live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loc {
    /// Small files (≤ INLINE_MAX) and symlink targets live in the entry itself.
    Inline(Vec<u8>),
    /// A range of a pack object. `len` covers the codec byte plus the stored data.
    Pack { pack: Hash, off: u64, len: u64 },
    /// Large files: content-defined chunks, each its own object.
    Chunks(Vec<ChunkRef>),
    /// Location not known yet: an old file version named by a commit's change list. Readers
    /// find it by content hash in the local cache, or in the parent's manifest. Never stored
    /// in a page.
    Unresolved,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub hash: Hash,
    pub loc: Loc,
}

impl Entry {
    /// Same file as far as users can tell: identical kind and contents. `loc` is ignored.
    pub fn same_content(&self, other: &Entry) -> bool {
        self.kind == other.kind && self.hash == other.hash && self.size == other.size
    }
}

/// A reference from a parent page to a child page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Child {
    /// Largest key in the child's subtree.
    pub key: String,
    pub hash: Hash,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub enum Page {
    Leaf(Vec<Entry>),
    Node { level: u8, children: Vec<Child> },
}

impl Page {
    pub fn level(&self) -> u8 {
        match self {
            Page::Leaf(_) => 0,
            Page::Node { level, .. } => *level,
        }
    }
}

const PAGE_MAGIC: &[u8; 2] = b"PG";
const PAGE_VERSION: u8 = 1;

fn common_prefix(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

pub fn encode_page(page: &Page) -> Vec<u8> {
    let mut w = Writer::default();
    w.buf.extend_from_slice(PAGE_MAGIC);
    w.u8(PAGE_VERSION);
    w.u8(page.level());
    match page {
        Page::Leaf(entries) => {
            // Pack table: each pack id is written once per page and referenced by index.
            let mut packs: Vec<Hash> = vec![];
            let mut pack_idx: BTreeMap<Hash, usize> = BTreeMap::new();
            for e in entries {
                if let Loc::Pack { pack, .. } = &e.loc {
                    pack_idx.entry(*pack).or_insert_with(|| {
                        packs.push(*pack);
                        packs.len() - 1
                    });
                }
            }
            w.varint(packs.len() as u64);
            for p in &packs {
                w.hash(p);
            }
            w.varint(entries.len() as u64);
            let mut prev = "";
            for e in entries {
                let shared = common_prefix(prev, &e.path);
                w.varint(shared as u64);
                w.bytes(&e.path.as_bytes()[shared..]);
                w.u8(e.kind as u8);
                w.varint(e.size);
                w.hash(&e.hash);
                match &e.loc {
                    Loc::Inline(b) => {
                        w.u8(0);
                        w.bytes(b);
                    }
                    Loc::Pack { pack, off, len } => {
                        w.u8(1);
                        w.varint(pack_idx[pack] as u64);
                        w.varint(*off);
                        w.varint(*len);
                    }
                    Loc::Chunks(chunks) => {
                        w.u8(2);
                        w.varint(chunks.len() as u64);
                        for c in chunks {
                            w.hash(&c.hash);
                            w.varint(c.len);
                        }
                    }
                    Loc::Unresolved => panic!("unresolved location for {} in a manifest page", e.path),
                }
                prev = &e.path;
            }
        }
        Page::Node { children, .. } => {
            w.varint(children.len() as u64);
            let mut prev = "";
            for c in children {
                let shared = common_prefix(prev, &c.key);
                w.varint(shared as u64);
                w.bytes(&c.key.as_bytes()[shared..]);
                w.hash(&c.hash);
                w.varint(c.files);
                w.varint(c.bytes);
                prev = &c.key;
            }
        }
    }
    w.buf
}

pub fn decode_page(buf: &[u8]) -> Result<Page> {
    let mut r = Reader::new(buf);
    if r.take(2)? != PAGE_MAGIC {
        bail!("not a page");
    }
    let version = r.u8()?;
    if version != PAGE_VERSION {
        bail!("unsupported page version {version}");
    }
    let level = r.u8()?;
    let read_key = |r: &mut Reader, prev: &str| -> Result<String> {
        let shared = r.varint()? as usize;
        let suffix = r.bytes()?;
        if shared > prev.len() {
            bail!("corrupt key prefix");
        }
        let mut k = prev.as_bytes()[..shared].to_vec();
        k.extend_from_slice(suffix);
        Ok(String::from_utf8(k)?)
    };
    if level == 0 {
        let np = r.varint()? as usize;
        let packs: Vec<Hash> = (0..np).map(|_| r.hash()).collect::<Result<_>>()?;
        let n = r.varint()? as usize;
        let mut entries = Vec::with_capacity(n);
        let mut prev = String::new();
        for _ in 0..n {
            let path = read_key(&mut r, &prev)?;
            let kind = Kind::from_u8(r.u8()?)?;
            let size = r.varint()?;
            let hash = r.hash()?;
            let loc = match r.u8()? {
                0 => Loc::Inline(r.bytes()?.to_vec()),
                1 => {
                    let i = r.varint()? as usize;
                    let pack = *packs.get(i).ok_or_else(|| anyhow::anyhow!("bad pack index"))?;
                    Loc::Pack { pack, off: r.varint()?, len: r.varint()? }
                }
                2 => {
                    let n = r.varint()? as usize;
                    let chunks = (0..n)
                        .map(|_| Ok(ChunkRef { hash: r.hash()?, len: r.varint()? }))
                        .collect::<Result<_>>()?;
                    Loc::Chunks(chunks)
                }
                t => bail!("bad loc tag {t}"),
            };
            prev = path.clone();
            entries.push(Entry { path, kind, size, hash, loc });
        }
        Ok(Page::Leaf(entries))
    } else {
        let n = r.varint()? as usize;
        let mut children = Vec::with_capacity(n);
        let mut prev = String::new();
        for _ in 0..n {
            let key = read_key(&mut r, &prev)?;
            let hash = r.hash()?;
            let files = r.varint()?;
            let bytes = r.varint()?;
            prev = key.clone();
            children.push(Child { key, hash, files, bytes });
        }
        Ok(Page::Node { level, children })
    }
}

/// A page ready to upload.
#[derive(Clone)]
pub struct BuiltPage {
    pub hash: Hash,
    /// zstd-compressed encoding, as stored in `pages/<hash>`.
    pub data: Bytes,
    pub page: Page,
}

pub fn build_page(page: Page) -> BuiltPage {
    let raw = encode_page(&page);
    let hash = Hash::of(&raw);
    let data = Bytes::from(zstd::encode_all(&raw[..], 3).expect("zstd encode"));
    BuiltPage { hash, data, page }
}

pub fn decode_stored_page(expected: &Hash, stored: &[u8]) -> Result<Page> {
    let raw = zstd::decode_all(stored)?;
    let got = Hash::of(&raw);
    if &got != expected {
        bail!("page hash mismatch: expected {expected}, got {got}");
    }
    decode_page(&raw)
}

pub fn is_boundary(level: u8, key: &str) -> bool {
    let target = if level == 0 { LEAF_TARGET } else { NODE_TARGET };
    let mut h = blake3::Hasher::new();
    h.update(&[level]);
    h.update(key.as_bytes());
    let v = u32::from_le_bytes(h.finalize().as_bytes()[..4].try_into().unwrap());
    v < u32::MAX / target
}

/// Split a sorted run into groups that each end on a boundary key.
/// Returns the terminated groups and the trailing unterminated items.
pub fn split_runs<T>(items: Vec<T>, level: u8, key: impl Fn(&T) -> &str) -> (Vec<Vec<T>>, Vec<T>) {
    let mut groups = vec![];
    let mut cur = vec![];
    for it in items {
        let b = is_boundary(level, key(&it));
        cur.push(it);
        if b {
            groups.push(std::mem::take(&mut cur));
        }
    }
    (groups, cur)
}

pub fn leaf_child(b: &BuiltPage) -> Child {
    match &b.page {
        Page::Leaf(es) => Child {
            key: es.last().map(|e| e.path.clone()).unwrap_or_default(),
            hash: b.hash,
            files: es.len() as u64,
            bytes: es.iter().map(|e| e.size).sum(),
        },
        Page::Node { children, .. } => Child {
            key: children.last().map(|c| c.key.clone()).unwrap_or_default(),
            hash: b.hash,
            files: children.iter().map(|c| c.files).sum(),
            bytes: children.iter().map(|c| c.bytes).sum(),
        },
    }
}

/// Build leaf pages for a sorted entry run that starts at a page boundary.
pub fn build_leaves(entries: Vec<Entry>) -> Vec<BuiltPage> {
    let (mut groups, tail) = split_runs(entries, 0, |e| e.path.as_str());
    if !tail.is_empty() {
        groups.push(tail);
    }
    groups.into_iter().map(|g| build_page(Page::Leaf(g))).collect()
}

/// Build the internal levels above a complete, ordered list of leaf references.
/// Returns the root hash and every internal page built (callers skip known ones on upload).
pub fn build_upper(mut children: Vec<Child>) -> (Hash, Vec<BuiltPage>) {
    let mut out = vec![];
    if children.is_empty() {
        let empty = build_page(Page::Leaf(vec![]));
        let h = empty.hash;
        out.push(empty);
        return (h, out);
    }
    let mut level: u8 = 1;
    while children.len() > 1 {
        let (mut groups, tail) = split_runs(children, level, |c| c.key.as_str());
        if !tail.is_empty() {
            groups.push(tail);
        }
        // Pathological guard: a level that fails to reduce collapses into one page.
        if level >= 32 {
            groups = vec![groups.into_iter().flatten().collect()];
        }
        children = groups
            .into_iter()
            .map(|g| {
                let b = build_page(Page::Node { level, children: g });
                let c = leaf_child(&b);
                out.push(b);
                c
            })
            .collect();
        level += 1;
    }
    (children[0].hash, out)
}

#[derive(Clone, Debug)]
pub struct Change {
    pub path: String,
    pub old: Option<Entry>,
    pub new: Option<Entry>,
}

/// Merge-join two sorted entry lists into the changes from `a` to `b`.
pub fn diff_sorted(a: &[Entry], b: &[Entry]) -> Vec<Change> {
    let (mut i, mut j) = (0, 0);
    let mut out = vec![];
    while i < a.len() || j < b.len() {
        let ord = match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) => x.path.cmp(&y.path),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => unreachable!(),
        };
        match ord {
            std::cmp::Ordering::Less => {
                out.push(Change { path: a[i].path.clone(), old: Some(a[i].clone()), new: None });
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(Change { path: b[j].path.clone(), old: None, new: Some(b[j].clone()) });
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                if !a[i].same_content(&b[j]) {
                    out.push(Change { path: a[i].path.clone(), old: Some(a[i].clone()), new: Some(b[j].clone()) });
                }
                i += 1;
                j += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(p: &str, n: u8) -> Entry {
        Entry { path: p.into(), kind: Kind::File, size: n as u64, hash: Hash([n; 32]), loc: Loc::Inline(vec![n]) }
    }

    #[test]
    fn page_roundtrip() {
        let p = Page::Leaf(vec![
            e("a/b", 1),
            Entry {
                path: "a/c".into(),
                kind: Kind::Exec,
                size: 9,
                hash: Hash([2; 32]),
                loc: Loc::Pack { pack: Hash([9; 32]), off: 5, len: 10 },
            },
            Entry {
                path: "z".into(),
                kind: Kind::Symlink,
                size: 9,
                hash: Hash([3; 32]),
                loc: Loc::Chunks(vec![ChunkRef { hash: Hash([4; 32]), len: 7 }]),
            },
        ]);
        let b = build_page(p);
        let back = decode_stored_page(&b.hash, &b.data).unwrap();
        match (b.page, back) {
            (Page::Leaf(a), Page::Leaf(c)) => assert_eq!(a, c),
            _ => panic!(),
        }
    }

    #[test]
    fn canonical_upper() {
        let entries: Vec<Entry> = (0..20000).map(|i| e(&format!("f/{i:06}"), (i % 250) as u8)).collect();
        let leaves = build_leaves(entries.clone());
        assert!(leaves.len() > 10);
        let (r1, _) = build_upper(leaves.iter().map(leaf_child).collect());
        let leaves2 = build_leaves(entries);
        let (r2, _) = build_upper(leaves2.iter().map(leaf_child).collect());
        assert_eq!(r1, r2);
    }
}
