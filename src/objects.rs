//! Commits and refs. Commits are immutable JSON objects named by their BLAKE3 hash;
//! a ref is a tiny mutable JSON object holding one commit hash.

use std::collections::BTreeMap;

use anyhow::Result;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::hash::Hash;

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
}

impl Commit {
    pub fn encode(&self) -> (Hash, Bytes) {
        let data = serde_json::to_vec(self).expect("commit serializes");
        (Hash::of(&data), Bytes::from(data))
    }

    pub fn decode(expected: &Hash, data: &[u8]) -> Result<Commit> {
        let got = Hash::of(data);
        anyhow::ensure!(&got == expected, "commit hash mismatch: expected {expected}, got {got}");
        Ok(serde_json::from_slice(data)?)
    }

    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefValue {
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
            && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)),
        "invalid branch name {name:?}"
    );
    Ok(())
}

/// Current time as a Git-style `<unix seconds> +0000` date.
pub fn now_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs} +0000")
}
