//! Reading a Git repository (for mirroring) and rebuilding Git object ids from pail data
//! (for proving a pail commit reproduces a Git commit byte for byte).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use crate::manifest::Kind;

struct CatFile {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

pub struct Git {
    pub dir: PathBuf,
    cat: Mutex<CatFile>,
}

#[derive(Debug, Clone)]
pub struct TreeItem {
    pub mode: String,
    pub oid: String,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct DiffItem {
    pub new_mode: String,
    pub new_oid: String,
    pub path: String,
    pub deleted: bool,
}

/// A Git commit object split into the parts pail stores.
#[derive(Debug, Clone)]
pub struct RawCommit {
    pub tree: String,
    pub parents: Vec<String>,
    pub author: String,
    pub author_date: String,
    pub committer: String,
    pub committer_date: String,
    /// Header lines after `committer` (gpgsig, mergetag, encoding...), verbatim.
    pub extra_headers: Vec<u8>,
    pub message: Vec<u8>,
}

pub fn kind_for_mode(mode: &str) -> Result<Kind> {
    Ok(match mode {
        "100644" => Kind::File,
        "100755" => Kind::Exec,
        "120000" => Kind::Symlink,
        m => bail!("unsupported git mode {m} (submodules are not modelled)"),
    })
}

pub fn mode_for_kind(k: Kind) -> &'static str {
    match k {
        Kind::File => "100644",
        Kind::Exec => "100755",
        Kind::Symlink => "120000",
    }
}

impl Git {
    pub fn open(dir: &Path) -> Result<Git> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("starting git cat-file")?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::with_capacity(1 << 20, child.stdout.take().unwrap());
        Ok(Git { dir: dir.to_path_buf(), cat: Mutex::new(CatFile { _child: child, stdin, stdout }) })
    }

    pub fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out = Command::new("git").arg("-C").arg(&self.dir).args(args).output()?;
        if !out.status.success() {
            bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout)
    }

    pub fn rev_parse(&self, rev: &str) -> Result<String> {
        Ok(String::from_utf8(self.run(&["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?)?.trim().to_string())
    }

    /// First-parent commits after `from` up to and including `to`, oldest first.
    pub fn first_parent_range(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let out = self.run(&["rev-list", "--first-parent", "--reverse", &format!("{from}..{to}")])?;
        Ok(String::from_utf8(out)?.lines().map(str::to_string).collect())
    }

    /// Read an object's raw bytes.
    pub fn cat(&self, oid: &str) -> Result<Vec<u8>> {
        let mut c = self.cat.lock().unwrap();
        writeln!(c.stdin, "{oid}")?;
        c.stdin.flush()?;
        let mut header = String::new();
        c.stdout.read_line(&mut header)?;
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() != 3 {
            bail!("git cat-file: {}", header.trim());
        }
        let size: usize = parts[2].parse()?;
        let mut buf = vec![0u8; size + 1];
        c.stdout.read_exact(&mut buf)?;
        buf.pop(); // trailing newline
        Ok(buf)
    }

    pub fn ls_tree(&self, commit: &str) -> Result<Vec<TreeItem>> {
        let out = self.run(&["ls-tree", "-r", "-z", "--full-tree", commit])?;
        out.split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|rec| {
                let tab = rec.iter().position(|&b| b == b'\t').context("bad ls-tree record")?;
                let meta = std::str::from_utf8(&rec[..tab])?;
                let path = String::from_utf8(rec[tab + 1..].to_vec()).context("non-UTF-8 path")?;
                let f: Vec<&str> = meta.split(' ').collect();
                Ok(TreeItem { mode: f[0].to_string(), oid: f[2].to_string(), path })
            })
            .collect()
    }

    /// Path-level changes between two commits (no rename detection).
    pub fn diff_tree(&self, parent: &str, commit: &str) -> Result<Vec<DiffItem>> {
        let out = self.run(&["diff-tree", "-r", "-z", "--no-renames", "--no-commit-id", parent, commit])?;
        let mut fields = out.split(|&b| b == 0).filter(|s| !s.is_empty());
        let mut items = vec![];
        while let Some(meta) = fields.next() {
            let meta = std::str::from_utf8(meta)?;
            let path = String::from_utf8(fields.next().context("diff-tree: missing path")?.to_vec())?;
            let f: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
            let status = f[4];
            items.push(DiffItem {
                new_mode: f[1].to_string(),
                new_oid: f[3].to_string(),
                path,
                deleted: status.starts_with('D'),
            });
        }
        Ok(items)
    }

    pub fn commit_info(&self, sha: &str) -> Result<RawCommit> {
        parse_commit(&self.cat(sha)?)
    }
}

fn split_ident(v: &str) -> Result<(String, String)> {
    // "<name> <email> <seconds> <tz>": the date is the last two fields.
    let mut it = v.rsplitn(3, ' ');
    let tz = it.next().context("ident tz")?;
    let ts = it.next().context("ident time")?;
    let who = it.next().context("ident name")?;
    Ok((who.to_string(), format!("{ts} {tz}")))
}

pub fn parse_commit(raw: &[u8]) -> Result<RawCommit> {
    let split = raw.windows(2).position(|w| w == b"\n\n").context("commit without message separator")?;
    let header = &raw[..split + 1];
    let message = raw[split + 2..].to_vec();
    let mut tree = None;
    let mut parents = vec![];
    let mut author = None;
    let mut committer = None;
    let mut extra = vec![];
    let mut seen_committer = false;
    for line in header.split_inclusive(|&b| b == b'\n') {
        if seen_committer {
            extra.extend_from_slice(line);
            continue;
        }
        let s = std::str::from_utf8(line)?.trim_end_matches('\n');
        if let Some(v) = s.strip_prefix("tree ") {
            tree = Some(v.to_string());
        } else if let Some(v) = s.strip_prefix("parent ") {
            parents.push(v.to_string());
        } else if let Some(v) = s.strip_prefix("author ") {
            author = Some(split_ident(v)?);
        } else if let Some(v) = s.strip_prefix("committer ") {
            committer = Some(split_ident(v)?);
            seen_committer = true;
        } else {
            bail!("unexpected commit header before committer: {s}");
        }
    }
    let (author, author_date) = author.context("commit without author")?;
    let (committer, committer_date) = committer.context("commit without committer")?;
    Ok(RawCommit {
        tree: tree.context("commit without tree")?,
        parents,
        author,
        author_date,
        committer,
        committer_date,
        extra_headers: extra,
        message,
    })
}

pub fn object_id(kind: &str, data: &[u8]) -> String {
    let mut h = sha1_smol::Sha1::new();
    h.update(format!("{kind} {}\0", data.len()).as_bytes());
    h.update(data);
    h.digest().to_string()
}

pub fn commit_bytes(c: &RawCommit) -> Vec<u8> {
    let mut out = format!("tree {}\n", c.tree).into_bytes();
    for p in &c.parents {
        out.extend_from_slice(format!("parent {p}\n").as_bytes());
    }
    out.extend_from_slice(format!("author {} {}\n", c.author, c.author_date).as_bytes());
    out.extend_from_slice(format!("committer {} {}\n", c.committer, c.committer_date).as_bytes());
    out.extend_from_slice(&c.extra_headers);
    out.push(b'\n');
    out.extend_from_slice(&c.message);
    out
}

enum Node {
    Blob { mode: &'static str, oid: [u8; 20] },
    Dir(BTreeMap<String, Node>),
}

/// Rebuild the Git tree id for a flat list of `(path, kind, blob oid)`.
pub fn tree_id(files: &[(String, Kind, String)]) -> Result<String> {
    let mut root: BTreeMap<String, Node> = BTreeMap::new();
    for (path, kind, oid) in files {
        let mut parts: Vec<&str> = path.split('/').collect();
        let name = parts.pop().unwrap().to_string();
        let mut dir = &mut root;
        for p in parts {
            let n = dir.entry(p.to_string()).or_insert_with(|| Node::Dir(BTreeMap::new()));
            dir = match n {
                Node::Dir(d) => d,
                Node::Blob { .. } => bail!("{path}: a file is also used as a directory"),
            };
        }
        let raw: [u8; 20] = hex::decode(oid)?.try_into().map_err(|_| anyhow::anyhow!("bad oid {oid}"))?;
        dir.insert(name, Node::Blob { mode: mode_for_kind(*kind), oid: raw });
    }
    fn write(dir: &BTreeMap<String, Node>) -> [u8; 20] {
        // Git sorts tree entries as if directory names ended with '/'.
        let mut items: Vec<(Vec<u8>, &str, [u8; 20], &str)> = dir
            .iter()
            .map(|(name, n)| match n {
                Node::Blob { mode, oid } => (name.as_bytes().to_vec(), *mode, *oid, name.as_str()),
                Node::Dir(d) => {
                    let mut k = name.as_bytes().to_vec();
                    k.push(b'/');
                    (k, "40000", write(d), name.as_str())
                }
            })
            .collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        let mut buf = vec![];
        for (_, mode, oid, name) in items {
            buf.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            buf.extend_from_slice(&oid);
        }
        let id = object_id("tree", &buf);
        hex::decode(id).unwrap().try_into().unwrap()
    }
    Ok(hex::encode(write(&root)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_ids() {
        assert_eq!(object_id("blob", b""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(tree_id(&[]).unwrap(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    }

    #[test]
    fn commit_roundtrip() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nparent aaaa\nauthor A B <a@b> 1 +0100\ncommitter C <c@d> 2 -0500\ngpgsig -----BEGIN-----\n line\n -----END-----\n\nmsg\n\nbody\n";
        let c = parse_commit(raw).unwrap();
        assert_eq!(c.author, "A B <a@b>");
        assert_eq!(c.author_date, "1 +0100");
        assert_eq!(commit_bytes(&c), raw.to_vec());
    }
}
