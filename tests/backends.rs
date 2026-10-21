//! Runs the CAS contract against real object-store APIs when `PAIL_TEST_STORES` lists
//! store URLs (space separated), e.g. moto for S3 and fake-gcs-server for GCS.

use bytes::Bytes;
use pail::store::{Cas, Store};

#[tokio::test]
async fn cas_contract_on_configured_stores() {
    let Ok(urls) = std::env::var("PAIL_TEST_STORES") else {
        return;
    };
    for url in urls.split_whitespace() {
        let run = format!("{url}/t{}", std::process::id());
        let s = Store::open(&run).unwrap();
        // Create-if-absent: first wins, second conflicts.
        let v1 = match s.cas("refs/main", Bytes::from("a"), None).await.unwrap() {
            Cas::Ok(v) => v,
            Cas::Conflict => panic!("{url}: create failed"),
        };
        assert!(
            matches!(
                s.cas("refs/main", Bytes::from("b"), None).await.unwrap(),
                Cas::Conflict
            ),
            "{url}: create twice"
        );
        // Update with the right version wins; a stale version conflicts.
        let (_, cur) = s.get_opt("refs/main").await.unwrap().unwrap();
        let v2 = match s
            .cas("refs/main", Bytes::from("c"), Some(&cur))
            .await
            .unwrap()
        {
            Cas::Ok(v) => v,
            Cas::Conflict => panic!("{url}: update with current version failed"),
        };
        assert_ne!(v1, v2);
        assert!(
            matches!(
                s.cas("refs/main", Bytes::from("d"), Some(&v1))
                    .await
                    .unwrap(),
                Cas::Conflict
            ),
            "{url}: stale update"
        );
        assert_eq!(s.get("refs/main").await.unwrap(), Bytes::from("c"));
        // Immutable puts are idempotent.
        assert!(s.put_new("pages/x", Bytes::from("1")).await.unwrap());
        assert!(!s.put_new("pages/x", Bytes::from("1")).await.unwrap());
        let r = s.get_ranges("pages/x", &[0..1]).await.unwrap();
        assert_eq!(r[0], Bytes::from("1"));
        assert_eq!(
            s.list("pages/").await.unwrap(),
            vec![("pages/x".to_string(), 1)]
        );
        eprintln!("{url}: CAS contract ok");
    }
}

/// Concurrent writers on one branch of a real store: all disjoint commits must land.
#[tokio::test]
async fn concurrent_commits_on_configured_stores() {
    use pail::manifest::Kind;
    use pail::repo::{CommitRequest, FileInput, Repo};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;
    let Ok(urls) = std::env::var("PAIL_TEST_STORES") else {
        return;
    };
    for url in urls.split_whitespace() {
        let run = format!("{url}/c{}", std::process::id());
        let repo = Arc::new(Repo::with_store(Store::open(&run).unwrap(), None));
        let req = |base, delta, msg: String| CommitRequest {
            branch: "main".into(),
            base,
            delta,
            author: "T <t@x>".into(),
            author_date: "0 +0000".into(),
            committer: "T <t@x>".into(),
            committer_date: "0 +0000".into(),
            message: msg,
            meta: BTreeMap::new(),
        };
        let c0 = repo
            .commit(req(None, BTreeMap::new(), "init".into()))
            .await
            .unwrap()
            .commit;
        let mut tasks = vec![];
        for i in 0..16 {
            let repo = repo.clone();
            tasks.push(tokio::spawn(async move {
                let input = FileInput {
                    path: format!("w/{i}.txt"),
                    kind: Kind::File,
                    data: Bytes::from(format!("writer {i} ").repeat(20)),
                };
                let e = repo
                    .write_files(&[input], &HashMap::new())
                    .await
                    .unwrap()
                    .remove(0);
                repo.commit(req(
                    Some(c0),
                    BTreeMap::from([(e.path.clone(), Some(e))]),
                    format!("w{i}"),
                ))
                .await
                .unwrap()
            }));
        }
        let mut attempts = vec![];
        for t in tasks {
            attempts.push(t.await.unwrap().attempts);
        }
        let (head, _) = repo.read_ref("main").await.unwrap().unwrap();
        let c = repo.commit_obj(&head).await.unwrap();
        assert_eq!((c.files, c.generation), (16, 16), "{url}");
        let root = c.root;
        let entries = repo.entries(root).await.unwrap();
        let data = repo.read_files(&entries).await.unwrap();
        assert!(
            data.iter()
                .enumerate()
                .all(|(_, d)| d.starts_with(b"writer "))
        );
        eprintln!(
            "{url}: 16 concurrent writers landed; max attempts {}",
            attempts.iter().max().unwrap()
        );
    }
}
