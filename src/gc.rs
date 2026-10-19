//! Garbage collection: mark everything reachable from refs (and reflog entries), then
//! delete unreferenced immutable objects older than a grace period. The grace period
//! protects objects uploaded by commits that have not reached their CAS yet.
//! Packs are deleted only when no live entry points into them (no repacking: entry
//! locations are part of page hashes).

use std::collections::HashSet;

use anyhow::Result;

use crate::hash::Hash;
use crate::manifest::{Loc, Page};
use crate::repo::Repo;

#[derive(Debug, Default)]
pub struct GcReport {
    pub live_commits: usize,
    pub live_pages: usize,
    pub live_packs: usize,
    pub live_chunks: usize,
    pub deleted: Vec<String>,
    pub bytes_freed: u64,
    pub kept_young: usize,
}

pub async fn gc(repo: &Repo, grace_secs: i64, dry_run: bool) -> Result<GcReport> {
    let mut roots: Vec<Hash> = repo.list_refs().await?.into_iter().map(|(_, h)| h).collect();
    for o in repo.store.list_meta("reflog/").await? {
        let b = repo.store.get(&o.key).await?;
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
            for k in ["old", "new"] {
                if let Some(h) = v.get(k).and_then(|x| x.as_str()).and_then(|s| Hash::from_hex(s).ok()) {
                    roots.push(h);
                }
            }
        }
    }

    let mut commits = HashSet::new();
    let mut tree_roots = vec![];
    for r in roots {
        let mut cur = Some(r);
        while let Some(h) = cur {
            if !commits.insert(h) {
                break;
            }
            let c = repo.commit_obj(&h).await?;
            tree_roots.push(c.root);
            cur = c.parent;
        }
    }

    let (mut pages, mut packs, mut chunks) = (HashSet::new(), HashSet::new(), HashSet::new());
    let mut stack = tree_roots;
    while let Some(h) = stack.pop() {
        if !pages.insert(h) {
            continue;
        }
        match &*repo.page(&h).await? {
            Page::Node { children, .. } => stack.extend(children.iter().map(|c| c.hash)),
            Page::Leaf(entries) => {
                for e in entries {
                    match &e.loc {
                        Loc::Pack { pack, .. } => {
                            packs.insert(pack.hex());
                        }
                        Loc::Chunks(cs) => chunks.extend(cs.iter().map(|c| c.hash.hex())),
                        Loc::Inline(_) => {}
                    }
                }
            }
        }
    }
    let commit_hex: HashSet<String> = commits.iter().map(|h| h.hex()).collect();
    let page_hex: HashSet<String> = pages.iter().map(|h| h.hex()).collect();

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    let mut report = GcReport {
        live_commits: commits.len(),
        live_pages: pages.len(),
        live_packs: packs.len(),
        live_chunks: chunks.len(),
        ..Default::default()
    };
    for (prefix, live) in [("commits/", &commit_hex), ("pages/", &page_hex), ("packs/", &packs), ("chunks/", &chunks)] {
        for o in repo.store.list_meta(prefix).await? {
            let name = o.key[prefix.len()..].trim_end_matches(".pack");
            if live.contains(name) {
                continue;
            }
            if now - o.modified < grace_secs {
                report.kept_young += 1;
                continue;
            }
            if !dry_run {
                repo.store.delete(&o.key).await?;
            }
            report.bytes_freed += o.size;
            report.deleted.push(o.key);
        }
    }
    Ok(report)
}
