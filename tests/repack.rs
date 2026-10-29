//! `pail maintenance repack` (`pail::repack`): the repack is an ordinary commit with the
//! same content, its blobs are the stored bytes copied, and it survives concurrent commits.
//! `verify/` models the protocol; these check the planner and the commit it makes.

use std::collections::{BTreeMap, HashMap, HashSet};

use bytes::Bytes;
use pail::hash::Hash;
use pail::manifest::{Entry, Kind, Loc};
use pail::pack::pack_key;
use pail::repack::{Options, Outcome, Who, plan};
use pail::repo::{CommitRequest, FileInput, Repo};
use pail::store::Store;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const BRANCH: &str = "main";

fn temp_repo() -> (tempfile::TempDir, Repo) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file://{}", dir.path().join("bucket").display());
    (dir, Repo::with_store(Store::open(&url).unwrap(), None))
}

fn who() -> Who {
    Who {
        author: "Maintainer <m@example.com>".into(),
        committer: "Maintainer <m@example.com>".into(),
        date: "1700000000 +0000".into(),
    }
}

/// Incompressible bytes, so a pack's size is about its files'.
fn noise(rng: &mut StdRng, len: usize) -> Vec<u8> {
    (0..len).map(|_| rng.random()).collect()
}

/// Commit `files` on `main`, one pack of their own.
async fn commit(repo: &Repo, files: &[(String, Vec<u8>)], meta: &[(&str, &str)]) -> Hash {
    let inputs: Vec<FileInput> = files
        .iter()
        .map(|(path, data)| FileInput {
            path: path.clone(),
            kind: Kind::File,
            data: Bytes::from(data.clone()),
        })
        .collect();
    let base = repo.read_ref(BRANCH).await.unwrap().map(|(h, _)| h);
    let writing = repo.writing().await.unwrap();
    let entries = repo.write_files(&inputs, &HashMap::new()).await.unwrap();
    let delta = entries
        .into_iter()
        .map(|e| (e.path.clone(), Some(e)))
        .collect();
    let out = repo
        .commit(CommitRequest {
            branch: BRANCH.into(),
            base,
            delta,
            author: "Dev <d@example.com>".into(),
            author_date: "1600000000 +0000".into(),
            committer: "Dev <d@example.com>".into(),
            committer_date: "1600000000 +0000".into(),
            message: "work\n".into(),
            meta: meta
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })
        .await
        .unwrap();
    writing.done().await.unwrap();
    out.commit
}

/// A history of `commits` commits, each writing `per` small files (and so one small pack),
/// plus one commit with a dense pack (files of 20 KB). Returns the tip.
async fn history(repo: &Repo, commits: usize, per: usize) -> Hash {
    let mut rng = StdRng::seed_from_u64(1);
    let dense: Vec<(String, Vec<u8>)> = (0..4)
        .map(|i| (format!("dense/d{i}.bin"), noise(&mut rng, 20_000)))
        .collect();
    commit(repo, &dense, &[]).await;
    let mut tip = Hash([0; 32]);
    for c in 0..commits {
        let files: Vec<(String, Vec<u8>)> = (0..per)
            .map(|i| (format!("src/c{c:02}/f{i}.rs"), noise(&mut rng, 300)))
            .collect();
        tip = commit(repo, &files, &[("git.commit", &format!("{c:040x}"))]).await;
    }
    tip
}

/// Tiered at 8 KB: the small packs move, the dense one stays.
fn opts() -> Options {
    let mut o = Options::new(BRANCH);
    o.min_pack_bytes = 8 << 10;
    o
}

async fn tip_entries(repo: &Repo) -> (Hash, Vec<Entry>) {
    let (tip, _) = repo.read_ref(BRANCH).await.unwrap().unwrap();
    let root = repo.commit_obj(&tip).await.unwrap().root;
    (tip, repo.entries(root).await.unwrap())
}

fn packs_of(entries: &[Entry]) -> HashSet<Hash> {
    entries
        .iter()
        .filter_map(|e| match &e.loc {
            Loc::Pack { pack, .. } => Some(*pack),
            _ => None,
        })
        .collect()
}

async fn contents(repo: &Repo, entries: &[Entry]) -> BTreeMap<String, Bytes> {
    let data = repo.read_files(entries).await.unwrap();
    entries.iter().map(|e| e.path.clone()).zip(data).collect()
}

fn done(out: Outcome) -> (pail::repack::Stats, Hash, Hash, usize, usize) {
    match out {
        Outcome::Done {
            stats,
            commit,
            parent,
            attempts,
            dropped,
        } => (stats, commit, parent, attempts, dropped),
        other => panic!("expected a repack, got {other:?}"),
    }
}

#[tokio::test]
async fn tiered_repack_changes_layout_not_content() {
    let (_d, repo) = temp_repo();
    history(&repo, 12, 3).await;
    let (old_tip, before) = tip_entries(&repo).await;
    let old_contents = contents(&repo, &before).await;
    assert_eq!(packs_of(&before).len(), 13, "a pack per commit");

    let (stats, commit, parent, attempts, dropped) = done(
        pail::repack::repack(&repo, &opts(), &who(), None)
            .await
            .unwrap(),
    );
    assert_eq!((attempts, dropped, parent), (1, 0, old_tip));
    assert_eq!(stats.moved_files, 36);
    assert_eq!(stats.source_packs, 12);
    assert_eq!(stats.new_packs, 1);
    assert_eq!((stats.requests_before, stats.requests_after), (13, 2));

    // An ordinary commit on top of the old tip, with the same content.
    let (tip, after) = tip_entries(&repo).await;
    assert_eq!(tip, commit);
    let c = repo.commit_obj(&tip).await.unwrap();
    assert_eq!(c.parent, Some(old_tip));
    assert_eq!(c.files, repo.commit_obj(&old_tip).await.unwrap().files);
    assert_eq!(
        c.meta.get("pail.maintenance").map(String::as_str),
        Some("repack")
    );
    assert_eq!(
        c.meta.get("git.commit"),
        repo.commit_obj(&old_tip)
            .await
            .unwrap()
            .meta
            .get("git.commit"),
        "the tip's git.* meta is carried so a mirror resumes"
    );
    let old_root = repo.commit_obj(&old_tip).await.unwrap().root;
    assert!(
        repo.diff(old_root, c.root).await.unwrap().is_empty(),
        "no path changed"
    );
    assert_eq!(contents(&repo, &after).await, old_contents);
    assert_eq!(
        before
            .iter()
            .map(|e| (&e.path, e.hash, e.size))
            .collect::<Vec<_>>(),
        after
            .iter()
            .map(|e| (&e.path, e.hash, e.size))
            .collect::<Vec<_>>()
    );

    // The dense pack stayed; the small ones are one pack, in path order.
    let dense_before: Vec<&Entry> = before
        .iter()
        .filter(|e| e.path.starts_with("dense/"))
        .collect();
    let dense_after: Vec<&Entry> = after
        .iter()
        .filter(|e| e.path.starts_with("dense/"))
        .collect();
    assert_eq!(
        dense_before.iter().map(|e| &e.loc).collect::<Vec<_>>(),
        dense_after.iter().map(|e| &e.loc).collect::<Vec<_>>()
    );
    let mut offsets = after
        .iter()
        .filter(|e| e.path.starts_with("src/"))
        .map(|e| match &e.loc {
            Loc::Pack { off, .. } => *off,
            other => panic!("{other:?}"),
        });
    let first = offsets.next().unwrap();
    assert_eq!(first, 0);
    assert!(offsets.collect::<Vec<_>>().windows(2).all(|w| w[0] < w[1]));

    // Old history still reads (its packs are alive), and gc deletes none of it.
    let report = pail::gc::gc(&repo, 0, false).await.unwrap();
    assert!(
        report.deleted.iter().all(|k| !k.starts_with("packs/")),
        "{:?}",
        report.deleted
    );
    let old_entries = repo.entries(old_root).await.unwrap();
    assert_eq!(contents(&repo, &old_entries).await, old_contents);
}

#[tokio::test]
async fn blobs_are_copied_as_stored() {
    let (_d, repo) = temp_repo();
    history(&repo, 4, 2).await;
    let (_, before) = tip_entries(&repo).await;
    done(
        pail::repack::repack(&repo, &opts(), &who(), None)
            .await
            .unwrap(),
    );
    let (_, after) = tip_entries(&repo).await;
    let read = |e: &Entry| {
        let Loc::Pack { pack, off, len } = &e.loc else {
            panic!("{e:?}")
        };
        (pack_key(pack), *off..off + len)
    };
    for (b, a) in before.iter().zip(&after) {
        let (kb, rb) = read(b);
        let (ka, ra) = read(a);
        let old = repo.store.get_ranges(&kb, &[rb]).await.unwrap();
        let new = repo.store.get_ranges(&ka, &[ra]).await.unwrap();
        assert_eq!(old, new, "{}", b.path);
    }
}

#[tokio::test]
async fn full_repack_moves_the_dense_pack_too() {
    let (_d, repo) = temp_repo();
    history(&repo, 6, 2).await;
    let (_, before) = tip_entries(&repo).await;
    let mut o = opts();
    o.full = true;
    let (stats, ..) = done(pail::repack::repack(&repo, &o, &who(), None).await.unwrap());
    assert_eq!(stats.moved_files, before.len());
    assert_eq!(stats.source_packs, 7);
    let (_, after) = tip_entries(&repo).await;
    assert!(packs_of(&after).is_disjoint(&packs_of(&before)));
    assert_eq!(
        contents(&repo, &after).await,
        contents(&repo, &before).await
    );
}

#[tokio::test]
async fn target_size_splits_the_new_packs() {
    let (_d, repo) = temp_repo();
    history(&repo, 12, 3).await;
    let mut o = opts();
    o.target_bytes = 3_000;
    let (stats, ..) = done(pail::repack::repack(&repo, &o, &who(), None).await.unwrap());
    let (_, after) = tip_entries(&repo).await;
    // 36 blobs of about 300 bytes: a pack closes once it holds 3,000.
    assert_eq!(stats.new_packs, 4);
    assert_eq!(
        packs_of(&after).len(),
        5,
        "four new packs and the dense one"
    );
}

#[tokio::test]
async fn dry_run_writes_nothing_and_predicts_the_run() {
    let (_d, repo) = temp_repo();
    history(&repo, 8, 3).await;
    let (tip, _) = tip_entries(&repo).await;
    let packs = repo.store.list("packs/").await.unwrap().len();
    let mut o = opts();
    o.dry_run = true;
    let Outcome::Planned(planned) = pail::repack::repack(&repo, &o, &who(), None).await.unwrap()
    else {
        panic!("a dry run plans")
    };
    assert_eq!(tip_entries(&repo).await.0, tip, "the ref did not move");
    assert_eq!(repo.store.list("packs/").await.unwrap().len(), packs);
    let (stats, ..) = done(
        pail::repack::repack(&repo, &opts(), &who(), None)
            .await
            .unwrap(),
    );
    assert_eq!(planned, stats);
    assert_eq!(repo.store.list("packs/").await.unwrap().len(), packs + 1);
}

#[tokio::test]
async fn nothing_to_move_commits_nothing() {
    let (_d, repo) = temp_repo();
    history(&repo, 1, 3).await;
    let (tip, _) = tip_entries(&repo).await;
    // One small pack, and the dense one: a tiered repack would only copy it.
    let out = pail::repack::repack(&repo, &opts(), &who(), None)
        .await
        .unwrap();
    assert!(matches!(out, Outcome::Nothing(_)), "{out:?}");
    assert_eq!(tip_entries(&repo).await.0, tip);
    // Repeating a repack leaves the one small pack it made alone.
    let mut rng = StdRng::seed_from_u64(9);
    for c in 0..5 {
        let files = vec![(format!("more/c{c}.rs"), noise(&mut rng, 300))];
        commit(&repo, &files, &[]).await;
    }
    done(
        pail::repack::repack(&repo, &opts(), &who(), None)
            .await
            .unwrap(),
    );
    let (tip, _) = tip_entries(&repo).await;
    let out = pail::repack::repack(&repo, &opts(), &who(), None)
        .await
        .unwrap();
    assert!(matches!(out, Outcome::Nothing(_)), "{out:?}");
    assert_eq!(
        tip_entries(&repo).await.0,
        tip,
        "a second repack adds no commit"
    );
}

#[tokio::test]
async fn a_corrupt_source_blob_is_refused() {
    let (dir, repo) = temp_repo();
    history(&repo, 4, 2).await;
    let (tip, before) = tip_entries(&repo).await;
    // Flip a byte inside the first small file's stored blob.
    let Loc::Pack { pack, off, .. } = before
        .iter()
        .find(|e| e.path.starts_with("src/"))
        .unwrap()
        .loc
        .clone()
    else {
        panic!()
    };
    let path = std::fs::read_dir(dir.path().join("bucket").join("packs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().contains(&pack.hex()))
        .expect("the pack's file");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[off as usize + 10] ^= 0xff;
    std::fs::write(&path, bytes).unwrap();
    let err = pail::repack::repack(&repo, &opts(), &who(), None)
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("hash") || format!("{err:#}").contains("blob"),
        "{err:#}"
    );
    assert_eq!(tip_entries(&repo).await.0, tip, "nothing was published");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_commit_to_another_path_is_replayed_over() {
    let (_d, repo) = temp_repo();
    history(&repo, 6, 2).await;
    let hook = |attempt: usize| {
        assert_eq!(attempt, 1);
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let fresh = vec![("docs/new.md".to_string(), vec![b'x'; 200])];
                commit(&repo, &fresh, &[]).await;
            })
        })
    };
    let (_, _, _, attempts, dropped) = done(
        pail::repack::repack(&repo, &opts(), &who(), Some(&hook))
            .await
            .unwrap(),
    );
    assert_eq!((attempts, dropped), (1, 0), "replayed, not retried");
    let (_, after) = tip_entries(&repo).await;
    assert!(after.iter().any(|e| e.path == "docs/new.md"));
    assert_eq!(after.len(), 4 + 12 + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_commit_to_a_moved_path_is_left_out_and_the_rest_retried() {
    let (_d, repo) = temp_repo();
    history(&repo, 6, 2).await;
    let changed = "src/c03/f1.rs";
    let hook = |attempt: usize| {
        if attempt == 1 {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let edit = vec![(changed.to_string(), vec![b'e'; 200])];
                    commit(&repo, &edit, &[]).await;
                })
            })
        }
    };
    let (_, _, _, attempts, dropped) = done(
        pail::repack::repack(&repo, &opts(), &who(), Some(&hook))
            .await
            .unwrap(),
    );
    assert_eq!((attempts, dropped), (2, 1));
    let (_, after) = tip_entries(&repo).await;
    let data = contents(&repo, &after).await;
    assert_eq!(
        data[changed].as_ref(),
        &vec![b'e'; 200][..],
        "the concurrent edit won"
    );
    // The edit's own pack (a small one) was not moved; every other small file was.
    let edit = after.iter().find(|e| e.path == changed).unwrap();
    let moved: Vec<&Entry> = after
        .iter()
        .filter(|e| e.path.starts_with("src/") && e.path != changed)
        .collect();
    assert_eq!(moved.len(), 11);
    let new_pack = packs_of(&[moved[0].clone()]);
    assert!(moved.iter().all(|e| packs_of(&[(*e).clone()]) == new_pack));
    assert!(!packs_of(std::slice::from_ref(edit)).is_subset(&new_pack));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_branch_that_keeps_changing_the_moved_files_is_given_up_on() {
    let (_d, repo) = temp_repo();
    history(&repo, 6, 2).await;
    let hook = |attempt: usize| {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let edit = vec![(
                    format!("src/c0{attempt}/f0.rs"),
                    vec![b'0' + attempt as u8; 200],
                )];
                commit(&repo, &edit, &[]).await;
            })
        })
    };
    let mut o = opts();
    o.attempts = 2;
    let err = pail::repack::repack(&repo, &o, &who(), Some(&hook))
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("kept changing"), "{err}");
    // Everything is intact: the branch holds only the concurrent commits.
    let (_, after) = tip_entries(&repo).await;
    let data = contents(&repo, &after).await;
    assert_eq!(data.len(), 4 + 12);
}

#[test]
fn the_plan_needs_two_small_packs_and_is_path_ordered() {
    let pack = |n: u8| Hash([n; 32]);
    let entry = |path: &str, n: u8, off: u64| Entry {
        path: path.into(),
        kind: Kind::File,
        size: 100,
        hash: Hash([n.wrapping_add(off as u8); 32]),
        loc: Loc::Pack {
            pack: pack(n),
            off,
            len: 50,
        },
    };
    let entries = vec![
        entry("a", 1, 0),
        entry("b", 2, 0),
        entry("c", 3, 0),
        entry("d", 3, 50),
    ];
    let sizes: HashMap<Hash, u64> = [(pack(1), 100), (pack(2), 100), (pack(3), 1 << 30)].into();
    let p = plan(&entries, &sizes, &Options::new("main"));
    assert_eq!(
        p.moves.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!((p.stats.requests_before, p.stats.requests_after), (3, 2));
    let sizes: HashMap<Hash, u64> = [(pack(1), 100), (pack(2), 1 << 30), (pack(3), 1 << 30)].into();
    assert!(
        plan(&entries, &sizes, &Options::new("main"))
            .moves
            .is_empty()
    );
    let mut full = Options::new("main");
    full.full = true;
    assert_eq!(plan(&entries, &sizes, &full).moves.len(), 4);
}

#[tokio::test]
async fn a_mirror_resumes_below_maintenance_commits() {
    let (_d, repo) = temp_repo();
    let work = history(&repo, 6, 2).await;
    let sha = repo.commit_obj(&work).await.unwrap().meta["git.commit"].clone();
    let (_, repack1, ..) = done(
        pail::repack::repack(&repo, &opts(), &who(), None)
            .await
            .unwrap(),
    );
    // More small commits, then a second repack: two maintenance commits in the history.
    let mut rng = StdRng::seed_from_u64(5);
    let mut last = repack1;
    for c in 0..3 {
        let files = vec![(format!("late/c{c}.rs"), noise(&mut rng, 300))];
        last = commit(
            &repo,
            &files,
            &[("git.commit", &format!("{:040x}", 100 + c))],
        )
        .await;
    }
    let (_, repack2, ..) = done(
        pail::repack::repack(&repo, &opts(), &who(), None)
            .await
            .unwrap(),
    );

    let (h, c) = pail::repack::skip_maintenance(&repo, repack1)
        .await
        .unwrap();
    assert_eq!((h, c.meta["git.commit"].clone()), (work, sha));
    let (h, c) = pail::repack::skip_maintenance(&repo, repack2)
        .await
        .unwrap();
    assert_eq!(h, last);
    assert_eq!(c.meta["git.commit"], format!("{:040x}", 102));
    assert!(repo.commit_obj(&repack2).await.unwrap().is_maintenance());
    assert!(!repo.commit_obj(&last).await.unwrap().is_maintenance());
}
