//! Commits and refs. Commits are immutable JSON objects named by their BLAKE3 hash;
//! a ref is a tiny mutable JSON object holding one commit hash.

use std::collections::BTreeMap;

use anyhow::Result;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::hash::Hash;
use crate::manifest::{ChunkRef, Entry, Kind, Loc};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Commit {
    /// Linear history: at most one parent.
    pub parent: Option<Hash>,
    /// Root page of the manifest.
    pub root: Hash,
    /// Depth in history (root commit = 0); lets ancestor searches jump.
    pub generation: u64,
    /// Total files and bytes in the snapshot.
    pub files: u64,
    pub bytes: u64,
    /// `Name <email>`.
    pub author: String,
    /// `<unix seconds> <+hhmm>`, as Git writes it.
    pub author_date: String,
    pub committer: String,
    pub committer_date: String,
    pub message: String,
    /// Free-form metadata, e.g. `git.commit` for mirrored history.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
    /// Every file this commit changed relative to its parent (as Mononoke's Bonsai
    /// changesets carry their file changes). Readers get a commit's delta from the commit
    /// alone, without diffing manifests. None in commits written before change lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Changes>,
}

/// A commit's change list, kept as the JSON it was stored as and parsed only when read: a
/// history walk shouldn't pay for an import commit that lists every file.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Changes(Box<serde_json::value::RawValue>);

impl PartialEq for Changes {
    fn eq(&self, o: &Self) -> bool {
        self.0.get() == o.0.get()
    }
}

impl Eq for Changes {}

impl Changes {
    pub fn new(map: &BTreeMap<String, FileChange>) -> Changes {
        Changes(serde_json::value::to_raw_value(map).expect("change list serializes"))
    }

    pub fn parse(&self) -> Result<BTreeMap<String, FileChange>> {
        Ok(serde_json::from_str(self.0.get())?)
    }
}

/// One path's change: its state before (None: added) and after (None: deleted).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileChange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<FileState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<FileState>,
}

/// A file version: kind, size, content hash, and (for the new side) where its bytes live.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileState {
    #[serde(rename = "k")]
    pub kind: KindTag,
    #[serde(rename = "s")]
    pub size: u64,
    #[serde(rename = "h")]
    pub hash: Hash,
    #[serde(rename = "l", default, skip_serializing_if = "Option::is_none")]
    pub loc: Option<LocRef>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum KindTag {
    #[serde(rename = "f")]
    File,
    #[serde(rename = "x")]
    Exec,
    #[serde(rename = "l")]
    Symlink,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum LocRef {
    /// Inline bytes, hex.
    #[serde(rename = "i")]
    Inline(String),
    /// Pack, offset, length.
    #[serde(rename = "p")]
    Pack(Hash, u64, u64),
    /// Chunk hashes and lengths.
    #[serde(rename = "c")]
    Chunks(Vec<(Hash, u64)>),
}

impl FileState {
    pub fn of(e: &Entry, with_loc: bool) -> FileState {
        let kind = match e.kind {
            Kind::File => KindTag::File,
            Kind::Exec => KindTag::Exec,
            Kind::Symlink => KindTag::Symlink,
        };
        let loc = with_loc
            .then(|| match &e.loc {
                Loc::Inline(b) => Some(LocRef::Inline(hex::encode(b))),
                Loc::Pack { pack, off, len } => Some(LocRef::Pack(*pack, *off, *len)),
                Loc::Chunks(cs) => {
                    Some(LocRef::Chunks(cs.iter().map(|c| (c.hash, c.len)).collect()))
                }
                Loc::Unresolved => None,
            })
            .flatten();
        FileState {
            kind,
            size: e.size,
            hash: e.hash,
            loc,
        }
    }

    pub fn entry(&self, path: &str) -> Entry {
        let kind = match self.kind {
            KindTag::File => Kind::File,
            KindTag::Exec => Kind::Exec,
            KindTag::Symlink => Kind::Symlink,
        };
        let loc = match &self.loc {
            Some(LocRef::Inline(h)) => Loc::Inline(hex::decode(h).unwrap_or_default()),
            Some(LocRef::Pack(pack, off, len)) => Loc::Pack {
                pack: *pack,
                off: *off,
                len: *len,
            },
            Some(LocRef::Chunks(cs)) => Loc::Chunks(
                cs.iter()
                    .map(|(h, l)| ChunkRef { hash: *h, len: *l })
                    .collect(),
            ),
            None => Loc::Unresolved,
        };
        Entry {
            path: path.to_string(),
            kind,
            size: self.size,
            hash: self.hash,
            loc,
        }
    }
}

impl Commit {
    pub fn encode(&self) -> (Hash, Bytes) {
        let data = serde_json::to_vec(self).expect("commit serializes");
        (Hash::of(&data), Bytes::from(data))
    }

    pub fn decode(expected: &Hash, data: &[u8]) -> Result<Commit> {
        let got = Hash::of(data);
        anyhow::ensure!(
            &got == expected,
            "commit hash mismatch: expected {expected}, got {got}"
        );
        Ok(serde_json::from_slice(data)?)
    }

    /// The commit's changes as tree changes, in path order, if it records them.
    pub fn change_list(&self) -> Option<Vec<crate::manifest::Change>> {
        let cs = self.changes.as_ref()?.parse().ok()?;
        Some(
            cs.iter()
                .map(|(p, c)| crate::manifest::Change {
                    path: p.clone(),
                    old: c.old.as_ref().map(|s| s.entry(p)),
                    new: c.new.as_ref().map(|s| s.entry(p)),
                })
                .collect(),
        )
    }

    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RefValue {
    pub commit: Hash,
    /// The commit's manifest root and generation, so a writer can build on the head without
    /// fetching its commit object first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<Hash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// Intents landed most recently on this ref (newest last), so writers combining pending
    /// intents never land one twice. Every writer of the ref carries it forward.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<Landed>,
    #[serde(flatten)]
    pub lineage: Lineage,
}

/// Which version of a branch's history a ref holds. Fast-forwards carry it unchanged; a
/// rewrite (any move to a commit that does not descend from the old tip) bumps `epoch` and
/// names the reflog record that maps the old history to the new ([`Rewrite`]). A client
/// whose base is from an older epoch knows its branch was rewritten under it, and the chain
/// of records tells it where each of its commits went.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lineage {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub epoch: u64,
    /// The latest rewrite: `reflog/<branch>/<rewrite>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite: Option<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A rewrite of a branch, kept as its reflog entry (`reflog/<branch>/<id>`). Entries written
/// before epochs have only `old` and `new`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Rewrite {
    pub old: Hash,
    pub new: Hash,
    /// The epoch the branch entered with this rewrite.
    #[serde(default)]
    pub epoch: u64,
    /// The previous rewrite's record, so the chain can be walked back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    /// What rewrote it: `rebase`, `undo` or `reset`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub op: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub by: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub date: String,
    /// Commits of the old history that were rewritten, each to its successor (None:
    /// dropped). Commits the new history still contains are not listed; neither are
    /// commits dropped by a plain reset (their successor is their nearest kept ancestor).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub map: BTreeMap<Hash, Option<Hash>>,
}

pub fn reflog_key(branch: &str, id: &str) -> String {
    format!("reflog/{branch}/{id}")
}

/// An intent and the commit it landed as.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Landed {
    pub intent: String,
    pub commit: Hash,
}

pub fn commit_key(h: &Hash) -> String {
    format!("commits/{}", h.hex())
}

pub fn page_key(h: &Hash) -> String {
    format!("pages/{}", h.hex())
}

pub fn ref_key(branch: &str) -> String {
    format!("refs/{branch}")
}

pub fn validate_branch(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && !name.starts_with('/')
            && !name.ends_with('/')
            && !name.contains("//")
            && !name.split('/').any(|s| s == "." || s == "..")
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)),
        "invalid branch name {name:?}"
    );
    Ok(())
}

/// Current time as a Git-style `<unix seconds> +0000` date. `PAIL_DATE` (`<unix seconds>
/// [<+zone>]`) stands in for it, as Git's GIT_AUTHOR_DATE/GIT_COMMITTER_DATE do: commit ids
/// then depend only on content, for reproducible tests and scripts.
pub fn now_date() -> String {
    if let Ok(d) = std::env::var("PAIL_DATE") {
        let mut parts = d.split_whitespace();
        if let Some(secs) = parts.next().filter(|s| s.parse::<u64>().is_ok()) {
            return format!("{secs} {}", parts.next().unwrap_or("+0000"));
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs} +0000")
}
