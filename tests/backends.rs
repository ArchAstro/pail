//! Runs the CAS contract against real object-store APIs when `PAIL_TEST_STORES` lists
//! store URLs (space separated), e.g. moto for S3 and fake-gcs-server for GCS.

use bytes::Bytes;
use pail::store::{Cas, Store};

#[tokio::test]
async fn cas_contract_on_configured_stores() {
    let Ok(urls) = std::env::var("PAIL_TEST_STORES") else { return };
    for url in urls.split_whitespace() {
        let run = format!("{url}/t{}", std::process::id());
        let s = Store::open(&run).unwrap();
        // Create-if-absent: first wins, second conflicts.
        let v1 = match s.cas("refs/main", Bytes::from("a"), None).await.unwrap() {
            Cas::Ok(v) => v,
            Cas::Conflict => panic!("{url}: create failed"),
        };
        assert!(matches!(s.cas("refs/main", Bytes::from("b"), None).await.unwrap(), Cas::Conflict), "{url}: create twice");
        // Update with the right version wins; a stale version conflicts.
        let (_, cur) = s.get_opt("refs/main").await.unwrap().unwrap();
        let v2 = match s.cas("refs/main", Bytes::from("c"), Some(&cur)).await.unwrap() {
            Cas::Ok(v) => v,
            Cas::Conflict => panic!("{url}: update with current version failed"),
        };
        assert_ne!(v1, v2);
        assert!(matches!(s.cas("refs/main", Bytes::from("d"), Some(&v1)).await.unwrap(), Cas::Conflict), "{url}: stale update");
        assert_eq!(s.get("refs/main").await.unwrap(), Bytes::from("c"));
        // Immutable puts are idempotent.
        assert!(s.put_new("pages/x", Bytes::from("1")).await.unwrap());
        assert!(!s.put_new("pages/x", Bytes::from("1")).await.unwrap());
        let r = s.get_ranges("pages/x", &[0..1]).await.unwrap();
        assert_eq!(r[0], Bytes::from("1"));
        assert_eq!(s.list("pages/").await.unwrap(), vec![("pages/x".to_string(), 1)]);
        eprintln!("{url}: CAS contract ok");
    }
}
