//! Amortized compaction (`pail::amortize`): ordinary commits relocate a bounded part of the
//! tip's small packs into the pack they upload anyway, and the relocations are soft.
//! `verify/` models the protocol ("Amortized compaction"); these check the planner's effect
//! through `Worktree::commit` and the soft rule through `Repo::commit_opts`.

use std::collections::{BTreeMap, HashMap, HashSet};

use bytes::Bytes;
use pail::hash::Hash;
use pail::manifest::{Entry, Kind, Loc};
use pail::repo::{CommitOptions, CommitRequest, ConflictError, FileInput, Reloc, Repo};
use pail::store::Store;
use pail::worktree::{Checkout, Worktree};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const BRANCH: &str = "main";

fn identity() {
    // SAFETY: tests in this file set the same values.
    unsafe {
        std::env::set_var("PAIL_AUTHOR_NAME", "Dev");
        std::env::set_var("PAIL_AUTHOR_EMAIL", "d@example.com");
        std::env::set_var("PAIL_COMMITTER_NAME", "Dev");
        std::env::set_var("PAIL_COMMITTER_EMAIL", "d@example.com");
        std::env::set_var("PAIL_CONFIG_GLOBAL", "/nonexistent/pail-config");
    }
}

fn noise(rng: &mut StdRng, len: usize) -> Vec<u8> {
    (0..len).map(|_| rng.random()).collect()
}

fn temp_store() -> (tempfile::TempDir, String, Repo) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file://{}", dir.path().join("bucket").display());
    let repo = Repo::with_store(Store::open(&url).unwrap(), None);
    (dir, url, repo)
}

async fn commit_files(repo: &Repo, files: &[(String, Vec<u8>)]) -> Hash {
    let inputs: Vec<FileInput> = files
        .iter()
        .map(|(path, data)| FileInput {
            path: path.clone(),
            kind: Kind::File,
            data: Bytes::from(data.clone()),
        })
        .collect();
    let base = repo.read_ref(BRANCH).await.unwrap().map(|(h, _)| h);
    let (entries, uploads) = repo.prepare_files(&inputs, &HashMap::new());
    let out = repo
        .commit_opts(
            request(
                base,
                entries
                    .into_iter()
                    .map(|e| (e.path.clone(), Some(e)))
                    .collect(),
            ),
            CommitOptions {
                uploads,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    out.commit
}

fn request(base: Option<Hash>, delta: pail::repo::Delta) -> CommitRequest {
    CommitRequest {
        branch: BRANCH.into(),
        base,
        delta,
        author: "Dev <d@example.com>".into(),
        author_date: "1600000000 +0000".into(),
        committer: "Dev <d@example.com>".into(),
        committer_date: "1600000000 +0000".into(),
        message: "work\n".into(),
        meta: BTreeMap::new(),
    }
}

/// `n` commits of one 2 KB file each (one small pack each); returns the files.
async fn history(repo: &Repo, n: usize) -> Vec<(String, Vec<u8>)> {
    let mut rng = StdRng::seed_from_u64(3);
    let mut all = vec![];
    for i in 0..n {
        let f = (format!("src/m{i:02}/f.bin"), noise(&mut rng, 2000));
        commit_files(repo, std::slice::from_ref(&f)).await;
        all.push(f);
    }
    all
}

async fn tip_entries(repo: &Repo) -> Vec<Entry> {
    let (tip, _) = repo.read_ref(BRANCH).await.unwrap().unwrap();
    let root = repo.commit_obj(&tip).await.unwrap().root;
    repo.entries(root).await.unwrap()
}

fn pack_count(es: &[Entry]) -> usize {
    es.iter()
        .filter_map(|e| match &e.loc {
            Loc::Pack { pack, .. } => Some(*pack),
            _ => None,
        })
        .collect::<HashSet<_>>()
        .len()
}

async fn checkout(url: &str, dir: &std::path::Path, repo: &Repo) -> Worktree {
    let mut co = Checkout::for_repo(repo);
    Worktree::clone(url, None, dir, BRANCH, None, &mut co)
        .await
        .unwrap()
}

async fn commit_new_file(wt: &mut Worktree, name: &str, len: usize) {
    let mut rng = StdRng::seed_from_u64(99);
    std::fs::write(wt.root.join(name), noise(&mut rng, len)).unwrap();
    wt.add(&[name.to_string()], false).await.unwrap();
    wt.commit("add\n", true).await.unwrap();
}

#[tokio::test]
async fn commit_relocates_small_packs_into_its_own() {
    identity();
    let (_d, url, repo) = temp_store();
    let files = history(&repo, 20).await;
    let before = tip_entries(&repo).await;
    assert_eq!(pack_count(&before), 20);
    let dir = tempfile::tempdir().unwrap();
    let mut wt = checkout(&url, dir.path(), &repo).await;
    commit_new_file(&mut wt, "new.bin", 3000).await;
    let after = tip_entries(&wt.repo).await;
    // All 20 old files (40 KB, within the 256 KiB floor) moved into the commit's one pack.
    assert_eq!(pack_count(&after), 1, "{after:#?}");
    // Same bytes everywhere.
    let data = wt.repo.read_files(&after).await.unwrap();
    let got: BTreeMap<String, Bytes> = after.iter().map(|e| e.path.clone()).zip(data).collect();
    for (path, bytes) in &files {
        assert_eq!(got[path].as_ref(), bytes.as_slice(), "{path}");
    }
    assert_eq!(got.len(), 21);
    // The commit's change list names only the new file.
    let tip = wt.head.base.unwrap();
    let changed = wt.repo.changed_paths(&tip);
    if let Some(c) = changed {
        assert_eq!(c, vec!["new.bin".to_string()]);
    }
    let diff = wt
        .repo
        .diff(
            wt.repo
                .root_of(wt.repo.commit_obj(&tip).await.unwrap().parent)
                .await
                .unwrap(),
            wt.repo.commit_obj(&tip).await.unwrap().root,
        )
        .await
        .unwrap();
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].path, "new.bin");
}

#[tokio::test]
async fn nothing_moves_below_the_trigger() {
    identity();
    let (_d, url, repo) = temp_store();
    history(&repo, 10).await;
    let dir = tempfile::tempdir().unwrap();
    let mut wt = checkout(&url, dir.path(), &repo).await;
    commit_new_file(&mut wt, "new.bin", 3000).await;
    assert_eq!(pack_count(&tip_entries(&wt.repo).await), 11);
}

#[tokio::test]
async fn config_opts_out() {
    identity();
    let (_d, url, repo) = temp_store();
    history(&repo, 20).await;
    let dir = tempfile::tempdir().unwrap();
    let mut wt = checkout(&url, dir.path(), &repo).await;
    std::fs::write(
        wt.local.dir.join("config"),
        "[maintenance]\n\tamortize = false\n",
    )
    .unwrap();
    commit_new_file(&mut wt, "new.bin", 3000).await;
    assert_eq!(pack_count(&tip_entries(&wt.repo).await), 21);
}

#[tokio::test]
async fn dirty_missing_and_excluded_files_are_skipped() {
    identity();
    let (_d, url, repo) = temp_store();
    let files = history(&repo, 20).await;
    let dir = tempfile::tempdir().unwrap();
    let mut wt = checkout(&url, dir.path(), &repo).await;
    // Edited, missing (as a sparse checkout leaves it): their packs stay where they are.
    std::fs::write(wt.root.join(&files[0].0), b"edited locally").unwrap();
    std::fs::remove_file(wt.root.join(&files[1].0)).unwrap();
    commit_new_file(&mut wt, "new.bin", 3000).await;
    let after = tip_entries(&wt.repo).await;
    // 18 moved into the new pack; two old packs remain.
    assert_eq!(pack_count(&after), 3);
    let by_path: HashMap<&str, &Entry> = after.iter().map(|e| (e.path.as_str(), e)).collect();
    let new_pack = match &by_path["new.bin"].loc {
        Loc::Pack { pack, .. } => *pack,
        l => panic!("{l:?}"),
    };
    for (i, (path, _)) in files.iter().enumerate() {
        let in_new =
            matches!(&by_path[path.as_str()].loc, Loc::Pack { pack, .. } if *pack == new_pack);
        assert_eq!(in_new, i >= 2, "{path}");
    }
    // The edit was not committed, and the stored content is still the original.
    let data = wt.repo.read_files(&after).await.unwrap();
    let got: BTreeMap<String, Bytes> = after.iter().map(|e| e.path.clone()).zip(data).collect();
    assert_eq!(got[&files[0].0].as_ref(), files[0].1.as_slice());
}

#[tokio::test]
async fn the_budget_bounds_the_extra_bytes() {
    identity();
    let (_d, url, repo) = temp_store();
    // 20 files of 40 KB each: 800 KB of old packs against a 256 KiB floor.
    let mut rng = StdRng::seed_from_u64(5);
    for i in 0..20 {
        commit_files(
            &repo,
            &[(format!("big/{i:02}.bin"), noise(&mut rng, 40_000))],
        )
        .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut wt = checkout(&url, dir.path(), &repo).await;
    commit_new_file(&mut wt, "new.bin", 100).await;
    let after = tip_entries(&wt.repo).await;
    // Whole packs only: floor 262144 / 40001 = 6 packs moved, 14 stay, plus the new one.
    assert_eq!(pack_count(&after), 15);
}

/// A relocation of `path` as `Worktree::commit` would plan it: the entry, and the same
/// content written again by `prepare_files_moving`.
async fn plan_move(
    repo: &Repo,
    files: &[(String, Vec<u8>)],
    new: &[(String, Vec<u8>)],
) -> (Vec<Entry>, Vec<Reloc>, Vec<pail::repo::Upload>) {
    let tip = tip_entries(repo).await;
    let by_path: HashMap<&str, &Entry> = tip.iter().map(|e| (e.path.as_str(), e)).collect();
    let moving: Vec<(Entry, Bytes)> = files
        .iter()
        .map(|(p, d)| (by_path[p.as_str()].clone(), Bytes::from(d.clone())))
        .collect();
    let inputs: Vec<FileInput> = new
        .iter()
        .map(|(p, d)| FileInput {
            path: p.clone(),
            kind: Kind::File,
            data: Bytes::from(d.clone()),
        })
        .collect();
    repo.prepare_files_moving(&inputs, &HashMap::new(), &moving)
}

#[tokio::test]
async fn relocations_never_make_a_commit_conflict() {
    let (_d, _url, repo) = temp_store();
    let files = history(&repo, 4).await;
    let (base, _) = repo.read_ref(BRANCH).await.unwrap().unwrap();
    let mut rng = StdRng::seed_from_u64(8);
    // The user's commit, planned on `base`, relocates all four old files and adds `mine`.
    let mine = vec![("mine.bin".to_string(), noise(&mut rng, 500))];
    let (entries, relocs, uploads) = plan_move(&repo, &files, &mine).await;
    assert_eq!(relocs.len(), 4);
    // Another writer changes one relocated path and relocates (repacks) another before it lands.
    let other = vec![(files[0].0.clone(), noise(&mut rng, 700))];
    commit_files(&repo, &other).await;
    let tip_before = tip_entries(&repo).await;
    let repacked = tip_before
        .iter()
        .find(|e| e.path == files[1].0)
        .unwrap()
        .clone();
    // The repack moves it into a pack with another file (a pack of the same bytes alone
    // would be the very pack it is in).
    let decoy = vec![FileInput {
        path: "repack-decoy.bin".into(),
        kind: Kind::File,
        data: Bytes::from(noise(&mut rng, 300)),
    }];
    let (mut decoy_entries, mut moved, ru) = repo.prepare_files_moving(
        &decoy,
        &HashMap::new(),
        &[(repacked.clone(), Bytes::from(files[1].1.clone()))],
    );
    let re = moved.remove(0);
    let decoy_entry = decoy_entries.remove(0);
    let (head, _) = repo.read_ref(BRANCH).await.unwrap().unwrap();
    repo.commit_opts(
        request(
            Some(head),
            [
                (re.to.path.clone(), Some(re.to.clone())),
                (decoy_entry.path.clone(), Some(decoy_entry.clone())),
            ]
            .into(),
        ),
        CommitOptions {
            uploads: ru,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // Now the user's commit lands on the newer head: no conflict, its own file is there, the
    // two relocations of paths the head changed or moved are dropped, the other two applied.
    let delta = entries
        .into_iter()
        .map(|e| (e.path.clone(), Some(e)))
        .collect();
    let out = repo
        .commit_opts(
            request(Some(base), delta),
            CommitOptions {
                relocs,
                uploads,
                ..Default::default()
            },
        )
        .await
        .expect("a relocation must not conflict");
    let _ = out;
    let after = tip_entries(&repo).await;
    let data = repo.read_files(&after).await.unwrap();
    let got: BTreeMap<String, Bytes> = after.iter().map(|e| e.path.clone()).zip(data).collect();
    assert_eq!(got["mine.bin"].as_ref(), mine[0].1.as_slice());
    assert_eq!(
        got[&files[0].0].as_ref(),
        other[0].1.as_slice(),
        "the head's change stays"
    );
    assert_eq!(got[&files[1].0].as_ref(), files[1].1.as_slice());
    assert_eq!(got[&files[2].0].as_ref(), files[2].1.as_slice());
    assert_eq!(got[&files[3].0].as_ref(), files[3].1.as_slice());
    let by_path: HashMap<&str, &Entry> = after.iter().map(|e| (e.path.as_str(), e)).collect();
    let pack_of = |p: &str| match &by_path[p].loc {
        Loc::Pack { pack, .. } => *pack,
        l => panic!("{l:?}"),
    };
    // Applied: the two untouched files moved into the user's pack. Dropped: the repacked one
    // stays where the repack put it, the changed one where its writer put it.
    assert_eq!(pack_of(&files[2].0), pack_of("mine.bin"));
    assert_eq!(pack_of(&files[3].0), pack_of("mine.bin"));
    assert_ne!(pack_of(&files[1].0), pack_of("mine.bin"));
    assert_ne!(pack_of(&files[0].0), pack_of("mine.bin"));
}

#[tokio::test]
async fn a_conflict_is_still_the_users_own() {
    let (_d, _url, repo) = temp_store();
    let files = history(&repo, 2).await;
    let (base, _) = repo.read_ref(BRANCH).await.unwrap().unwrap();
    let mut rng = StdRng::seed_from_u64(9);
    // The user changes `files[0]` itself and relocates `files[1]`.
    let mine = vec![(files[0].0.clone(), noise(&mut rng, 900))];
    let (entries, relocs, uploads) = plan_move(&repo, &files[1..], &mine).await;
    commit_files(&repo, &[(files[0].0.clone(), noise(&mut rng, 800))]).await;
    let delta = entries
        .into_iter()
        .map(|e| (e.path.clone(), Some(e)))
        .collect();
    let err = repo
        .commit_opts(
            request(Some(base), delta),
            CommitOptions {
                relocs,
                uploads,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<ConflictError>().is_some());
}

/// Writers racing into combining, each relocating the same files; one changes a relocated
/// path. Every writer lands.
#[tokio::test]
async fn contended_writers_with_relocations_all_land() {
    let (_d, _url, repo) = temp_store();
    let repo = std::sync::Arc::new(repo);
    let files = history(&repo, 6).await;
    let (base, _) = repo.read_ref(BRANCH).await.unwrap().unwrap();
    let mut tasks = vec![];
    for w in 0..8usize {
        let repo = repo.clone();
        let files = files.clone();
        tasks.push(tokio::spawn(async move {
            let mut rng = StdRng::seed_from_u64(100 + w as u64);
            let (mine, moved): (Vec<_>, Vec<_>) = if w == 0 {
                // Changes a file the others relocate.
                (
                    vec![(files[0].0.clone(), noise(&mut rng, 600))],
                    files[1..].to_vec(),
                )
            } else {
                (
                    vec![(format!("w{w}.bin"), noise(&mut rng, 600))],
                    files.clone(),
                )
            };
            let (entries, relocs, uploads) = plan_move(&repo, &moved, &mine).await;
            let delta = entries
                .into_iter()
                .map(|e| (e.path.clone(), Some(e)))
                .collect();
            repo.commit_opts(
                request(Some(base), delta),
                CommitOptions {
                    relocs,
                    uploads,
                    ..Default::default()
                },
            )
            .await
        }));
    }
    for t in tasks {
        t.await.unwrap().expect("every writer lands");
    }
    let after = tip_entries(&repo).await;
    let data = repo.read_files(&after).await.unwrap();
    let got: BTreeMap<String, Bytes> = after.iter().map(|e| e.path.clone()).zip(data).collect();
    for (p, d) in &files[1..] {
        assert_eq!(got[p].as_ref(), d.as_slice(), "{p} keeps its content");
    }
    for w in 1..8 {
        assert!(got.contains_key(&format!("w{w}.bin")));
    }
}
