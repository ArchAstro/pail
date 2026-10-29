//! Garbage collection: take the exclusive lease (waiting for writers to finish; see
//! `lease`), mark everything reachable from refs (and reflog entries), then delete
//! unreferenced immutable objects older than a grace period. The lease keeps commits from
//! running meanwhile: one can reuse an old stored object without rewriting it, so no age
//! tells gc that an in-flight commit needs it (`verify/gc.qnt`). The grace period spares
//! objects written outside any lease.
//! Packs are deleted only when no live entry points into them (no repacking: entry
//! locations are part of page hashes).

use std::collections::HashSet;

use anyhow::Result;

use crate::hash::Hash;
use crate::lease::Lease;
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

pub struct GcOptions {
    pub grace_secs: i64,
    /// Report what would go, deleting nothing (and taking no lease).
    pub dry_run: bool,
    /// How long to wait for writers to finish.
    pub wait: std::time::Duration,
    /// The time, in unix seconds, that object ages are measured from (default: now).
    pub now: Option<i64>,
}

pub async fn gc(repo: &Repo, grace_secs: i64, dry_run: bool) -> Result<GcReport> {
    gc_with(
        repo,
        &GcOptions {
            grace_secs,
            dry_run,
            wait: std::time::Duration::from_secs(600),
            now: None,
        },
    )
    .await
}

pub async fn gc_with(repo: &Repo, opts: &GcOptions) -> Result<GcReport> {
    if opts.dry_run {
        return sweep(repo, opts, None).await;
    }
    let lease = Lease::exclusive(&repo.store, opts.wait).await?;
    let report = gc_under(repo, opts, &lease).await;
    let released = lease.release().await;
    let report = report?;
    released?;
    Ok(report)
}

/// [`gc_with`] under a lease the caller took (`Lease::exclusive`) and will release.
pub async fn gc_under(repo: &Repo, opts: &GcOptions, lease: &Lease) -> Result<GcReport> {
    sweep(repo, opts, Some(lease)).await
}

/// Mark, then delete. Under a `lease`, each delete first checks it ([`Lease::check`]): a gc
/// that stalled (a long pause, a slow listing) may have lost its lease without knowing it,
/// if the others stopped seeing it, took shared ones and reused objects it had marked
/// unreachable. Its marks are stale then, so it deletes nothing more
/// (`verify/gc.qnt`, `staleGcRefusesToDeleteRun`).
async fn sweep(repo: &Repo, opts: &GcOptions, lease: Option<&Lease>) -> Result<GcReport> {
    let (grace_secs, dry_run) = (opts.grace_secs, opts.dry_run);
    let mut roots: Vec<Hash> = repo
        .list_refs()
        .await?
        .into_iter()
        .map(|(_, h)| h)
        .collect();
    for o in repo.store.list_meta("reflog/").await? {
        let b = repo.store.get(&o.key).await?;
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
            for k in ["old", "new"] {
                if let Some(h) = v
                    .get(k)
                    .and_then(|x| x.as_str())
                    .and_then(|s| Hash::from_hex(s).ok())
                {
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
                        Loc::Inline(_) | Loc::Unresolved => {}
                    }
                }
            }
        }
    }
    let commit_hex: HashSet<String> = commits.iter().map(|h| h.hex()).collect();
    let page_hex: HashSet<String> = pages.iter().map(|h| h.hex()).collect();

    let now = match opts.now {
        Some(now) => now,
        None => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64,
    };
    let mut report = GcReport {
        live_commits: commits.len(),
        live_pages: pages.len(),
        live_packs: packs.len(),
        live_chunks: chunks.len(),
        ..Default::default()
    };
    for (prefix, live) in [
        ("commits/", &commit_hex),
        ("pages/", &page_hex),
        ("packs/", &packs),
        ("chunks/", &chunks),
    ] {
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
                if let Some(lease) = lease {
                    lease.check()?;
                }
                repo.store.delete(&o.key).await?;
            }
            report.bytes_freed += o.size;
            report.deleted.push(o.key);
        }
    }
    Ok(report)
}
