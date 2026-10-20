use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use bytes::Bytes;
use pail::hash::Hash;
use pail::manifest::{Entry, Kind, Loc};
use pail::repo::{CommitRequest, ConflictError, Delta, FileInput, Repo};
use pail::store::Store;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

fn temp_repo() -> (tempfile::TempDir, Repo) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file://{}", dir.path().join("bucket").display());
    let repo = Repo::with_store(Store::open(&url).unwrap(), None);
    (dir, repo)
}

fn fake_entry(path: &str, v: u64) -> Entry {
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&v.to_le_bytes());
    Entry { path: path.into(), kind: Kind::File, size: v, hash: Hash(h), loc: Loc::Inline(vec![]) }
}

fn random_path(rng: &mut StdRng) -> String {
    let dirs = ["src", "src/a", "src/b/c", "docs", "lib/x", "z"];
    format!("{}/f{:05}.rs", dirs[rng.random_range(0..dirs.len())], rng.random_range(0..40_000))
}

/// Incremental `apply_delta` must produce exactly the tree a from-scratch build produces,
/// and `diff` must report exactly the changes applied.
#[tokio::test]
async fn incremental_tree_is_canonical() {
    let (_d, repo) = temp_repo();
    let empty = repo.root_of(None).await.unwrap();
    let mut rng = StdRng::seed_from_u64(7);
    let mut model: BTreeMap<String, Entry> = BTreeMap::new();
    for i in 0..12_000 {
        let p = random_path(&mut rng);
        model.insert(p.clone(), fake_entry(&p, i));
    }
    let full: Delta = model.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
    let t = repo.apply_delta(empty, &full).await.unwrap();
    for p in &t.pages {
        repo.store.put_new(&format!("pages/{}", p.hash.hex()), p.data.clone()).await.unwrap();
    }
    let mut root = t.root;

    for round in 0..60 {
        let mut delta: Delta = BTreeMap::new();
        let n = rng.random_range(1..300);
        let keys: Vec<String> = model.keys().cloned().collect();
        for _ in 0..n {
            match rng.random_range(0..3) {
                0 => {
                    let p = random_path(&mut rng);
                    delta.insert(p.clone(), Some(fake_entry(&p, rng.random())));
                }
                1 if !keys.is_empty() => {
                    let p = &keys[rng.random_range(0..keys.len())];
                    delta.insert(p.clone(), Some(fake_entry(p, rng.random())));
                }
                _ if !keys.is_empty() => {
                    let p = &keys[rng.random_range(0..keys.len())];
                    delta.insert(p.clone(), None);
                }
                _ => {}
            }
        }
        // Occasionally delete a large contiguous block to force page merges.
        if round % 10 == 9 {
            for k in keys.iter().skip(rng.random_range(0..keys.len())).take(2_000) {
                delta.insert(k.clone(), None);
            }
        }
        let before = model.clone();
        for (k, v) in &delta {
            match v {
                Some(e) => {
                    model.insert(k.clone(), e.clone());
                }
                None => {
                    model.remove(k);
                }
            }
        }
        let inc = repo.apply_delta(root, &delta).await.unwrap();
        for p in &inc.pages {
            repo.store.put_new(&format!("pages/{}", p.hash.hex()), p.data.clone()).await.unwrap();
        }
        let scratch_delta: Delta = model.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
        let scratch = repo.apply_delta(empty, &scratch_delta).await.unwrap();
        assert_eq!(inc.root, scratch.root, "round {round}: incremental root differs from scratch build");
        assert_eq!(inc.files as usize, model.len());

        let got: Vec<Entry> = repo.entries(inc.root).await.unwrap();
        assert_eq!(got, model.values().cloned().collect::<Vec<_>>());

        let changes = repo.diff(root, inc.root).await.unwrap();
        let expected: Vec<String> = delta
            .iter()
            .filter(|(k, v)| match (before.get(*k), v) {
                (None, None) => false,
                (Some(a), Some(b)) => !a.same_content(b),
                _ => true,
            })
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(changes.iter().map(|c| c.path.clone()).collect::<Vec<_>>(), expected, "round {round}");
        root = inc.root;
    }
}

#[tokio::test]
async fn files_roundtrip_all_storage_classes() {
    let (_d, repo) = temp_repo();
    let mut rng = StdRng::seed_from_u64(1);
    let big: Vec<u8> = (0..3_500_000).map(|_| rng.random::<u8>() % 16).collect();
    let inputs = vec![
        FileInput { path: "a/empty".into(), kind: Kind::File, data: Bytes::new() },
        FileInput { path: "a/link".into(), kind: Kind::Symlink, data: Bytes::from_static(b"../target") },
        FileInput { path: "a/small.txt".into(), kind: Kind::File, data: Bytes::from(vec![b'x'; 5000]) },
        FileInput { path: "a/dup.txt".into(), kind: Kind::Exec, data: Bytes::from(vec![b'x'; 5000]) },
        FileInput { path: "b/big.bin".into(), kind: Kind::File, data: Bytes::from(big) },
    ];
    let entries = repo.write_files(&inputs, &HashMap::new()).await.unwrap();
    assert!(matches!(entries[0].loc, Loc::Inline(_)));
    assert!(matches!(entries[2].loc, Loc::Pack { .. }));
    assert_eq!(entries[2].loc, entries[3].loc, "duplicate content stored once");
    assert!(matches!(&entries[4].loc, Loc::Chunks(c) if c.len() > 1));
    let back = repo.read_files(&entries).await.unwrap();
    for (i, b) in inputs.iter().zip(back) {
        assert_eq!(i.data, b, "{}", i.path);
    }
}

fn req(branch: &str, base: Option<Hash>, delta: Delta, msg: &str) -> CommitRequest {
    CommitRequest {
        branch: branch.into(),
        base,
        delta,
        author: "T <t@example.com>".into(),
        author_date: "0 +0000".into(),
        committer: "T <t@example.com>".into(),
        committer_date: "0 +0000".into(),
        message: msg.into(),
        meta: BTreeMap::new(),
    }
}

async fn put_files(repo: &Repo, files: &[(&str, &str)]) -> Delta {
    let inputs: Vec<FileInput> = files
        .iter()
        .map(|(p, c)| FileInput { path: p.to_string(), kind: Kind::File, data: Bytes::from(c.to_string()) })
        .collect();
    repo.write_files(&inputs, &HashMap::new())
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.path.clone(), Some(e)))
        .collect()
}

#[tokio::test]
async fn concurrent_commits_replay_or_conflict() {
    let (_d, repo) = temp_repo();
    let repo = Arc::new(repo);
    let d0 = put_files(&repo, &[("shared.txt", "v0"), ("x.txt", "x")]).await;
    let c0 = repo.commit(req("main", None, d0, "init")).await.unwrap().commit;

    // 16 writers, disjoint paths, same base: all must land via CAS retry + replay.
    let mut tasks = vec![];
    for i in 0..16 {
        let repo = repo.clone();
        tasks.push(tokio::spawn(async move {
            let d = put_files(&repo, &[(&format!("w/{i}.txt"), &format!("writer {i}"))]).await;
            repo.commit(req("main", Some(c0), d, &format!("w{i}"))).await.unwrap()
        }));
    }
    let mut max_attempts = 0;
    for t in tasks {
        max_attempts = max_attempts.max(t.await.unwrap().attempts);
    }
    assert!(max_attempts > 1, "expected some CAS contention");
    let (head, _) = repo.read_ref("main").await.unwrap().unwrap();
    let c = repo.commit_obj(&head).await.unwrap();
    assert_eq!(c.files, 18);
    assert_eq!(c.generation, 16);

    // Overlapping path from a stale base must be a conflict, not a silent overwrite.
    let d = put_files(&repo, &[("shared.txt", "mine")]).await;
    repo.commit(req("main", Some(c0), d, "ok")).await.unwrap();
    let d = put_files(&repo, &[("shared.txt", "theirs")]).await;
    let err = repo.commit(req("main", Some(c0), d, "stale")).await.unwrap_err();
    let conflict = err.downcast_ref::<ConflictError>().expect("conflict error");
    assert_eq!(conflict.paths, vec!["shared.txt".to_string()]);
}

/// Every commit's change list is exactly the manifest diff from the parent it landed on,
/// including commits replayed onto a newer head and edits that change nothing.
#[tokio::test]
async fn change_lists_match_manifest_diffs() {
    let (_d, repo) = temp_repo();
    let repo = Arc::new(repo);
    let d0 = put_files(&repo, &[("a.txt", "a0"), ("b.txt", "b0"), ("dir/c.txt", "c0")]).await;
    let c0 = repo.commit(req("main", None, d0, "init")).await.unwrap().commit;
    // Concurrent writers from the same base: most land by replay onto someone else's commit.
    let mut tasks = vec![];
    for i in 0..8 {
        let repo = repo.clone();
        tasks.push(tokio::spawn(async move {
            let d = put_files(&repo, &[(&format!("w/{i}.txt"), &format!("writer {i}"))]).await;
            repo.commit(req("main", Some(c0), d, &format!("w{i}"))).await.unwrap()
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    // A modification, a deletion, and a no-op rewrite of b.txt in one commit.
    let (head, _) = repo.read_ref("main").await.unwrap().unwrap();
    let mut d = put_files(&repo, &[("a.txt", "a1"), ("b.txt", "b0")]).await;
    d.insert("dir/c.txt".into(), None);
    repo.commit(req("main", Some(head), d, "edit")).await.unwrap();

    let (mut cur, _) = repo.read_ref("main").await.unwrap().unwrap();
    let mut checked = 0;
    loop {
        let c = repo.commit_obj(&cur).await.unwrap();
        let listed = c.change_list().expect("new commits record their changes");
        let actual = repo.diff(repo.root_of(c.parent).await.unwrap(), c.root).await.unwrap();
        let key = |cs: &[pail::manifest::Change]| -> Vec<(String, Option<Hash>, Option<Hash>)> {
            cs.iter().map(|c| (c.path.clone(), c.old.as_ref().map(|e| e.hash), c.new.as_ref().map(|e| e.hash))).collect()
        };
        assert_eq!(key(&listed), key(&actual), "commit {}", c.message);
        for (l, a) in listed.iter().zip(&actual) {
            assert_eq!(l.new.as_ref().map(|e| &e.loc), a.new.as_ref().map(|e| &e.loc), "new side must locate the bytes");
        }
        if c.message == "edit" {
            assert_eq!(listed.len(), 2, "the no-op rewrite of b.txt is not a change");
        }
        checked += 1;
        match c.parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    assert_eq!(checked, 10);
}

/// Landing a stack races other writers on main: it retries until it lands in one piece, in
/// order and contiguous, and nobody's commit is lost.
#[tokio::test]
async fn stack_lands_atomically_under_contention() {
    let (_d, repo) = temp_repo();
    let repo = Arc::new(repo);
    let d0 = put_files(&repo, &[("base.txt", "b")]).await;
    let c0 = repo.commit(req("main", None, d0, "base")).await.unwrap().commit;
    repo.create_branch("stack", c0).await.unwrap();
    let mut tip = c0;
    for i in 0..5 {
        let d = put_files(&repo, &[(&format!("s/{i}.txt"), &format!("stack {i}"))]).await;
        tip = repo.commit(req("stack", Some(tip), d, &format!("s{i}"))).await.unwrap().commit;
    }
    // Writers on main and the lander, all at once.
    let mut tasks = vec![];
    for i in 0..8 {
        let repo = repo.clone();
        tasks.push(tokio::spawn(async move {
            let d = put_files(&repo, &[(&format!("w/{i}.txt"), &format!("writer {i}"))]).await;
            repo.commit(req("main", Some(c0), d, &format!("w{i}"))).await.unwrap();
        }));
    }
    let lander = {
        let repo = repo.clone();
        tokio::spawn(async move { repo.land_stack("stack", "main", "Lander <l@x>", "1 +0000").await.unwrap() })
    };
    for t in tasks {
        t.await.unwrap();
    }
    let out = lander.await.unwrap();
    assert_eq!(out.landed.len(), 5);
    // Walk main: 8 writers + 5 stack commits + base, the stack contiguous and in order.
    let (head, _) = repo.read_ref("main").await.unwrap().unwrap();
    let mut subjects = vec![];
    let mut cur = Some(head);
    while let Some(h) = cur {
        let c = repo.commit_obj(&h).await.unwrap();
        let listed = c.change_list().unwrap();
        let actual = repo.diff(repo.root_of(c.parent).await.unwrap(), c.root).await.unwrap();
        assert_eq!(listed.iter().map(|x| &x.path).collect::<Vec<_>>(), actual.iter().map(|x| &x.path).collect::<Vec<_>>());
        subjects.push(c.message.clone());
        cur = c.parent;
    }
    subjects.reverse();
    assert_eq!(subjects.len(), 14, "{subjects:?}");
    let s0 = subjects.iter().position(|s| s == "s0").unwrap();
    assert_eq!(&subjects[s0..s0 + 5], ["s0", "s1", "s2", "s3", "s4"], "{subjects:?}");
    for i in 0..8 {
        assert!(subjects.contains(&format!("w{i}")), "writer {i} lost: {subjects:?}");
    }
    let c = repo.commit_obj(&head).await.unwrap();
    assert_eq!(c.files, 1 + 5 + 8);
}
