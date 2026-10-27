//! Leases under `locks/` (`src/lease.rs`, modelled in `verify/gc.qnt`).

use std::time::Duration;

use pail::lease::{LOCKS, Lease};
use pail::store::Store;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file://{}", dir.path().join("bucket").display());
    (dir, Store::open(&url).unwrap())
}

#[tokio::test]
async fn writers_share_and_gc_waits_for_them() {
    let (_d, s) = store();
    let a = Lease::shared(&s, Duration::ZERO).await.unwrap();
    let b = Lease::shared(&s, Duration::ZERO).await.unwrap();
    let err = Lease::exclusive(&s, Duration::from_millis(200))
        .await
        .err()
        .expect("gc may not run under writers");
    assert!(err.to_string().contains("2 writer(s)"), "{err}");
    a.release().await.unwrap();
    b.release().await.unwrap();
    let gc = Lease::exclusive(&s, Duration::ZERO).await.unwrap();
    gc.release().await.unwrap();
    assert!(
        s.list(LOCKS).await.unwrap().is_empty(),
        "every lease released"
    );
}

#[tokio::test]
async fn writers_wait_out_gc() {
    let (_d, s) = store();
    let gc = Lease::exclusive(&s, Duration::ZERO).await.unwrap();
    let err = Lease::shared(&s, Duration::from_millis(200))
        .await
        .err()
        .expect("no writes under gc");
    assert!(err.to_string().contains("gc is running"), "{err}");
    assert_eq!(
        s.list(LOCKS).await.unwrap().len(),
        1,
        "the writer took its lease back"
    );
    let s2 = s.clone();
    let writer = tokio::spawn(async move { Lease::shared(&s2, Duration::from_secs(10)).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!writer.is_finished());
    gc.release().await.unwrap();
    writer.await.unwrap().unwrap().release().await.unwrap();
}

#[tokio::test]
async fn gc_keeps_writers_out_while_it_drains() {
    let (_d, s) = store();
    let early = Lease::shared(&s, Duration::ZERO).await.unwrap();
    let s2 = s.clone();
    let gc = tokio::spawn(async move { Lease::exclusive(&s2, Duration::from_secs(10)).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    // gc is waiting for `early`, holding its lease: a newcomer may not slip in.
    assert!(Lease::shared(&s, Duration::ZERO).await.is_err());
    early.release().await.unwrap();
    gc.await.unwrap().unwrap().release().await.unwrap();
}

#[tokio::test]
async fn a_dead_writers_lease_is_ignored_once_expired() {
    let (d, s) = store();
    let dead = Lease::shared(&s, Duration::ZERO).await.unwrap();
    let path = d.path().join("bucket").join(&dead.key);
    std::mem::forget(dead);
    let old = std::time::SystemTime::now() - pail::lease::LEASE_TTL - Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old)
        .unwrap();
    Lease::exclusive(&s, Duration::ZERO)
        .await
        .unwrap()
        .release()
        .await
        .unwrap();
}
