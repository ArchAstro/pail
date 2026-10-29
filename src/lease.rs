//! Leases under `locks/`: writers hold shared ones, `gc` an exclusive one, so gc never
//! deletes while a commit is in flight.
//!
//! A writer may reuse an object that is already stored (`put_new` of an existing object
//! writes nothing, so its LastModified stays old). gc's grace period cannot protect such an
//! object: it looks old and unreferenced until the writer's ref swap lands. So a writer
//! holds a shared lease from before its first read to its return, and gc deletes only
//! while it holds the exclusive lease and no writer holds one (`verify/gc.qnt`).
//!
//! Taking a lease is write, then list:
//!
//! ```text
//!   shared:    put locks/shared-<id>    → list: an exclusive lease? delete mine, retry later
//!   exclusive: put locks/exclusive-<id> → list: another exclusive? delete mine, retry later
//!                                               a shared one? keep mine, list again
//! ```
//!
//! Whichever of a writer and gc lists second sees the other's lease, so they never both go
//! ahead. gc keeps its lease while it waits, so new writers stay out and it cannot starve.
//!
//! A lease is refreshed (rewritten) every third of [`LEASE_TTL`]; one not rewritten for a
//! whole TTL belongs to a dead process and is ignored. A holder that has not refreshed for
//! half the TTL ([`Lease::check`]) makes no more ref swaps, so to the others it is dead.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::clock::Clock;

use anyhow::{Result, bail};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::store::Store;

pub const LOCKS: &str = "locks/";

/// How long a lease lives without a refresh.
pub const LEASE_TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseKind {
    Shared,
    Exclusive,
}

impl LeaseKind {
    fn name(self) -> &'static str {
        match self {
            LeaseKind::Shared => "shared",
            LeaseKind::Exclusive => "exclusive",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Body {
    kind: LeaseKind,
    pid: u32,
}

/// When a lease counts as last refreshed, on its clock's [`Clock::instant`] scale.
#[derive(Clone)]
pub struct Probe(Stamp);

impl Probe {
    pub fn refreshed(&self) -> Duration {
        self.0.get()
    }
}

/// A held lease. Release it with [`Lease::release`]; dropped, it is deleted in the
/// background (and otherwise expires).
pub struct Lease {
    store: Store,
    pub key: String,
    pub kind: LeaseKind,
    refreshed: Stamp,
    refresher: Option<tokio::task::JoinHandle<()>>,
}

/// When a lease was last refreshed, by the holder's clock, and that clock.
#[derive(Clone)]
struct Stamp {
    at: Arc<Mutex<Duration>>,
    clock: Arc<dyn Clock>,
}

impl Stamp {
    fn get(&self) -> Duration {
        *self.at.lock().unwrap()
    }

    /// A refresh sent at `sent` has been answered: the lease counts from `sent`, the start
    /// of the put, which is no later than the LastModified it got. Not if the holder is
    /// stale by now: the others may have stopped seeing the lease before the refresh landed,
    /// and a late reply cannot take that back (`verify/clock.qnt`, `refreshAckNext`).
    fn advance(&self, sent: Duration) {
        let mut at = self.at.lock().unwrap();
        if self.clock.instant().saturating_sub(*at) <= LEASE_TTL / 2 {
            *at = sent;
        }
    }

    fn age(&self) -> Duration {
        self.clock
            .instant()
            .saturating_sub(*self.at.lock().unwrap())
    }
}

/// The kind of the lease at `key`, by its name.
fn kind_of(key: &str) -> Option<LeaseKind> {
    let name = key.strip_prefix(LOCKS)?;
    if name.starts_with("exclusive-") {
        Some(LeaseKind::Exclusive)
    } else if name.starts_with("shared-") {
        Some(LeaseKind::Shared)
    } else {
        None
    }
}

/// Live leases other than `mine`: (key, kind).
async fn others(store: &Store, mine: &str) -> Result<Vec<(String, LeaseKind)>> {
    let now = store.clock().unix_secs();
    Ok(store
        .list_meta(LOCKS)
        .await?
        .into_iter()
        .filter(|o| o.key != mine && now - o.modified <= LEASE_TTL.as_secs() as i64)
        .filter_map(|o| kind_of(&o.key).map(|k| (o.key, k)))
        .collect())
}

fn backoff(attempt: u32) -> Duration {
    let cap = 50u64.saturating_mul(1 << attempt.min(6)).min(2000);
    Duration::from_millis(cap / 2 + rand::random::<u64>() % (cap / 2 + 1))
}

impl Lease {
    /// Write a lease; also when the put started, by the store's clock, which is no later
    /// than the LastModified it got: the lease counts from there, however long the put
    /// and what follows it took ([`Lease::held`]).
    async fn write(store: &Store, kind: LeaseKind) -> Result<(String, Duration)> {
        let key = format!(
            "{LOCKS}{}-{:020}-{:08x}",
            kind.name(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            rand::random::<u32>()
        );
        let body = serde_json::to_vec(&Body {
            kind,
            pid: std::process::id(),
        })?;
        let sent = store.clock().instant();
        store.put(&key, Bytes::from(body)).await?;
        Ok((key, sent))
    }

    /// The lease at `key`, written by a put that started at `sent`: its age counts from
    /// there. Counting from now instead (after the put and the listing that followed) made
    /// a holder that was slow to get here believe its lease younger than the store's
    /// timestamp has it, so it could still swap a ref after others had stopped seeing it.
    fn held(store: &Store, key: String, kind: LeaseKind, sent: Duration) -> Lease {
        let clock = store.clock().clone();
        let refreshed = Stamp {
            at: Arc::new(Mutex::new(sent)),
            clock,
        };
        let refresher = tokio::runtime::Handle::try_current().ok().map(|rt| {
            let (store, key, refreshed) = (store.clone(), key.clone(), refreshed.clone());
            rt.spawn(async move {
                loop {
                    store.clock().sleep(LEASE_TTL / 3).await;
                    let body = serde_json::to_vec(&Body {
                        kind,
                        pid: std::process::id(),
                    })
                    .unwrap_or_default();
                    let sent = store.clock().instant();
                    if store.put(&key, Bytes::from(body)).await.is_ok() {
                        refreshed.advance(sent);
                    }
                }
            })
        });
        Lease {
            store: store.clone(),
            key,
            kind,
            refreshed,
            refresher,
        }
    }

    /// A shared lease, waiting up to `wait` while gc holds the exclusive one.
    pub async fn shared(store: &Store, wait: Duration) -> Result<Lease> {
        let started = Instant::now();
        for attempt in 0.. {
            let (key, sent) = Lease::write(store, LeaseKind::Shared).await?;
            let held_by_gc = others(store, &key)
                .await?
                .into_iter()
                .find(|(_, k)| *k == LeaseKind::Exclusive);
            let Some((gc, _)) = held_by_gc else {
                return Ok(Lease::held(store, key, LeaseKind::Shared, sent));
            };
            store.delete(&key).await?;
            if started.elapsed() >= wait {
                bail!("gc is running ({gc}); try again when it finishes");
            }
            tokio::time::sleep(backoff(attempt)).await;
        }
        unreachable!()
    }

    /// The exclusive lease, waiting up to `wait` for writers' leases to go.
    pub async fn exclusive(store: &Store, wait: Duration) -> Result<Lease> {
        let started = Instant::now();
        let mut attempt = 0;
        'write: loop {
            let (key, sent) = Lease::write(store, LeaseKind::Exclusive).await?;
            loop {
                let others = others(store, &key).await?;
                let blocked = if let Some((gc, _)) =
                    others.iter().find(|(_, k)| *k == LeaseKind::Exclusive)
                {
                    // Another collector: both back off, so neither waits on the other.
                    store.delete(&key).await?;
                    if started.elapsed() >= wait {
                        bail!("another gc is running ({gc})");
                    }
                    tokio::time::sleep(backoff(attempt)).await;
                    attempt += 1;
                    continue 'write;
                } else {
                    others.len()
                };
                if blocked == 0 {
                    return Ok(Lease::held(store, key, LeaseKind::Exclusive, sent));
                }
                if started.elapsed() >= wait {
                    store.delete(&key).await?;
                    bail!("{blocked} writer(s) still hold leases; try again later");
                }
                tokio::time::sleep(backoff(attempt)).await;
                attempt += 1;
            }
        }
    }

    /// Whether this lease is still certainly held: refreshed within half its lifetime. Call
    /// before every ref swap.
    pub fn check(&self) -> Result<()> {
        let age = self.refreshed.age();
        if age > LEASE_TTL / 2 {
            bail!(
                "lease {} not refreshed for {}s; refusing to move a ref",
                self.key,
                age.as_secs()
            );
        }
        Ok(())
    }

    /// A read-only view of when the lease counts as last refreshed (the refinement tests
    /// compare it with the model's).
    pub fn probe(&self) -> Probe {
        Probe(self.refreshed.clone())
    }

    pub async fn release(mut self) -> Result<()> {
        if let Some(r) = self.refresher.take() {
            r.abort();
        }
        let key = std::mem::take(&mut self.key);
        self.store.delete(&key).await
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(r) = self.refresher.take() {
            r.abort();
        }
        if self.key.is_empty() {
            return;
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let (store, key) = (self.store.clone(), std::mem::take(&mut self.key));
            rt.spawn(async move {
                let _ = store.delete(&key).await;
            });
        }
    }
}

// ---------- a client's shared lease, shared by its operations ----------

/// One client's shared lease, held while any of its operations runs.
#[derive(Default)]
pub struct WriteLeases {
    held: tokio::sync::Mutex<(Option<Lease>, usize)>,
    /// The held lease's refresh time, for [`WriteLeases::check`] without awaiting.
    refreshed: Mutex<Option<Stamp>>,
    /// The store refused this client a lease (an enforcement point that predates
    /// `locks/`): it writes unleased, protected by gc's grace period alone.
    refused: std::sync::atomic::AtomicBool,
}

fn permission_denied(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<object_store::Error>(),
            Some(object_store::Error::PermissionDenied { .. })
        )
    })
}

impl WriteLeases {
    /// Join (or take) the shared lease.
    pub async fn enter(&self, store: &Store, wait: Duration) -> Result<()> {
        use std::sync::atomic::Ordering;
        let mut held = self.held.lock().await;
        if held.0.is_none() && !self.refused.load(Ordering::Relaxed) {
            match Lease::shared(store, wait).await {
                Ok(lease) => {
                    *self.refreshed.lock().unwrap() = Some(lease.refreshed.clone());
                    held.0 = Some(lease);
                }
                Err(e) if permission_denied(&e) => {
                    eprintln!(
                        "warning: this store refuses write leases ({e:#}); writing without one, \
                         so a concurrent gc relies on its grace period alone"
                    );
                    self.refused.store(true, Ordering::Relaxed);
                }
                Err(e) => return Err(e),
            }
        }
        held.1 += 1;
        Ok(())
    }

    /// Leave; the last one out releases the lease.
    pub async fn leave(&self) -> Result<()> {
        let mut held = self.held.lock().await;
        held.1 = held.1.saturating_sub(1);
        if held.1 > 0 {
            return Ok(());
        }
        *self.refreshed.lock().unwrap() = None;
        match held.0.take() {
            Some(lease) => lease.release().await,
            None => Ok(()),
        }
    }

    /// A view of the shared lease's last refresh, if one is held: see [`Lease::probe`].
    pub fn probe(&self) -> Option<Probe> {
        self.refreshed.lock().unwrap().clone().map(Probe)
    }

    /// Fails unless a fresh shared lease is held: see [`Lease::check`].
    pub fn check(&self) -> Result<()> {
        if self.refused.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        let Some(refreshed) = self.refreshed.lock().unwrap().clone() else {
            bail!("moving a ref without holding a write lease");
        };
        let age = refreshed.age();
        if age > LEASE_TTL / 2 {
            bail!(
                "write lease not refreshed for {}s; refusing to move a ref",
                age.as_secs()
            );
        }
        Ok(())
    }
}

/// A caller's hold on its client's write lease ([`crate::repo::Repo::writing`]).
pub struct Writing {
    leases: Option<Arc<WriteLeases>>,
}

impl Writing {
    pub(crate) fn new(leases: Arc<WriteLeases>) -> Writing {
        Writing {
            leases: Some(leases),
        }
    }

    /// Let go; the last holder releases the lease.
    pub async fn done(mut self) -> Result<()> {
        match self.leases.take() {
            Some(l) => l.leave().await,
            None => Ok(()),
        }
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        if let (Some(l), Ok(rt)) = (self.leases.take(), tokio::runtime::Handle::try_current()) {
            rt.spawn(async move {
                let _ = l.leave().await;
            });
        }
    }
}
