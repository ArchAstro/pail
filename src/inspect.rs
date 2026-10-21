//! `pail inspect`: a local web view of a repository's physical data model: commits, manifest
//! pages, packs and chunks, and how a file in the filesystem view maps onto them.
//!
//! The page (`inspect.html`) talks to a small JSON API served from here; every answer is
//! read from the store (through the local repository's caches when there is one).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::hash::Hash;
use crate::manifest::{Entry, Kind, Loc, Page};
use crate::pack::{chunk_key, pack_key};
use crate::rev::Ctx;
use crate::snapshot::{NodeKind, Snapshot};

const PAGE: &str = include_str!("inspect.html");

fn kind(k: Kind) -> &'static str {
    match k {
        Kind::File => "file",
        Kind::Exec => "exec",
        Kind::Symlink => "symlink",
    }
}

fn loc_json(l: &Loc) -> Value {
    match l {
        Loc::Inline(b) => json!({"type": "inline", "bytes": b.len()}),
        Loc::Pack { pack, off, len } => {
            json!({"type": "pack", "pack": pack.hex(), "off": off, "len": len})
        }
        Loc::Chunks(cs) => {
            json!({"type": "chunks", "chunks": cs.iter().map(|c| json!({"hash": c.hash.hex(), "len": c.len})).collect::<Vec<_>>()})
        }
        Loc::Unresolved => json!({"type": "unresolved"}),
    }
}

fn entry_json(e: &Entry) -> Value {
    json!({"path": e.path, "kind": kind(e.kind), "size": e.size, "hash": e.hash.hex(), "loc": loc_json(&e.loc)})
}

struct Inspector {
    ctx: Ctx,
    /// Store prefix totals, computed once (listing a big store takes a while).
    prefixes: tokio::sync::OnceCell<Value>,
}

impl Inspector {
    async fn commit_id(&self, rev: Option<&str>) -> Result<Hash> {
        match rev.filter(|r| !r.is_empty()) {
            Some(r) => self.ctx.resolve(r).await,
            None => self.ctx.head_commit().await,
        }
    }

    async fn stored_size(&self, key: &str) -> Option<u64> {
        self.ctx.repo.store.size(key).await.ok().flatten()
    }

    async fn overview(&self) -> Result<Value> {
        let default = self.ctx.default_branch().await?.to_string();
        let mut refs = vec![];
        for (name, h) in self.ctx.refs().await? {
            let c = self.ctx.commit(&h).await?;
            refs.push(json!({"name": name, "commit": h.hex(), "subject": c.subject(), "date": c.committer_date, "files": c.files, "bytes": c.bytes}));
        }
        let local = self
            .ctx
            .local
            .as_ref()
            .map(|l| json!({"dir": l.dir.display().to_string(), "bare": l.config.bare}));
        Ok(json!({
            "store": self.ctx.repo.store.url,
            "default_branch": default,
            "worktree": self.ctx.worktree.as_ref().map(|p| p.display().to_string()),
            "refs": refs,
            "local": local,
        }))
    }

    /// What the local repository holds, by part (walks its files: only on request).
    async fn local(&self) -> Result<Value> {
        let l = self
            .ctx
            .local
            .as_ref()
            .context("reading the store directly: nothing is kept locally")?;
        let size = |d: &str| -> (u64, u64) {
            let mut n = 0;
            let mut b = 0;
            for e in ignore::WalkBuilder::new(l.dir.join(d))
                .standard_filters(false)
                .build()
                .flatten()
            {
                if let Ok(m) = e.metadata() {
                    if m.is_file() {
                        n += 1;
                        b += m.len();
                    }
                }
            }
            (n, b)
        };
        let parts: Vec<Value> = [
            ("blobs", "file contents"),
            ("objects", "commits and pages"),
            ("graph", "commit graph"),
            ("index", "Git ids"),
        ]
        .iter()
        .map(|(d, what)| {
            let (n, b) = size(d);
            json!({"name": d, "what": what, "files": n, "bytes": b})
        })
        .collect();
        Ok(json!({"dir": l.dir.display().to_string(), "bare": l.config.bare, "parts": parts}))
    }

    async fn store_prefixes(&self) -> Result<Value> {
        self.prefixes
            .get_or_try_init(|| async {
                let mut out = serde_json::Map::new();
                for p in ["commits/", "pages/", "packs/", "chunks/", "refs/"] {
                    let objs = self.ctx.repo.store.list_meta(p).await?;
                    out.insert(p.trim_end_matches('/').to_string(), json!({"objects": objs.len(), "bytes": objs.iter().map(|o| o.size).sum::<u64>()}));
                }
                Ok::<_, anyhow::Error>(Value::Object(out))
            })
            .await
            .cloned()
    }

    async fn log(&self, rev: Option<&str>, n: usize, skip: usize) -> Result<Value> {
        let mut cur = Some(self.commit_id(rev).await?);
        let mut out = vec![];
        let mut i = 0;
        while let Some(h) = cur {
            let c = self.ctx.commit(&h).await?;
            if i >= skip {
                let changed = self.ctx.repo.changed_paths(&h).map(|p| p.len());
                out.push(json!({
                    "id": h.hex(), "subject": c.subject(), "author": c.author, "date": c.committer_date,
                    "files": c.files, "bytes": c.bytes, "generation": c.generation, "changed": changed,
                    "git": c.meta.get("git.commit"),
                }));
            }
            i += 1;
            if out.len() >= n {
                break;
            }
            cur = c.parent;
        }
        Ok(json!({"commits": out, "more": cur.is_some()}))
    }

    async fn commit(&self, rev: &str) -> Result<Value> {
        let h = self.commit_id(Some(rev)).await?;
        let c = self.ctx.repo.commit_obj(&h).await?;
        let (_, raw) = c.encode();
        let changes: Vec<Value> = c
            .change_list()
            .unwrap_or_default()
            .iter()
            .map(|ch| json!({"path": ch.path, "old": ch.old.as_ref().map(entry_json), "new": ch.new.as_ref().map(entry_json)}))
            .collect();
        let root = self.ctx.repo.page(&c.root).await?;
        Ok(json!({
            "id": h.hex(), "parent": c.parent.map(|p| p.hex()), "root": c.root.hex(), "root_level": root.level(),
            "generation": c.generation, "files": c.files, "bytes": c.bytes,
            "author": c.author, "author_date": c.author_date, "committer": c.committer, "committer_date": c.committer_date,
            "message": c.message, "meta": c.meta, "changes": changes, "has_change_list": c.changes.is_some(),
            "object_bytes": raw.len(), "key": crate::objects::commit_key(&h),
        }))
    }

    /// An object's stored bytes, as text, for the small textual ones: a branch ref, the
    /// store's HEAD, a commit (its JSON, shortened when it lists many changes).
    async fn raw(&self, key: &str) -> Result<Value> {
        let ok = key == "HEAD" || key.starts_with("refs/") || key.starts_with("commits/");
        anyhow::ensure!(ok, "only refs, HEAD and commits are shown as text");
        let data = self.ctx.repo.store.get(key).await?;
        let text = String::from_utf8_lossy(&data);
        let shown: String = text.chars().take(5000).collect();
        Ok(
            json!({"key": key, "bytes": data.len(), "text": shown, "truncated": text.len() > shown.len()}),
        )
    }

    async fn page(&self, h: &str) -> Result<Value> {
        let h = Hash::from_hex(h)?;
        let p = self.ctx.repo.page(&h).await?;
        let stored = self.stored_size(&crate::objects::page_key(&h)).await;
        Ok(match &*p {
            Page::Node { level, children } => json!({
                "hash": h.hex(), "level": level, "stored": stored, "kind": "node",
                "children": children.iter().map(|c| json!({"key": c.key, "hash": c.hash.hex(), "files": c.files, "bytes": c.bytes})).collect::<Vec<_>>(),
            }),
            Page::Leaf(es) => json!({
                "hash": h.hex(), "level": 0, "stored": stored, "kind": "leaf",
                "entries": es.iter().map(entry_json).collect::<Vec<_>>(),
            }),
        })
    }

    async fn tree(&self, rev: Option<&str>, path: &str) -> Result<Value> {
        let h = self.commit_id(rev).await?;
        let snap = Snapshot::open(&self.ctx.repo, h).await?;
        if !path.is_empty() && snap.file(path).await?.is_some() {
            return Ok(json!({"commit": h.hex(), "path": path, "is_file": true}));
        }
        let items: Vec<Value> = snap
            .list_dir(path)
            .await?
            .iter()
            .map(|n| {
                let k = match n.kind {
                    NodeKind::Dir => "dir",
                    NodeKind::File => "file",
                    NodeKind::Exec => "exec",
                    NodeKind::Symlink => "symlink",
                };
                json!({"name": n.name(), "path": n.path, "kind": k, "size": n.size, "files": n.files,
                       "loc": n.entry.as_ref().map(|e| loc_json(&e.loc))})
            })
            .collect();
        Ok(json!({"commit": h.hex(), "path": path, "items": items}))
    }

    /// The physical objects behind a folder of the logical tree: every manifest page whose path
    /// range overlaps the folder (root to leaves), and the packs and chunks its files use.
    async fn cover(&self, rev: Option<&str>, dir: &str) -> Result<Value> {
        let h = self.commit_id(rev).await?;
        let c = self.ctx.commit(&h).await?;
        let dir = dir.trim_matches('/');
        let (lo, hi) = if dir.is_empty() {
            (String::new(), "\u{10FFFF}".to_string())
        } else {
            (format!("{dir}/"), format!("{dir}0"))
        };
        let mut pages = vec![];
        let mut leaves = 0;
        let mut storage: BTreeMap<String, (String, u64, u64, std::collections::BTreeSet<String>)> =
            BTreeMap::new();
        let mut files = 0u64;
        // (page, parent, range low (exclusive), range high (inclusive))
        let mut stack: Vec<(Hash, Option<Hash>, Option<String>, Option<String>)> =
            vec![(c.root, None, None, None)];
        while let Some((ph, parent, rlo, rhi)) = stack.pop() {
            let p = self.ctx.repo.page(&ph).await?;
            let level = p.level();
            let (count, n_files) = match &*p {
                Page::Node { children, .. } => {
                    let mut prev: Option<String> = rlo.clone();
                    let mut picked = vec![];
                    for ch in children {
                        let overlaps = ch.key.as_str() >= lo.as_str()
                            && prev.as_deref().is_none_or(|pv| pv < hi.as_str());
                        if overlaps {
                            picked.push((ch.hash, Some(ph), prev.clone(), Some(ch.key.clone())));
                        }
                        prev = Some(ch.key.clone());
                    }
                    let n = picked.len();
                    for item in picked.into_iter().rev() {
                        stack.push(item);
                    }
                    (n, children.iter().map(|c| c.files).sum::<u64>())
                }
                Page::Leaf(es) => {
                    leaves += 1;
                    let inside: Vec<&Entry> = es
                        .iter()
                        .filter(|e| e.path.as_str() >= lo.as_str() && e.path.as_str() < hi.as_str())
                        .collect();
                    files += inside.len() as u64;
                    for e in &inside {
                        match &e.loc {
                            Loc::Pack { pack, len, .. } => {
                                let s = storage.entry(pack.hex()).or_insert((
                                    "pack".into(),
                                    0,
                                    0,
                                    Default::default(),
                                ));
                                s.1 += 1;
                                s.2 += len;
                                s.3.insert(ph.hex());
                            }
                            Loc::Chunks(cs) => {
                                for ch in cs {
                                    let s = storage.entry(ch.hash.hex()).or_insert((
                                        "chunk".into(),
                                        0,
                                        0,
                                        Default::default(),
                                    ));
                                    s.1 += 1;
                                    s.2 += ch.len;
                                    s.3.insert(ph.hex());
                                }
                            }
                            _ => {
                                let s = storage.entry("inline".into()).or_insert((
                                    "inline".into(),
                                    0,
                                    0,
                                    Default::default(),
                                ));
                                s.1 += 1;
                                s.2 += e.size;
                                s.3.insert(ph.hex());
                            }
                        }
                    }
                    (inside.len(), es.len() as u64)
                }
            };
            if pages.len() < 400 {
                pages.push(json!({"hash": ph.hex(), "parent": parent.map(|p| p.hex()), "level": level, "range": [rlo, rhi],
                                  "overlap": count, "files": n_files}));
            }
        }
        let mut storage: Vec<Value> = storage.into_iter().map(|(h, (kind, n, b, ls))| json!({"hash": h, "kind": kind, "files": n, "bytes": b, "leaves": ls})).collect();
        storage.sort_by(|a, b| b["bytes"].as_u64().cmp(&a["bytes"].as_u64()));
        Ok(
            json!({"commit": h.hex(), "root": c.root.hex(), "dir": dir, "files": files, "leaves": leaves, "pages": pages, "storage": storage}),
        )
    }

    /// A file and the physical path to its bytes: the pages from the root to its leaf, its
    /// entry, and where the bytes are stored.
    async fn file(&self, rev: Option<&str>, path: &str) -> Result<Value> {
        let h = self.commit_id(rev).await?;
        let c = self.ctx.commit(&h).await?;
        let mut spine = vec![];
        let mut cur = c.root;
        let entry = loop {
            let p = self.ctx.repo.page(&cur).await?;
            match &*p {
                Page::Node { level, children } => {
                    let i = children
                        .partition_point(|ch| ch.key.as_str() < path)
                        .min(children.len() - 1);
                    let lo = if i == 0 {
                        None
                    } else {
                        Some(children[i - 1].key.clone())
                    };
                    spine.push(json!({"hash": cur.hex(), "level": level, "children": children.len(), "index": i,
                        "range": [lo, children[i].key], "files": children.iter().map(|c| c.files).sum::<u64>()}));
                    cur = children[i].hash;
                }
                Page::Leaf(es) => {
                    let i = es
                        .binary_search_by(|e| e.path.as_str().cmp(path))
                        .ok()
                        .with_context(|| format!("{path} is not in this commit"))?;
                    spine.push(json!({"hash": cur.hex(), "level": 0, "children": es.len(), "index": i,
                        "range": [es.first().map(|e| e.path.clone()), es.last().map(|e| e.path.clone())]}));
                    break es[i].clone();
                }
            }
        };
        let data = self
            .ctx
            .repo
            .read_files(std::slice::from_ref(&entry))
            .await?
            .remove(0);
        let text = !data.iter().take(8000).any(|&b| b == 0);
        let preview = if text {
            String::from_utf8_lossy(&data[..data.len().min(6000)]).into_owned()
        } else {
            String::new()
        };
        let mut neighbors = Value::Null;
        if let Loc::Pack { pack, .. } = &entry.loc {
            neighbors = self.pack_map(pack, Some(h)).await.unwrap_or(Value::Null);
        }
        let git = crate::gitobj::blob_id_of(&self.ctx.repo, &entry.hash, &data);
        let cached = self.ctx.repo.blobs.as_ref().map(|b| b.has(&entry.hash));
        Ok(json!({
            "commit": h.hex(), "root": c.root.hex(), "entry": entry_json(&entry), "spine": spine,
            "text": text, "preview": preview, "truncated": data.len() > 6000, "git_blob": git,
            "pack": neighbors, "cached": cached,
        }))
    }

    /// A pack's blobs (from its footer index) with the paths of `commit` that use each.
    async fn pack_map(&self, pack: &Hash, commit: Option<Hash>) -> Result<Value> {
        let key = pack_key(pack);
        let size = self.stored_size(&key).await.context("pack not found")?;
        let tail = self
            .ctx
            .repo
            .store
            .get_ranges(&key, &[size - 12..size])
            .await?
            .remove(0);
        anyhow::ensure!(&tail[4..] == b"PAILPK1\0", "not a pail pack");
        let footer_len = u32::from_le_bytes(tail[..4].try_into().unwrap()) as u64;
        let footer = self
            .ctx
            .repo
            .store
            .get_ranges(&key, &[size - 12 - footer_len..size - 12])
            .await?
            .remove(0);
        let mut r = crate::enc::Reader::new(&footer);
        let n = r.varint()?;
        let mut blobs = vec![];
        for _ in 0..n {
            let h = r.hash()?;
            let (off, len, raw) = (r.varint()?, r.varint()?, r.varint()?);
            blobs.push((h, off, len, raw));
        }
        // Which paths of the commit point into this pack.
        let mut paths: HashMap<u64, Vec<String>> = HashMap::new();
        if let Some(c) = commit {
            let root = self.ctx.commit(&c).await?.root;
            for e in self.ctx.repo.entries(root).await? {
                if let Loc::Pack { pack: p, off, .. } = &e.loc {
                    if p == pack {
                        paths.entry(*off).or_default().push(e.path);
                    }
                }
            }
        }
        let data_len = size - 12 - footer_len;
        Ok(json!({
            "hash": pack.hex(), "size": size, "data_bytes": data_len, "footer_bytes": footer_len + 12,
            "commit": commit.map(|c| c.hex()),
            "blobs": blobs.iter().map(|(h, off, len, raw)| json!({
                "hash": h.hex(), "off": off, "len": len, "raw": raw,
                "codec": if *len == raw + 1 { "raw" } else { "zstd" },
                "paths": paths.get(off).cloned().unwrap_or_default(),
            })).collect::<Vec<_>>(),
        }))
    }

    async fn chunk(&self, h: &str, rev: Option<&str>) -> Result<Value> {
        let h = Hash::from_hex(h)?;
        let size = self.stored_size(&chunk_key(&h)).await;
        let c = self.commit_id(rev).await?;
        let root = self.ctx.commit(&c).await?.root;
        let mut users = vec![];
        for e in self.ctx.repo.entries(root).await? {
            if let Loc::Chunks(cs) = &e.loc {
                let mut off = 0;
                for (i, ch) in cs.iter().enumerate() {
                    if ch.hash == h {
                        users.push(json!({"path": e.path, "index": i, "of": cs.len(), "offset": off, "len": ch.len, "file_size": e.size}));
                    }
                    off += ch.len;
                }
            }
        }
        Ok(json!({"hash": h.hex(), "stored": size, "commit": c.hex(), "users": users}))
    }

    async fn route(&self, path: &str, q: &BTreeMap<String, String>) -> Result<Value> {
        let rev = q.get("rev").map(String::as_str);
        let parts: Vec<&str> = path.trim_start_matches("/api/").split('/').collect();
        match parts.as_slice() {
            ["overview"] => self.overview().await,
            ["store"] => self.store_prefixes().await,
            ["local"] => self.local().await,
            ["raw"] => self.raw(q.get("key").context("key is required")?).await,
            ["log"] => {
                self.log(
                    rev,
                    q.get("n").and_then(|n| n.parse().ok()).unwrap_or(50),
                    q.get("skip").and_then(|n| n.parse().ok()).unwrap_or(0),
                )
                .await
            }
            ["commit", r] => self.commit(r).await,
            ["page", h] => self.page(h).await,
            ["cover"] => {
                self.cover(rev, q.get("path").map(String::as_str).unwrap_or(""))
                    .await
            }
            ["tree"] => {
                self.tree(rev, q.get("path").map(String::as_str).unwrap_or(""))
                    .await
            }
            ["file"] => {
                self.file(rev, q.get("path").context("path is required")?)
                    .await
            }
            ["pack", h] => {
                let c = self.commit_id(rev).await?;
                self.pack_map(&Hash::from_hex(h)?, Some(c)).await
            }
            ["chunk", h] => self.chunk(h, rev).await,
            _ => bail!("no such endpoint: {path}"),
        }
    }
}

/// Decode a URL query component (`+` and `%XX`).
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(v) => {
                    out.push(v);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Serve the inspector until interrupted.
pub async fn serve(ctx: Ctx, addr: &str, open: bool) -> Result<()> {
    let server =
        tiny_http::Server::http(addr).map_err(|e| anyhow::anyhow!("listening on {addr}: {e}"))?;
    let url = format!("http://{addr}/");
    println!(
        "inspecting {} at {url}  (ctrl-c to stop)",
        ctx.repo.store.url
    );
    if open {
        let _ = std::process::Command::new(if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        })
        .arg(&url)
        .spawn();
    }
    let ins = Arc::new(Inspector {
        ctx,
        prefixes: tokio::sync::OnceCell::new(),
    });
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        for req in server.incoming_requests() {
            let ins = ins.clone();
            let handle = handle.clone();
            std::thread::spawn(move || {
                let url = req.url().to_string();
                let (path, query) = url.split_once('?').unwrap_or((&url, ""));
                let q: BTreeMap<String, String> = query
                    .split('&')
                    .filter(|p| !p.is_empty())
                    .map(|p| {
                        let (k, v) = p.split_once('=').unwrap_or((p, ""));
                        (decode(k), decode(v))
                    })
                    .collect();
                let header = |v: &str| {
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], v.as_bytes()).unwrap()
                };
                let resp = if path.starts_with("/api/") {
                    match handle.block_on(ins.route(path, &q)) {
                        Ok(v) => tiny_http::Response::from_string(v.to_string())
                            .with_header(header("application/json")),
                        Err(e) => tiny_http::Response::from_string(
                            json!({"error": format!("{e:#}")}).to_string(),
                        )
                        .with_status_code(400)
                        .with_header(header("application/json")),
                    }
                } else {
                    tiny_http::Response::from_string(PAGE)
                        .with_header(header("text/html; charset=utf-8"))
                };
                let _ = req.respond(resp);
            });
        }
    })
    .await?;
    Ok(())
}
