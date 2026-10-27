//! Behaviour of `pail::gcs_emulator` beyond what the store refinement exercises:
//! paging, ranges, generations across delete, preconditions.

use bytes::Bytes;
use pail::gcs_emulator::GcsEmulator;
use pail::store::{Cas, Store};

fn open(emu: &GcsEmulator, prefix: &str) -> Store {
    let url = format!("gs://b/{prefix}");
    Store::open_with_gcs_base(&url, None, Some(emu.url())).unwrap()
}

#[tokio::test]
async fn lists_across_pages() {
    let emu = GcsEmulator::start("127.0.0.1:0").unwrap();
    let s = open(&emu, "p");
    for i in 0..1105 {
        s.put(&format!("k/{i:05}"), Bytes::from(vec![0u8; i % 7]))
            .await
            .unwrap();
    }
    s.put("other/x", Bytes::new()).await.unwrap();
    let got = s.list("k/").await.unwrap();
    assert_eq!(got.len(), 1105);
    assert!(got.iter().all(|(k, _)| k.starts_with("k/")));
    let mut sorted: Vec<_> = got.iter().map(|(k, _)| k.clone()).collect();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 1105);
}

#[tokio::test]
async fn ranges_and_size() {
    let emu = GcsEmulator::start("127.0.0.1:0").unwrap();
    let s = open(&emu, "p");
    s.put("a", Bytes::from("0123456789")).await.unwrap();
    assert_eq!(s.size("a").await.unwrap(), Some(10));
    assert_eq!(s.size("nope").await.unwrap(), None);
    let (b, total) = s.read_range("a", 2..5).await.unwrap().unwrap();
    assert_eq!((&b[..], total), (&b"234"[..], 10));
    assert!(s.read_range("nope", 0..1).await.unwrap().is_none());
}

#[tokio::test]
async fn generations_are_fresh_and_preconditions_hold() {
    let emu = GcsEmulator::start("127.0.0.1:0").unwrap();
    let s = open(&emu, "p");
    let Cas::Ok(v1) = s.cas("r", Bytes::from("x"), None).await.unwrap() else {
        panic!("create")
    };
    assert!(matches!(
        s.cas("r", Bytes::from("x"), None).await.unwrap(),
        Cas::Conflict
    ));
    // Identical bytes, new generation; the old version is now stale.
    let Cas::Ok(v2) = s.cas("r", Bytes::from("x"), Some(&v1)).await.unwrap() else {
        panic!("swap")
    };
    assert_ne!(v1, v2);
    assert!(matches!(
        s.cas("r", Bytes::from("y"), Some(&v1)).await.unwrap(),
        Cas::Conflict
    ));
    // Delete then recreate: new generation, and the pre-delete one cannot swap.
    s.delete("r").await.unwrap();
    s.delete("r").await.unwrap();
    assert!(matches!(
        s.cas("r", Bytes::from("y"), Some(&v2)).await.unwrap(),
        Cas::Conflict
    ));
    let Cas::Ok(v3) = s.cas("r", Bytes::from("x"), None).await.unwrap() else {
        panic!("recreate")
    };
    assert_ne!(v3.version, v1.version);
    assert_ne!(v3.version, v2.version);
}
