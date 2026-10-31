use std::path::Path;
use std::process::Command;

/// A global config file private to this test process (never the user's).
fn global_config() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap())
        .path()
        .join("config")
}

fn cmd(dir: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_pail"));
    c.args(args)
        .current_dir(dir)
        .env("PAIL_AUTHOR", "Tester <t@example.com>")
        .env_remove("PAIL_CLONE")
        .env_remove("PAIL_STORE")
        .env_remove("PAIL_DIR")
        .env("PAIL_CONFIG_GLOBAL", global_config());
    c
}

fn pail(dir: &Path, args: &[&str]) -> String {
    let out = cmd(dir, args).output().unwrap();
    let s = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "pail {args:?} failed:\n{s}");
    s
}

fn pail_fails(dir: &Path, args: &[&str]) -> String {
    let out = cmd(dir, args).output().unwrap();
    assert!(!out.status.success(), "pail {args:?} should fail");
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

#[test]
fn end_to_end_workflow() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "alice"]);
    pail(&t, &["clone", &store, "bob"]);
    let (a, b) = (t.join("alice"), t.join("bob"));

    // Stage semantics: edits after `add` are not committed until re-added.
    write(&a.join("src/lib.rs"), "v1");
    write(&a.join("README.md"), "hello");
    pail(&a, &["add", "src"]);
    write(&a.join("src/lib.rs"), "v2-unstaged");
    let st = pail(&a, &["status"]);
    assert!(st.contains("staged:\n  A src/lib.rs"), "{st}");
    assert!(st.contains("not staged:\n  M src/lib.rs"), "{st}");
    assert!(st.contains("? README.md"), "{st}");
    pail(&a, &["commit", "-m", "add lib"]);
    assert_eq!(pail(&a, &["cat", "src/lib.rs"]), "v1");
    pail_fails(&a, &["commit", "-m", "empty"]);

    // Bob commits from a stale base on a disjoint path: replayed, not rejected.
    write(&b.join("bob.txt"), "bob");
    pail(&b, &["add", "-A"]);
    let out = pail(&b, &["commit", "-m", "bob"]);
    assert!(out.contains("[main"), "{out}");
    assert_eq!(
        std::fs::read_to_string(b.join("src/lib.rs")).unwrap(),
        "v1",
        "bob picked up alice's file"
    );

    // Alice syncs, then both edit the same file: the stale one conflicts.
    pail(&a, &["restore", "src/lib.rs"]);
    pail(&a, &["sync"]);
    assert_eq!(std::fs::read_to_string(a.join("bob.txt")).unwrap(), "bob");
    write(&a.join("bob.txt"), "alice was here");
    pail(&a, &["commit", "-a", "-m", "alice edits bob.txt"]);
    write(&b.join("bob.txt"), "bob again");
    let err = pail_fails(&b, &["commit", "-a", "-m", "bob edits"]);
    assert!(err.contains("conflict") && err.contains("bob.txt"), "{err}");

    // Branch, switch, land.
    pail(&a, &["switch", "-c", "feature"]);
    write(&a.join("feature.txt"), "f");
    std::fs::remove_file(a.join("README.md")).ok();
    pail(&a, &["add", "-A"]);
    pail(&a, &["commit", "-m", "feature work"]);
    pail(&a, &["switch", "main"]);
    assert!(!a.join("feature.txt").exists());
    pail(&a, &["land", "feature", "--onto", "main"]);
    pail(&a, &["sync"]);
    assert_eq!(std::fs::read_to_string(a.join("feature.txt")).unwrap(), "f");
    let log = pail(&a, &["log", "--oneline"]);
    assert!(
        log.lines().next().unwrap().contains("Land feature"),
        "{log}"
    );
    let branches = pail(&a, &["branch"]);
    assert!(
        branches.contains("* main") && branches.contains("feature"),
        "{branches}"
    );

    // Symlinks and exec bits survive a fresh clone.
    std::os::unix::fs::symlink("feature.txt", a.join("link")).unwrap();
    write(&a.join("run.sh"), "#!/bin/sh\n");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(a.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    pail(&a, &["add", "-A"]);
    pail(&a, &["commit", "-m", "link + exec"]);
    pail(&t, &["clone", &store, "carol"]);
    let c = t.join("carol");
    assert_eq!(
        std::fs::read_link(c.join("link"))
            .unwrap()
            .to_str()
            .unwrap(),
        "feature.txt"
    );
    assert_eq!(
        std::fs::metadata(c.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert!(pail(&c, &["status"]).contains("clean"));
}

#[test]
fn gc_removes_only_unreachable() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "w"]);
    let w = t.join("w");
    write(&w.join("keep.txt"), &"k".repeat(500));
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "keep"]);
    pail(&w, &["switch", "-c", "doomed"]);
    write(&w.join("doomed.txt"), &"d".repeat(500));
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "doomed"]);
    pail(&w, &["switch", "main"]);
    // Young garbage survives the grace period; nothing reachable is ever deleted.
    pail(&w, &["branch", "--delete", "doomed"]);
    let out = pail(&w, &["gc"]);
    assert!(out.contains("deleted 0 object(s)"), "{out}");
    let out = pail(&w, &["gc", "--grace-hours", "0"]);
    assert!(!out.contains("deleted 0 object(s)"), "{out}");
    pail(&t, &["clone", &store, "after"]);
    assert_eq!(
        std::fs::read_to_string(t.join("after/keep.txt")).unwrap(),
        "k".repeat(500)
    );
    assert!(!t.join("after/doomed.txt").exists());
}

/// Where a repository keeps a file's contents.
fn blob_path(repo: &Path, content: &[u8]) -> std::path::PathBuf {
    let hex = pail::hash::Hash::of(content).hex();
    repo.join("blobs").join(&hex[..2]).join(hex)
}

fn mtime(p: &Path) -> std::time::SystemTime {
    std::fs::symlink_metadata(p).unwrap().modified().unwrap()
}

#[test]
fn worktrees_share_content_and_build_output() {
    use std::io::{Seek, Write};
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "a"]);
    let a = t.join("a");

    let lib = "pub fn f() {}\n".repeat(100);
    let big: Vec<u8> = (0..3u32 << 20)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    write(&a.join("src/lib.rs"), &lib);
    write(&a.join("README.md"), &"readme ".repeat(20));
    write(&a.join("tiny"), "inline");
    std::fs::write(a.join("big.bin"), &big).unwrap();
    write(
        &a.join("run.sh"),
        &format!("#!/bin/sh\n{}", "echo hi\n".repeat(20)),
    );
    std::fs::set_permissions(a.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("src/lib.rs", a.join("link")).unwrap();
    write(&a.join(".gitignore"), "target/\n.env\nscratch/\n");
    pail(&a, &["add", "-A"]);
    pail(&a, &["commit", "-m", "base"]);
    // Ignored build output and env file, as a build would leave them.
    write(&a.join("target/debug/app"), &"binary".repeat(1000));
    write(&a.join(".env"), "SECRET=1");
    write(&a.join("scratch/huge.log"), "ignored but not build output");
    // Build tools compare mtimes: sources older than outputs mean "up to date".
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    std::fs::File::options()
        .write(true)
        .open(a.join("src/lib.rs"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    // An unstaged edit in the donor: the new worktree gets the base version with a fresh mtime.
    write(&a.join("README.md"), "edited in a");
    let started = std::time::SystemTime::now() - std::time::Duration::from_secs(2);

    let out = pail(&a, &["worktree", "add", "../b", "-b", "feat"]);
    assert!(
        out.contains("seeded 2 ignored path(s)") && out.contains(".env") && out.contains("target"),
        "{out}"
    );
    let b = t.join("b");
    assert_eq!(std::fs::read_to_string(b.join("src/lib.rs")).unwrap(), lib);
    assert_eq!(std::fs::read(b.join("big.bin")).unwrap(), big);
    assert_eq!(
        std::fs::read_to_string(b.join("README.md")).unwrap(),
        "readme ".repeat(20)
    );
    assert_eq!(
        std::fs::read_link(b.join("link"))
            .unwrap()
            .to_str()
            .unwrap(),
        "src/lib.rs"
    );
    assert_eq!(
        std::fs::metadata(b.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(b.join("src/lib.rs"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
    assert_eq!(
        std::fs::read_to_string(b.join("target/debug/app")).unwrap(),
        "binary".repeat(1000)
    );
    assert_eq!(std::fs::read_to_string(b.join(".env")).unwrap(), "SECRET=1");
    assert!(
        !b.join("scratch").exists(),
        "only build output and env files are seeded by default"
    );
    assert_eq!(
        mtime(&b.join("src/lib.rs")),
        old,
        "identical file keeps the donor's mtime"
    );
    assert!(
        mtime(&b.join("README.md")) >= started,
        "differing file gets a fresh mtime"
    );
    let repo = a.join(".pail");
    assert!(blob_path(&repo, lib.as_bytes()).exists() && blob_path(&repo, &big).exists());
    assert_eq!(
        std::fs::read_to_string(b.join(".pail/repo")).unwrap(),
        repo.to_str().unwrap(),
        "linked to a's repository"
    );
    if cfg!(target_vendor = "apple") {
        assert!(out.contains("MB cloned"), "APFS should clone: {out}");
    }
    let st = pail(&b, &["status"]);
    assert!(st.contains("on feat") && st.contains("clean"), "{st}");

    // Editing a clone in place touches neither the blob store nor the donor.
    let mut f = std::fs::File::options()
        .write(true)
        .open(b.join("src/lib.rs"))
        .unwrap();
    f.seek(std::io::SeekFrom::Start(0)).unwrap();
    f.write_all(b"XXXX").unwrap();
    drop(f);
    let mut f = std::fs::File::options()
        .write(true)
        .open(b.join("target/debug/app"))
        .unwrap();
    f.write_all(b"REBUILT").unwrap();
    drop(f);
    assert_eq!(std::fs::read_to_string(a.join("src/lib.rs")).unwrap(), lib);
    assert_eq!(
        std::fs::read_to_string(a.join("target/debug/app")).unwrap(),
        "binary".repeat(1000)
    );
    let stored = blob_path(&repo, lib.as_bytes());
    assert_eq!(
        pail::hash::Hash::of(&std::fs::read(&stored).unwrap()),
        pail::hash::Hash::of(lib.as_bytes())
    );
    assert!(pail(&b, &["status"]).contains("M src/lib.rs"));

    // Committed content goes into the blob store for the next worktree to clone.
    write(&b.join("new.txt"), &"fresh content ".repeat(10));
    pail(&b, &["add", "new.txt"]);
    pail(&b, &["commit", "-m", "new"]);
    assert!(blob_path(&repo, "fresh content ".repeat(10).as_bytes()).exists());

    // Without clone support, the same result by copying.
    let out = cmd(&a, &["worktree", "add", "../c", "--branch", "feat"])
        .env("PAIL_CLONE", "off")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.contains("0 KB cloned") && out.contains("0 cloned, 2 copied"),
        "{out}"
    );
    let c = t.join("c");
    assert_eq!(
        std::fs::read_to_string(c.join("new.txt")).unwrap(),
        "fresh content ".repeat(10)
    );
    assert_eq!(std::fs::read(c.join("big.bin")).unwrap(), big);
    assert_eq!(
        std::fs::read_to_string(c.join("target/debug/app")).unwrap(),
        "binary".repeat(1000)
    );

    let list = pail(&a, &["worktree", "list"]);
    assert!(list.contains(&format!("* {}", a.display())), "{list}");
    assert!(
        list.contains("feat") && list.contains(&c.display().to_string()),
        "{list}"
    );
    let err = pail_fails(&a, &["worktree", "remove", "../b"]);
    assert!(err.contains("uncommitted"), "{err}");
    pail(&a, &["worktree", "remove", "../b", "--force"]);
    pail(&a, &["worktree", "remove", "../c"]);
    assert!(!b.exists() && !c.exists());
    let list = pail(&a, &["worktree", "list"]);
    assert!(!list.contains(&c.display().to_string()), "{list}");
}

/// Read commands work on a bare store (a plain directory path) and inside a working copy,
/// with Git's log/show/ls-tree/rev-parse interfaces.
#[test]
fn bare_store_inspection() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let bare = t.join("bare.pail");
    let bare_s = bare.to_str().unwrap();
    pail(&t, &["init", bare_s, "--branch", "trunk"]);
    pail(&t, &["clone", bare_s, "w", "--branch", "trunk"]);
    let w = t.join("w");
    write(&w.join("src/lib.rs"), "fn a() {}\n");
    write(&w.join("README.md"), "hello\n");
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "first\n\nbody line"]);
    write(&w.join("src/lib.rs"), "fn a() {}\nfn b() {}\n");
    write(&w.join("src/util/mod.rs"), "pub mod x;\n");
    pail(&w, &["commit", "-a", "-m", "second"]);
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "third: util"]);

    let bare_cmd = |args: &[&str]| -> String {
        let mut full = vec!["--store", bare_s];
        full.extend_from_slice(args);
        pail(&t, &full)
    };
    // Bare HEAD is the store's default branch.
    let subjects = bare_cmd(&["log", "--format=%s"]);
    assert_eq!(
        subjects,
        "third: util\nsecond\nfirst\nInitial empty commit\n"
    );
    assert_eq!(
        pail(&w, &["log", "--format=%s", "-2"]),
        "third: util\nsecond\n"
    );
    let medium = bare_cmd(&["log", "-1", "--skip=2"]);
    assert!(
        medium.starts_with("commit ")
            && medium.contains("Author: Tester <t@example.com>\nDate:   "),
        "{medium}"
    );
    assert!(
        medium.ends_with("\n\n    first\n    \n    body line\n"),
        "{medium:?}"
    );
    assert_eq!(
        bare_cmd(&["log", "--format=%s", "--", "src/util"]),
        "third: util\n"
    );
    let ns = bare_cmd(&["log", "--format=%s", "--name-status", "-1", "HEAD~1"]);
    assert_eq!(ns, "second\n\nM\tsrc/lib.rs\n");
    let patch = bare_cmd(&["show", "--format=%s", "HEAD~1"]);
    assert!(
        patch.contains("@@ -1 +1,2 @@\n fn a() {}\n+fn b() {}\n"),
        "{patch}"
    );
    // Git object ids computed from pail content.
    assert_eq!(
        bare_cmd(&["rev-parse", "HEAD:README.md"]).trim(),
        "ce013625030ba8dba906f756967f9e9ca394464a"
    );
    let tree = bare_cmd(&["ls-tree", "HEAD"]);
    assert_eq!(tree.lines().count(), 2, "{tree}");
    assert!(
        tree.starts_with(
            "100644 blob ce013625030ba8dba906f756967f9e9ca394464a\tREADME.md\n040000 tree "
        ),
        "{tree}"
    );
    assert_eq!(
        bare_cmd(&["cat-file", "-p", "HEAD:src/util/mod.rs"]),
        "pub mod x;\n"
    );
    assert_eq!(bare_cmd(&["cat-file", "-t", "HEAD:src"]), "tree\n");
    // Filesystem-shaped listing and reads.
    assert_eq!(bare_cmd(&["ls", "HEAD:src"]), "lib.rs\nutil/\n");
    assert_eq!(bare_cmd(&["ls", "-r", "HEAD~2"]), "README.md\nsrc/lib.rs\n");
    assert_eq!(bare_cmd(&["cat", "HEAD~2:src/lib.rs"]), "fn a() {}\n");
    // Git-style failures.
    let err = pail_fails(&t, &["--store", bare_s, "log", "nope"]);
    assert!(err.starts_with("fatal: ambiguous argument 'nope'"), "{err}");
}

/// Bare reads keep a local content cache (as a bare Git repository keeps its objects):
/// `fetch` fills it, reads are served from it, and `cache prune` trims it.
#[test]
fn content_cache_serves_bare_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let bare = t.join("bare");
    let bare_s = bare.to_str().unwrap();
    pail(&t, &["init", bare_s]);
    pail(&t, &["clone", bare_s, "w"]);
    let w = t.join("w");
    write(&w.join("a.txt"), &"alpha ".repeat(100));
    write(&w.join("b.txt"), &"beta ".repeat(100));
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "one"]);
    write(&w.join("a.txt"), &"alpha two ".repeat(100));
    pail(&w, &["commit", "-a", "-m", "two"]);

    // A bare repository holds the local copies; nothing is cached anywhere else.
    pail(&t, &["clone", "--bare", bare_s, "r.pail"]);
    let r = t.join("r.pail");
    let run = |args: &[&str]| -> std::process::Output {
        let mut full = vec!["--repo", r.to_str().unwrap()];
        full.extend_from_slice(args);
        cmd(&t, &full).output().unwrap()
    };
    let out = run(&["fetch", "--history"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && s.starts_with("fetched 3 file version(s)"),
        "{s}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // With the store's packs gone, contents still come from the cache.
    std::fs::remove_dir_all(bare.join("packs")).unwrap();
    let out = run(&["log", "-p", "--format=%s"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && s.contains("+alpha two alpha two") && s.contains("+beta beta"),
        "{s}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&run(&["cat", "HEAD~1:a.txt"]).stdout),
        "alpha ".repeat(100)
    );
    let out = run(&["cache", "prune", "--max-size", "0"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("removed 3 file(s)"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !run(&["cat", "a.txt"]).status.success(),
        "pruned content must be refetched, and the packs are gone"
    );
    // Direct store reads keep nothing locally.
    let out = cmd(&t, &["--store", bare_s, "fetch"]).output().unwrap();
    assert!(!out.status.success(), "fetch needs a local repository");
}

/// With the commit graph on, history reads print exactly what they print without it, and
/// commits made after the graph was built are picked up.
#[test]
fn commit_graph_matches_uncached_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let bare = t.join("bare");
    let bare_s = bare.to_str().unwrap();
    pail(&t, &["init", bare_s]);
    pail(&t, &["clone", bare_s, "w"]);
    let w = t.join("w");
    let commit = |i: usize| {
        write(&w.join(format!("f{}.txt", i % 4)), &format!("v{i}\n"));
        pail(&w, &["add", "-A"]);
        pail(&w, &["commit", "-m", &format!("c{i}\n\nbody {i}")]);
    };
    for i in 0..6 {
        commit(i);
    }
    pail(&t, &["clone", "--bare", bare_s, "r.pail"]);
    let r = t.join("r.pail");
    // Through the bare repository (commit graph, caches) vs straight from the store.
    let read = |local: bool, args: &[&str]| -> String {
        let mut full = if local {
            vec!["--repo", r.to_str().unwrap()]
        } else {
            vec!["--store", bare_s]
        };
        full.extend_from_slice(args);
        let out = cmd(&t, &full).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let cases: [&[&str]; 4] = [
        &["log"],
        &["log", "--oneline", "--name-status"],
        &["log", "-p", "-3", "HEAD~1"],
        &["log", "--format=%h %p %s", "--", "f1.txt"],
    ];
    for args in cases {
        assert_eq!(read(true, args), read(false, args), "{args:?}");
    }
    assert!(
        r.join("graph/commits2.idx").exists(),
        "the repository's commit graph was built"
    );
    // New commits after the graph exists.
    commit(6);
    commit(7);
    for args in cases {
        assert_eq!(
            read(true, args),
            read(false, args),
            "{args:?} after new commits"
        );
    }
    // Abbreviated ids resolve through the graph.
    let short = read(true, &["log", "-1", "--format=%h", "HEAD~3"]);
    assert_eq!(
        read(true, &["log", "-1", "--format=%s", short.trim()]),
        "c4\n"
    );
}

/// Working copies attach to a repository: a bare one, or the first clone's `.pail/`.
#[test]
fn worktrees_attach_to_a_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = t.join("store");
    let store_s = store.to_str().unwrap();
    pail(&t, &["init", store_s]);
    pail(&t, &["clone", store_s, "w"]);
    write(&t.join("w/a.txt"), &"aaaa".repeat(50));
    pail(&t.join("w"), &["add", "-A"]);
    pail(&t.join("w"), &["commit", "-m", "a"]);

    // Bare repository with a linked working copy.
    pail(&t, &["clone", "--bare", store_s, "r.pail"]);
    let r = t.join("r.pail");
    pail(&r, &["worktree", "add", "../from-bare"]);
    assert_eq!(
        std::fs::read_to_string(t.join("from-bare/a.txt")).unwrap(),
        "aaaa".repeat(50)
    );
    assert_eq!(
        std::fs::read_to_string(t.join("from-bare/.pail/repo")).unwrap(),
        r.to_str().unwrap()
    );
    assert!(
        blob_path(&r, "aaaa".repeat(50).as_bytes()).exists(),
        "contents live in the bare repository"
    );
    let list = pail(&r, &["worktree", "list"]);
    assert!(
        list.contains("(bare)") && list.contains("from-bare"),
        "{list}"
    );
    // Commands inside the linked copy use the shared repository.
    assert!(pail(&t.join("from-bare"), &["log", "--format=%s"]).starts_with("a\n"));

    // The first clone holds the repository its linked copies use: it can't go first.
    pail(&t.join("w"), &["worktree", "add", "../linked"]);
    let err = pail_fails(&t, &["worktree", "remove", "w"]);
    assert!(err.contains("holds the repository"), "{err}");
    pail(&t.join("w"), &["worktree", "remove", "../linked"]);
    pail(&r, &["worktree", "remove", "../from-bare"]);
    assert!(!pail(&r, &["worktree", "list"]).contains("from-bare"));
}

/// A stack of commits on a branch lands on main commit by commit: a fast-forward when main
/// hasn't moved, a replay on the new head when it has, and nothing at all on a conflict.
#[test]
fn stacked_commits_land_on_main() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = t.join("store");
    let s = store.to_str().unwrap();
    pail(&t, &["init", s]);
    pail(&t, &["clone", s, "a"]);
    pail(&t, &["clone", s, "b"]);
    let (a, b) = (t.join("a"), t.join("b"));
    let read = |args: &[&str]| -> String {
        let mut full = vec!["--store", s];
        full.extend_from_slice(args);
        pail(&t, &full)
    };
    let commit = |dir: &Path, msg: &str| {
        pail(dir, &["add", "-A"]);
        pail(dir, &["commit", "-m", msg]);
    };
    write(&a.join("x.txt"), "x0\n");
    write(&a.join("y.txt"), "y0\n");
    commit(&a, "base");

    // 1. A stack on a branch while main stays put: landing fast-forwards (same commit ids).
    pail(&a, &["switch", "-c", "feature"]);
    write(&a.join("f1.txt"), "one\n");
    commit(&a, "s1: add f1");
    write(&a.join("f1.txt"), "one\ntwo\n");
    commit(&a, "s2: extend f1");
    write(&a.join("f2.txt"), "f2\n");
    std::fs::remove_file(a.join("y.txt")).unwrap();
    commit(&a, "s3: add f2, drop y");
    let stack_ids = read(&["log", "--format=%H", "-3", "feature"]);
    let out = pail(&a, &["land", "feature", "--stack"]);
    assert!(
        out.contains("landed 3 commit(s)") && out.contains("fast-forward"),
        "{out}"
    );
    assert_eq!(
        read(&["log", "--format=%H", "-3", "main"]),
        stack_ids,
        "the stack's own commits"
    );
    assert_eq!(
        read(&["log", "--format=%s", "main"]),
        "s3: add f2, drop y\ns2: extend f1\ns1: add f1\nbase\nInitial empty commit\n"
    );

    // 2. Main moves meanwhile (disjoint paths): the stack is replayed on the new head.
    pail(&a, &["switch", "main"]);
    pail(&a, &["sync"]);
    pail(&a, &["switch", "-c", "feature2"]);
    write(&a.join("g.txt"), "g1\n");
    commit(&a, "t1: add g");
    write(&a.join("g.txt"), "g1\ng2\n");
    commit(&a, "t2: extend g");
    let before = read(&["log", "--format=%s|%an|%ad", "-2", "feature2"]);
    let diffs_before = read(&["log", "--format=%s", "--name-status", "-2", "feature2"]);
    pail(&b, &["sync"]);
    write(&b.join("other.txt"), "theirs\n");
    commit(&b, "main moved");
    let out = pail(&a, &["land", "feature2", "--stack"]);
    assert!(
        out.contains("landed 2 commit(s)") && out.contains("replayed"),
        "{out}"
    );
    assert_eq!(
        read(&["log", "--format=%s", "-4", "main"]),
        "t2: extend g\nt1: add g\nmain moved\ns3: add f2, drop y\n"
    );
    assert_eq!(
        read(&["log", "--format=%s|%an|%ad", "-2", "main"]),
        before,
        "authors, dates and messages kept"
    );
    assert_eq!(
        read(&["log", "--format=%s", "--name-status", "-2", "main"]),
        diffs_before,
        "each commit changes what it changed"
    );
    assert_eq!(read(&["cat", "main:g.txt"]), "g1\ng2\n");
    assert_eq!(read(&["cat", "main:other.txt"]), "theirs\n");
    // Landing again is a no-op; a commit added to the stack later lands alone, on top.
    let out = pail(&a, &["land", "feature2", "--stack"]);
    assert!(out.contains("nothing to land"), "{out}");
    write(&a.join("g.txt"), "g1\ng2\ng3\n");
    commit(&a, "t3: extend g again");
    let out = pail(&a, &["land", "feature2", "--stack"]);
    assert!(out.contains("landed 1 commit(s)"), "{out}");
    assert_eq!(
        read(&["log", "--format=%s", "-4", "main"]),
        "t3: extend g again\nt2: extend g\nt1: add g\nmain moved\n"
    );
    assert_eq!(read(&["cat", "main:g.txt"]), "g1\ng2\ng3\n");

    // 3. A stack touching a path main changed meanwhile: conflict, and main doesn't move.
    pail(&a, &["switch", "main"]);
    pail(&a, &["sync"]);
    pail(&a, &["switch", "-c", "feature3"]);
    write(&a.join("h.txt"), "h\n");
    commit(&a, "u1: add h");
    write(&a.join("x.txt"), "x mine\n");
    commit(&a, "u2: edit x");
    pail(&b, &["sync"]);
    write(&b.join("x.txt"), "x theirs\n");
    commit(&b, "main edits x");
    let main_before = read(&["log", "--format=%H", "-1", "main"]);
    let err = pail_fails(&a, &["land", "feature3", "--stack"]);
    assert!(err.contains("conflict") && err.contains("x.txt"), "{err}");
    assert_eq!(
        read(&["log", "--format=%H", "-1", "main"]),
        main_before,
        "nothing landed"
    );
    assert!(
        !read(&["ls", "-r", "main"]).contains("h.txt"),
        "not even the commits before the conflicting one"
    );
}

/// Git-style config: identity from config (required), local over global, and the default
/// branch from `init.defaultBranch` / the store's HEAD instead of an assumed `main`.
#[test]
fn config_identity_and_default_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let global = t.join("global-config");
    let run = |dir: &Path, args: &[&str]| -> std::process::Output {
        cmd(dir, args)
            .env_remove("PAIL_AUTHOR")
            .env("PAIL_CONFIG_GLOBAL", &global)
            .output()
            .unwrap()
    };
    let ok = |dir: &Path, args: &[&str]| -> String {
        let o = run(dir, args);
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let store = t.join("store");
    let s = store.to_str().unwrap();
    // No identity: refuse, as Git does.
    let o = run(&t, &["init", s]);
    assert!(
        !o.status.success()
            && String::from_utf8_lossy(&o.stderr).contains("Please tell me who you are")
    );
    ok(&t, &["config", "--global", "user.name", "Ada Lovelace"]);
    ok(&t, &["config", "--global", "user.email", "ada@example.com"]);
    ok(&t, &["config", "--global", "init.defaultBranch", "trunk"]);
    assert_eq!(ok(&t, &["config", "user.name"]), "Ada Lovelace\n");
    assert!(
        std::fs::read_to_string(&global).unwrap().contains("[user]"),
        "Git-format file"
    );

    // init.defaultBranch names the first branch; clone checks out the store's default.
    let out = ok(&t, &["init", s]);
    assert!(out.contains("(trunk @"), "{out}");
    ok(&t, &["clone", s, "w"]);
    let w = t.join("w");
    assert!(ok(&w, &["status"]).starts_with("on trunk"));
    assert_eq!(ok(&w, &["branch", "--default"]), "trunk\n");
    // Local identity overrides global; commits carry it.
    ok(&w, &["config", "user.name", "Grace Hopper"]);
    assert!(
        ok(&w, &["config", "--list", "--show-scope"]).contains("local\tuser.name=Grace Hopper")
    );
    write(&w.join("a.txt"), "a\n");
    ok(&w, &["add", "-A"]);
    ok(&w, &["commit", "-m", "a"]);
    assert_eq!(
        ok(&w, &["log", "-1", "--format=%an <%ae>|%cn"]),
        "Grace Hopper <ada@example.com>|Grace Hopper\n"
    );
    // land defaults onto the store's default branch.
    ok(&w, &["switch", "-c", "feature"]);
    write(&w.join("b.txt"), "b\n");
    ok(&w, &["add", "-A"]);
    ok(&w, &["commit", "-m", "b"]);
    let out = ok(&w, &["land", "feature", "--stack"]);
    assert!(out.contains("onto trunk"), "{out}");
    // A new default branch is what later clones check out.
    ok(&w, &["branch", "--default", "feature"]);
    ok(&t, &["clone", s, "w2"]);
    assert!(ok(&t.join("w2"), &["status"]).starts_with("on feature"));
    // log options from config, overridden by flags.
    ok(&w, &["config", "log.date", "short"]);
    let d = ok(&w, &["log", "-1", "--format=%ad"]);
    assert!(d.trim().len() == 10 && d.contains('-'), "{d}");
    assert!(
        ok(&w, &["log", "-1", "--format=%ad", "--date=unix"])
            .trim()
            .parse::<u64>()
            .is_ok()
    );
    assert!(ok(&w, &["config", "--unset", "log.date"]).is_empty());
    assert!(!run(&w, &["config", "log.date"]).status.success());
}

/// `pail inspect` serves the object graph's API: overview, commits, pages, files with the
/// physical path to their bytes, and packs.
#[test]
fn inspect_serves_the_object_graph() {
    use std::io::{Read, Write};
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = t.join("store");
    let s = store.to_str().unwrap();
    pail(&t, &["init", s]);
    pail(&t, &["clone", s, "w"]);
    let w = t.join("w");
    write(&w.join("src/lib.rs"), &"fn a() {}\n".repeat(30));
    write(&w.join("tiny"), "x");
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "files"]);
    let port = 20000 + (std::process::id() % 20000) as u16;
    let addr = format!("127.0.0.1:{port}");
    let mut child = cmd(&w, &["inspect", "--no-open", "--addr", &addr])
        .spawn()
        .unwrap();
    let get = |path: &str| -> serde_json::Value {
        for _ in 0..100 {
            if let Ok(mut c) = std::net::TcpStream::connect(&addr) {
                write!(c, "GET {path} HTTP/1.0\r\nHost: x\r\n\r\n").unwrap();
                let mut body = String::new();
                c.read_to_string(&mut body).unwrap();
                let json = body.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                return serde_json::from_str(&json).unwrap_or(serde_json::Value::String(body));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("inspect did not start");
    };
    let o = get("/api/overview");
    assert_eq!(o["default_branch"], "main");
    let tip = o["refs"][0]["commit"].as_str().unwrap().to_string();
    let c = get(&format!("/api/commit/{tip}"));
    assert_eq!(c["changes"].as_array().unwrap().len(), 2);
    let f = get("/api/file?path=src/lib.rs");
    assert_eq!(f["entry"]["loc"]["type"], "pack");
    assert_eq!(
        f["spine"].as_array().unwrap().last().unwrap()["level"],
        0,
        "the descent ends at a leaf page"
    );
    assert!(f["preview"].as_str().unwrap().starts_with("fn a()"));
    let pack = f["entry"]["loc"]["pack"].as_str().unwrap();
    let p = get(&format!("/api/pack/{pack}"));
    assert!(
        p["blobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["paths"][0] == "src/lib.rs")
    );
    assert_eq!(get("/api/file?path=tiny")["entry"]["loc"]["type"], "inline");
    // Logical → physical: the pages and packs behind a folder.
    let cover = get("/api/cover?path=src");
    assert_eq!(cover["files"], 1);
    assert!(
        cover["storage"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["kind"] == "pack")
    );
    assert_eq!(
        cover["pages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["level"] == 0)
            .count(),
        1
    );
    let page = get(&format!("/api/page/{}", c["root"].as_str().unwrap()));
    assert!(page["entries"].is_array() || page["children"].is_array());
    child.kill().unwrap();
}

fn hook(dir: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    write(&p, &format!("#!/bin/bash\nset -euo pipefail\n{body}\n"));
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The hook patterns firstlanding's archdev workflow relies on, in pail's model: a checked-in
/// hooks dir (core.hooksPath), a formatting pre-commit that restages what it formats, a
/// refusing pre-commit, commit-msg trailers, pre-publish seeing the exact commit before it is
/// published, post-checkout/post-sync with ORIG_HEAD, post-rewrite on replays and landings,
/// --no-verify, exit code 3 and the trace file.
#[test]
fn hooks_follow_the_commit_and_publish_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "alice"]);
    let a = t.join("alice");
    let log = t.join("hooks.log");
    let trace = t.join("trace.jsonl");
    let hooks = a.join(".pailhooks");
    let l = log.display();
    // A formatter: upper-cases staged files and restages them, refuses `bad.txt`.
    hook(
        &hooks,
        "pre-commit",
        &format!(
            r#"
echo "pre-commit $PAIL_HOOK $PAIL_HOOK_BRANCH dir=${{PAIL_DIR:-unset}} cwd=$(pwd)" >> {l}
while IFS= read -r -d '' f; do
  [[ "$f" == bad.txt ]] && {{ echo "bad.txt is not allowed" >&2; exit 1; }}
  tr a-z A-Z < "$f" > "$f.tmp" && mv "$f.tmp" "$f"
  pail add "$f"
done < <(pail diff --staged --name-only -z --diff-filter=AM)"#
        ),
    );
    hook(
        &hooks,
        "commit-msg",
        &format!("echo commit-msg >> {l}\nprintf '\\nHook-Checked: yes\\n' >> \"$1\""),
    );
    hook(
        &hooks,
        "pre-publish",
        &format!(
            r#"read -r branch old new; echo "pre-publish $1 $2 $branch $old $new" >> {l}
[[ "${{BLOCK_PUBLISH:-0}}" != 1 ]]"#
        ),
    );
    hook(&hooks, "post-commit", &format!("echo post-commit >> {l}"));
    hook(
        &hooks,
        "post-checkout",
        &format!(
            "echo \"post-checkout $1 $2 $3 orig=$(pail rev-parse ORIG_HEAD 2>/dev/null || echo none)\" >> {l}"
        ),
    );
    hook(
        &hooks,
        "post-sync",
        &format!("echo \"post-sync $1 $2\" >> {l}"),
    );
    hook(
        &hooks,
        "post-rewrite",
        &format!("echo \"post-rewrite $1 $(cat | tr '\\n' ';')\" >> {l}"),
    );
    hook(&hooks, "_helper", "exit 1"); // not a hook name: never run
    pail(&a, &["config", "core.hooksPath", ".pailhooks"]);
    // Checked in, like firstlanding's .githooks.
    pail(&a, &["add", ".pailhooks"]);
    pail(&a, &["commit", "--no-verify", "-m", "hooks"]);
    std::fs::write(&log, "").unwrap();
    let read_log = || std::fs::read_to_string(&log).unwrap_or_default();
    let clear_log = || std::fs::write(&log, "").unwrap();

    // 1. pre-commit formats and restages; commit-msg adds a trailer; pre-publish sees the
    //    exact commit that is then published.
    write(&a.join("src/a.txt"), "hello");
    pail(&a, &["add", "src"]);
    let out = cmd(&a, &["commit", "-m", "add a"])
        .env("PAIL_TRACE_HOOKS", &trace)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(pail(&a, &["cat", "src/a.txt"]), "HELLO");
    assert_eq!(
        std::fs::read_to_string(a.join("src/a.txt")).unwrap(),
        "HELLO"
    );
    assert!(pail(&a, &["status"]).contains("nothing to commit"));
    let msg = pail(&a, &["log", "-1", "--format=%B"]);
    assert!(msg.contains("add a\n\nHook-Checked: yes"), "{msg}");
    let head = pail(&a, &["rev-parse", "HEAD"]).trim().to_string();
    let h = read_log();
    assert!(
        h.contains(&format!(
            "pre-commit pre-commit main dir=unset cwd={}",
            a.display()
        )),
        "{h}"
    );
    assert!(
        h.contains(&format!(
            "pre-publish main commit main {} {head}",
            "0".repeat(64)
        )) || h.contains(&format!("{head}\n")),
        "{h}"
    );
    let order: Vec<&str> = h.lines().map(|l| l.split(' ').next().unwrap()).collect();
    assert_eq!(
        order,
        ["pre-commit", "commit-msg", "pre-publish", "post-commit"],
        "{h}"
    );
    let tr = std::fs::read_to_string(&trace).unwrap();
    assert!(
        tr.lines()
            .any(|l| l.contains(r#""hook":"pre-commit""#) && l.contains(r#""exit":0"#)),
        "{tr}"
    );

    // 2. A refusing pre-commit: exit code 3, nothing published, the stage kept.
    write(&a.join("bad.txt"), "x");
    pail(&a, &["add", "bad.txt"]);
    let out = cmd(&a, &["commit", "-m", "bad"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("bad.txt is not allowed"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("pre-commit hook refused (exit 1)"));
    assert_eq!(pail(&a, &["rev-parse", "HEAD"]).trim(), head);
    assert!(pail(&a, &["status"]).contains("A bad.txt"));

    // 3. A refusing pre-publish: the commit was written but never published.
    pail(&a, &["unstage", "bad.txt"]);
    std::fs::remove_file(a.join("bad.txt")).unwrap();
    write(&a.join("src/b.txt"), "b");
    pail(&a, &["add", "src/b.txt"]);
    clear_log();
    let out = cmd(&a, &["commit", "-m", "b"])
        .env("BLOCK_PUBLISH", "1")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(pail(&a, &["rev-parse", "main"]).trim(), head);
    assert!(!read_log().contains("post-commit"));

    // 4. --no-verify skips pre-commit, commit-msg and pre-publish (post-commit still runs).
    clear_log();
    let out = cmd(&a, &["commit", "-n", "-m", "b unchecked"])
        .env("BLOCK_PUBLISH", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Formatted and restaged by the pre-commit of the refused attempt above.
    assert_eq!(pail(&a, &["cat", "src/b.txt"]), "B");
    assert_eq!(read_log(), "post-commit\n");
    let head2 = pail(&a, &["rev-parse", "HEAD"]).trim().to_string();

    // 5. post-checkout on switch (with ORIG_HEAD), then on clone and worktree add.
    pail(&a, &["switch", "-c", "feature"]);
    write(&a.join("src/f.txt"), "f");
    pail(&a, &["add", "src/f.txt"]);
    pail(&a, &["commit", "-n", "-m", "f"]);
    let feature = pail(&a, &["rev-parse", "HEAD"]).trim().to_string();
    clear_log();
    pail(&a, &["switch", "main"]);
    assert_eq!(
        read_log(),
        format!("post-checkout {feature} {head2} 1 orig={feature}\n")
    );

    // 6. Another writer moves main: bob's commit is replayed (post-rewrite replay), and
    //    alice's sync reports post-sync with the old and new base.
    pail(&t, &["clone", &store, "bob"]);
    let b = t.join("bob");
    write(&b.join("other.txt"), "o");
    pail(&b, &["add", "other.txt"]);
    pail(&b, &["commit", "-m", "other"]);
    let bob_head = pail(&b, &["rev-parse", "HEAD"]).trim().to_string();
    write(&a.join("src/c.txt"), "c");
    pail(&a, &["add", "src/c.txt"]);
    clear_log();
    pail(&a, &["commit", "-m", "c on a stale base"]);
    let published = pail(&a, &["rev-parse", "HEAD"]).trim().to_string();
    let h = read_log();
    // The commit is built on bob's head before it is checked, so pre-publish saw exactly
    // what was published and nothing was rewritten (a race after the check is covered in
    // tests/repo.rs).
    assert!(
        h.contains(&format!(
            "pre-publish main commit main {bob_head} {published}"
        )),
        "{h}"
    );
    assert!(!h.contains("post-rewrite"), "{h}");
    assert_eq!(pail(&a, &["rev-parse", "ORIG_HEAD"]).trim(), head2);
    write(&b.join("other2.txt"), "o2");
    pail(&b, &["add", "other2.txt"]);
    pail(&b, &["commit", "-m", "other2"]);
    let tip = pail(&b, &["rev-parse", "HEAD"]).trim().to_string();
    clear_log();
    pail(&a, &["sync"]);
    assert_eq!(read_log(), format!("post-sync {published} {tip}\n"));

    // 7. land --stack runs pre-publish (mode land) once and reports the rewrite.
    clear_log();
    pail(&a, &["land", "feature", "--stack", "--onto", "main"]);
    let h = read_log();
    assert!(
        h.contains(&format!("pre-publish main land main {tip} ")),
        "{h}"
    );
    assert!(h.contains(&format!("post-rewrite land {feature} ")), "{h}");
    // A squash land is guarded the same way.
    pail(&a, &["switch", "-c", "squash"]);
    write(&a.join("src/s.txt"), "s");
    pail(&a, &["add", "src/s.txt"]);
    pail(&a, &["commit", "-n", "-m", "s"]);
    let main_tip = pail(&a, &["rev-parse", "main"]).trim().to_string();
    let out = cmd(&a, &["land", "squash", "--onto", "main"])
        .env("BLOCK_PUBLISH", "1")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(pail(&a, &["rev-parse", "main"]).trim(), main_tip);
    pail(&a, &["land", "squash", "--onto", "main", "--no-verify"]);
    pail(&a, &["switch", "main"]);

    // 8. Hooks off: an empty core.hooksPath.
    pail(&a, &["config", "core.hooksPath", ""]);
    write(&a.join("bad.txt"), "x");
    pail(&a, &["add", "bad.txt"]);
    pail(&a, &["commit", "-m", "hooks off"]);
}

/// Help, the agent skill, shell completion (with live branch names) and exit codes.
#[test]
fn help_skill_completion_and_exit_codes() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "a"]);
    let a = t.join("a");
    pail(&a, &["branch", "feature-x"]);

    let top = pail(&t, &["--help"]);
    assert!(
        top.contains("Branches and landing") && top.contains("Exit codes:"),
        "{top}"
    );
    assert_eq!(pail(&t, &[]), top, "bare `pail` prints the help");
    let land = pail(&t, &["land", "--help"]);
    assert!(
        land.contains("Examples:") && land.contains("pail land feature --stack"),
        "{land}"
    );
    assert!(pail(&t, &["help", "worktree", "add"]).contains("Usage: pail worktree add"));
    assert!(pail(&a, &["log", "--help"]).contains("Usage: pail log"));
    let bare_group = cmd(&t, &["worktree"]).output().unwrap();
    assert!(String::from_utf8_lossy(&bare_group.stderr).contains("Commands:"));

    let skill = pail(&t, &["--skill"]);
    assert!(
        skill.starts_with("---\nname: pail\ndescription: "),
        "{skill}"
    );
    assert!(skill.contains("Commit = publish"));

    for sh in ["bash", "zsh", "fish", "elvish", "powershell"] {
        assert!(pail(&t, &["completion", sh]).contains("COMPLETE"), "{sh}");
    }
    // What the registered script runs on <TAB>: live branch names with their commits.
    let out = cmd(&a, &["--", "pail", "switch", "fea"])
        .env("COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", "2")
        .env("_CLAP_IFS", "\n")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "feature-x");
    let out = cmd(&a, &["--", "pail", "config", "core.ho"])
        .env("COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", "2")
        .env("_CLAP_IFS", "\n")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "core.hooksPath"
    );

    // Exit code 4: another writer changed the same path since our base.
    pail(&t, &["clone", &store, "b"]);
    let b = t.join("b");
    write(&a.join("f.txt"), "a");
    pail(&a, &["add", "f.txt"]);
    pail(&a, &["commit", "-m", "a"]);
    write(&b.join("f.txt"), "b");
    pail(&b, &["add", "f.txt"]);
    let out = cmd(&b, &["commit", "-m", "b"]).output().unwrap();
    assert_eq!(out.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&out.stderr).contains("pail sync"));
}

/// Kills the child on drop.
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// People without bucket credentials: a `pail serve` proxy in front of the store, a credential
/// helper handing the client a bearer token, and an auth command deciding per repository and
/// operation. Writes and reads go through; a read-only token cannot publish; unknown tokens
/// cannot read; the helper's answer is cached until it expires.
#[test]
fn proxy_store_with_a_credential_helper() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let auth = t.join("auth.sh");
    write(
        &auth,
        "#!/bin/sh\ncase \"$PAIL_AUTH_TOKEN:$PAIL_AUTH_REPO:$PAIL_AUTH_OP\" in\n  tok-alice:team/app:*) echo alice;;\n  tok-bob:team/app:read) echo bob;;\n  tok-bob:*) exit 3;;\n  *) exit 1;;\nesac\n",
    );
    let calls = t.join("helper-calls");
    // The company's provider, shipped as an extension: `describe`, and `get` answering with
    // the token the user holds (TOK stands in for their login).
    let helper = t.join("pail-credential-acme");
    write(
        &helper,
        &format!(
            "#!/bin/sh\n[ \"$1\" = describe ] && {{ echo acme sso; exit 0; }}\nread req\necho \"$req\" >> {}\necho \"{{\\\"token\\\":\\\"$TOK\\\",\\\"expires_at\\\":4102444800}}\"\n",
            calls.display()
        ),
    );
    for f in [&auth, &helper] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _server = Child(
        cmd(
            &t,
            &[
                "serve",
                "--store",
                &format!("file://{}", t.join("bucket").display()),
                "--addr",
                &format!("127.0.0.1:{port}"),
                "--auth-command",
                &auth.to_string_lossy(),
            ],
        )
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap(),
    );
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let store = format!("{base}/team/app");
    let ext = t.join("extensions");
    let as_user = |dir: &Path, token: &str, args: &[&str]| {
        let mut c = cmd(dir, args);
        c.env("TOK", token)
            .env("XDG_CACHE_HOME", t.join(format!("cache-{token}")))
            .env("PAIL_EXTENSIONS_DIR", &ext);
        c.output().unwrap()
    };
    let ok = |o: std::process::Output| {
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).to_string()
    };

    // Install the provider and point the proxy's URL at it.
    let out = ok(as_user(
        &t,
        "tok-alice",
        &["auth", "install", &helper.to_string_lossy()],
    ));
    assert!(
        out.contains("installed acme") && out.contains("acme sso"),
        "{out}"
    );
    assert!(ok(as_user(&t, "tok-alice", &["auth", "list"])).contains("acme             acme sso"));
    ok(as_user(
        &t,
        "tok-alice",
        &["auth", "use", "acme", "--for", &base, "--global"],
    ));
    ok(as_user(&t, "tok-alice", &["init", &store]));
    ok(as_user(&t, "tok-alice", &["clone", &store, "a"]));
    let a = t.join("a");
    write(&a.join("src/lib.rs"), "fn main() {}");
    ok(as_user(&a, "tok-alice", &["add", "-A"]));
    ok(as_user(
        &a,
        "tok-alice",
        &["commit", "-m", "through the proxy"],
    ));
    assert!(
        t.join("bucket/team/app/refs/main").is_file(),
        "objects land in the backing store"
    );
    let status = ok(as_user(&a, "tok-alice", &["auth", "status"]));
    assert!(status.contains("access    ok"), "{status}");

    // Read-only: clone and log work, publishing does not.
    ok(as_user(&t, "tok-bob", &["clone", &store, "b"]));
    let b = t.join("b");
    assert!(ok(as_user(&b, "tok-bob", &["log", "--oneline"])).contains("through the proxy"));
    write(&b.join("x.txt"), "x");
    ok(as_user(&b, "tok-bob", &["add", "-A"]));
    let out = as_user(&b, "tok-bob", &["commit", "-m", "nope"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("403"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Unknown token: nothing.
    let out = as_user(&t, "tok-eve", &["clone", &store, "e"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("401"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The helper got the pail-credential/1 request, and its answer was reused from the cache.
    let reqs = std::fs::read_to_string(&calls).unwrap();
    assert!(
        reqs.contains(r#""protocol":"pail-credential/1""#) && reqs.contains(&store),
        "{reqs}"
    );
    let alice_calls = reqs.lines().count();
    ok(as_user(&a, "tok-alice", &["log", "-1"]));
    assert_eq!(
        std::fs::read_to_string(&calls).unwrap().lines().count(),
        alice_calls,
        "cached"
    );
    assert!(ok(as_user(&a, "tok-alice", &["auth", "logout"])).contains("forgot"));

    // A provider whose cached token the store stops accepting (it claimed a far expiry):
    // the 401 drops the cache, the provider is asked again, and the command succeeds.
    let calls = t.join("rot-calls");
    let rot = t.join("pail-credential-rot");
    write(
        &rot,
        &format!(
            "#!/bin/sh\ncat >/dev/null\necho x >> {c}\nif [ $(wc -l < {c}) -eq 1 ]; then T=tok-stale; else T=tok-alice; fi\necho \"{{\\\"token\\\":\\\"$T\\\",\\\"expires_at\\\":4102444800}}\"\n",
            c = calls.display()
        ),
    );
    ok(as_user(
        &t,
        "tok-alice",
        &["auth", "install", &rot.to_string_lossy()],
    ));
    let out = cmd(&a, &["sync"])
        .env("PAIL_EXTENSIONS_DIR", &ext)
        .env("PAIL_CREDENTIAL_PROVIDER", "rot")
        .env("XDG_CACHE_HOME", t.join("cache-rot"))
        .output()
        .unwrap();
    assert!(ok(out).contains("main @"));
    assert_eq!(
        std::fs::read_to_string(&calls).unwrap().lines().count(),
        2,
        "asked once more after the 401"
    );
}

/// Concurrent writers through the CLI, each in its own working copy, committing in a loop on
/// one branch: every commit any of them reported is on main (none lost to a swap from a
/// remembered head, none landed twice), whether it landed directly or in a combined batch.
#[test]
fn concurrent_cli_writers_lose_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    let (writers, per) = (8usize, 10usize);
    for w in 0..writers {
        pail(&t, &["clone", &store, &format!("w{w}")]);
    }
    let handles: Vec<_> = (0..writers)
        .map(|w| {
            let dir = t.join(format!("w{w}"));
            std::thread::spawn(move || {
                for i in 0..per {
                    write(&dir.join(format!("w{w}/f{i}.txt")), &format!("{w}-{i}"));
                    pail(&dir, &["add", "-A"]);
                    pail(&dir, &["commit", "-m", &format!("w{w} c{i}")]);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    pail(&t, &["clone", &store, "check"]);
    let log = pail(&t.join("check"), &["log", "--format=%s"]);
    let subjects: Vec<&str> = log.lines().collect();
    for w in 0..writers {
        for i in 0..per {
            let want = format!("w{w} c{i}");
            assert_eq!(
                subjects.iter().filter(|s| **s == want).count(),
                1,
                "{want} must be on main exactly once"
            );
        }
    }
    let files = pail(&t.join("check"), &["ls", "-r"]);
    assert_eq!(
        files.lines().filter(|l| l.contains("/f")).count(),
        writers * per
    );
}

/// `put --expect PATH=ID` is a compare-and-swap on one path: it commits only if the path is as
/// the caller read it (exit 4 otherwise), and a change to other paths in between does not
/// matter.
#[test]
fn put_expect_is_a_cas() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    let put = |path: &str, body: &str, extra: &[&str]| {
        let src = t.join("src.json");
        write(&src, body);
        let mut args = vec![
            "--store".to_string(),
            store.clone(),
            "put".into(),
            "_meta".into(),
            "-m".into(),
            "m".into(),
            "--file".into(),
            format!("{path}={}", src.display()),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        let args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        cmd(&t, &args).output().unwrap().status.code()
    };
    let id_of = |path: &str| {
        let out = cmd(&t, &["--store", &store, "cat-file", "--batch-check"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        out.stdin
            .as_ref()
            .unwrap()
            .write_all(format!("_meta:{path}\n").as_bytes())
            .unwrap();
        let o = out.wait_with_output().unwrap();
        String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        put("prs/1.json", "v1", &["--expect", "prs/1.json=absent"]),
        Some(0)
    );
    assert_eq!(
        put("prs/1.json", "x", &["--expect", "prs/1.json=absent"]),
        Some(4)
    );
    let v1 = id_of("prs/1.json");
    // Another path changes: not a conflict.
    assert_eq!(put("prs/2.json", "other", &["--if-absent"]), Some(0));
    assert_eq!(
        put(
            "prs/1.json",
            "v2",
            &["--expect", &format!("prs/1.json={v1}")]
        ),
        Some(0)
    );
    // v1 is stale now.
    assert_eq!(
        put(
            "prs/1.json",
            "v3",
            &["--expect", &format!("prs/1.json={v1}")]
        ),
        Some(4)
    );
    assert_eq!(
        pail(&t, &["--store", &store, "cat", "_meta:prs/1.json"]),
        "v2"
    );
    // An expectation about a path the put does not write is a usage error.
    assert_eq!(
        put("prs/1.json", "v4", &["--expect", "prs/9.json=absent"]),
        Some(1)
    );
}

/// `land --stack --expect-tip C` lands only if the branch is at C (exit 4 if it moved).
#[test]
fn land_expect_tip() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "w"]);
    let w = t.join("w");
    write(&w.join("a"), "1");
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "base"]);
    pail(&w, &["switch", "-c", "feat"]);
    write(&w.join("a"), "2");
    pail(&w, &["commit", "-am", "first"]);
    let judged = pail(&w, &["rev-parse", "feat"]).trim().to_string();
    write(&w.join("b"), "3");
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "second"]);
    let err = pail_fails(&w, &["land", "feat", "--stack", "--expect-tip", &judged]);
    assert!(err.contains("not the expected"), "{err}");
    let code = cmd(&w, &["land", "feat", "--stack", "--expect-tip", &judged])
        .output()
        .unwrap()
        .status
        .code();
    assert_eq!(code, Some(4));
    assert!(!pail(&w, &["log", "--format=%s", "main"]).contains("first"));
    let tip = pail(&w, &["rev-parse", "feat"]).trim().to_string();
    pail(&w, &["land", "feat", "--stack", "--expect-tip", &tip]);
    assert!(pail(&w, &["log", "--format=%s", "main"]).contains("second"));
}

/// `pail put` commits to a branch without a working copy (creating it), `--if-absent` claims
/// a path once (exit 4 after), and `pail events` reports each ref change once.
#[test]
fn put_and_events() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    let mut events = cmd(
        &t,
        &[
            "--store",
            &store,
            "events",
            "--interval-ms",
            "100",
            "--branch",
            "_*",
        ],
    )
    .stdout(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    write(&t.join("rec.json"), "{\"id\":1}");
    let src = format!("prs/1.json={}", t.join("rec.json").display());
    pail(
        &t,
        &[
            "--store",
            &store,
            "put",
            "_meta",
            "-m",
            "open",
            "--file",
            &src,
            "--if-absent",
        ],
    );
    let again = cmd(
        &t,
        &[
            "--store",
            &store,
            "put",
            "_meta",
            "-m",
            "again",
            "--file",
            &src,
            "--if-absent",
        ],
    )
    .output()
    .unwrap();
    assert_eq!(again.status.code(), Some(4));
    assert_eq!(
        pail(&t, &["--store", &store, "cat", "_meta:prs/1.json"]),
        "{\"id\":1}"
    );
    pail(
        &t,
        &[
            "--store",
            &store,
            "put",
            "_meta",
            "-m",
            "close",
            "--delete",
            "prs/1.json",
        ],
    );
    // main changes too, but only _* branches were asked for.
    pail(
        &t,
        &[
            "--store",
            &store,
            "put",
            "main",
            "-m",
            "x",
            "--file",
            &format!("x.txt={}", t.join("rec.json").display()),
        ],
    );
    std::thread::sleep(std::time::Duration::from_millis(600));
    events.kill().unwrap();
    let out = String::from_utf8(events.wait_with_output().unwrap().stdout).unwrap();
    let lines: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        !lines.is_empty() && lines.iter().all(|e| e["branch"] == "_meta"),
        "{out}"
    );
    let commits: std::collections::BTreeSet<String> =
        lines.iter().map(|e| e["commit"].to_string()).collect();
    assert_eq!(commits.len(), lines.len(), "no duplicate events: {out}");
}

/// The PR and CI extensions (Python, using only pail commands and conventions): CI builds
/// commits from ref events and records checks on _meta; a PR merges only with an approval
/// and its required checks passing, by landing its stack.
#[test]
fn pr_and_ci_extensions() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 not found; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    let ext = Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions");
    let run = |dir: &Path, prog: &str, args: &[&str], author: &str| {
        let out = Command::new(ext.join(prog))
            .args(args)
            .current_dir(dir)
            .env("PAIL_BIN", env!("CARGO_BIN_EXE_pail"))
            .env("PAIL_STORE", &store)
            .env("PAIL_AUTHOR", author)
            .env("PAIL_CONFIG_GLOBAL", global_config())
            .output()
            .unwrap();
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    };
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "w"]);
    let w = t.join("w");
    write(
        &w.join("pail.ci.json"),
        r#"{"checks":{"test":"grep -q hello hello.txt"},"required":["test"]}"#,
    );
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "ci config"]);
    let mut ci = Command::new(ext.join("pail-ci"))
        .args(["run", "--branch", "pr/*"])
        .env("XDG_STATE_HOME", t.join("state"))
        .env("PAIL_BIN", env!("CARGO_BIN_EXE_pail"))
        .env("PAIL_STORE", &store)
        .env("PAIL_AUTHOR", "CI <ci@x>")
        .env("PAIL_CONFIG_GLOBAL", global_config())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    pail(&w, &["switch", "-c", "pr/hello"]);
    write(&w.join("hello.txt"), "hello");
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "say hello"]);
    let (ok, out) = run(
        &w,
        "pail-pr",
        &["open", "pr/hello", "--title", "Say hello"],
        "Alice <a@x>",
    );
    assert!(ok, "{out}");
    // CI records the check from the ref event.
    let mut status = String::new();
    for _ in 0..100 {
        status = run(&w, "pail-ci", &["status", "pr/hello"], "Alice <a@x>").1;
        if status.contains("success") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let _ = ci.kill();
    assert!(
        status.contains("test") && status.contains("success"),
        "{status}"
    );
    // No approval yet: refused. Self-approval does not count.
    run(&w, "pail-pr", &["review", "1", "--approve"], "Alice <a@x>");
    let (ok, out) = run(&w, "pail-pr", &["merge", "1"], "Alice <a@x>");
    assert!(!ok && out.contains("0 of 1 approvals"), "{out}");
    let (ok, out) = run(
        &w,
        "pail-pr",
        &["review", "1", "--approve", "-m", "lgtm"],
        "Bob <b@x>",
    );
    assert!(ok, "{out}");
    let (ok, out) = run(&w, "pail-pr", &["merge", "1"], "Alice <a@x>");
    assert!(ok, "{out}");
    assert_eq!(
        pail(&t, &["--store", &store, "cat", "main:hello.txt"]),
        "hello"
    );
    let (_, show) = run(&w, "pail-pr", &["show", "1"], "Alice <a@x>");
    assert!(show.contains("[merged]") && show.contains("Bob"), "{show}");
}

/// Branch policy enforced by `pail serve`: the auth command names the caller and its groups,
/// the repository's `_policy` branch says who may write what, and every request is checked.
#[test]
fn proxy_enforces_branch_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let bucket = format!("file://{}", t.join("bucket").display());
    // Seed the repository and its policy directly in the bucket (as its owner would).
    let direct = format!("{bucket}/app");
    pail(&t, &["init", &direct]);
    write(
        &t.join("policy.json"),
        r#"{"version":1,"admins":["role:admin"],"branches":{"main":{"writers":["lander"],"approvals":1,"required_checks":["test"]},"_policy":{"writers":["role:admin"]},"*":{"writers":["*"]}}}"#,
    );
    pail(
        &t,
        &[
            "--store",
            &direct,
            "policy",
            "set",
            &t.join("policy.json").to_string_lossy(),
        ],
    );
    let check = cmd(
        &t,
        &[
            "--store",
            &direct,
            "policy",
            "check",
            "--as",
            "user:alice",
            "write",
            "main",
        ],
    )
    .output()
    .unwrap();
    assert_eq!(check.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&check.stdout).contains("main is protected"));
    let req = pail(&t, &["--store", &direct, "policy", "requirements", "main"]);
    assert!(
        req.contains("\"approvals\": 1") && req.contains("\"test\""),
        "{req}"
    );
    // Tokens name callers: tok-alice is alice; tok-lander is the lander.
    let auth = t.join("auth.sh");
    write(
        &auth,
        "#!/bin/sh\ncase \"$PAIL_AUTH_TOKEN\" in\n  tok-alice) echo user:alice; echo group:eng;;\n  tok-lander) echo lander;;\n  *) exit 1;;\nesac\n",
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _server = Child(
        cmd(
            &t,
            &[
                "serve",
                "--store",
                &bucket,
                "--addr",
                &format!("127.0.0.1:{port}"),
                "--auth-command",
                &auth.to_string_lossy(),
            ],
        )
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap(),
    );
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let store = format!("http://127.0.0.1:{port}/app");
    let as_who = |dir: &Path, token: &str, args: &[&str]| {
        cmd(dir, args)
            .env("PAIL_CREDENTIAL_HELPER", format!("echo {token} #"))
            .env("XDG_CACHE_HOME", t.join(format!("cache-{token}")))
            .output()
            .unwrap()
    };
    let ok = |o: std::process::Output| {
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };
    ok(as_who(&t, "tok-alice", &["clone", &store, "a"]));
    let a = t.join("a");
    write(&a.join("x.txt"), "x");
    ok(as_who(&a, "tok-alice", &["add", "-A"]));
    let denied = as_who(&a, "tok-alice", &["commit", "-m", "to main"]);
    assert!(!denied.status.success());
    let err = String::from_utf8_lossy(&denied.stderr);
    assert!(
        err.contains("permission denied") && err.contains("protected"),
        "{err}"
    );
    // Moving the change to a PR branch keeps it (the branch is at the same commit).
    ok(as_who(&a, "tok-alice", &["switch", "-c", "pr/x"]));
    ok(as_who(&a, "tok-alice", &["commit", "-m", "on a pr branch"]));
    // Alice cannot rewrite the policy; the lander can land.
    let open = t.join("open.json");
    write(&open, r#"{"version":1,"branches":{"*":{"writers":["*"]}}}"#);
    assert!(
        !as_who(
            &t,
            "tok-alice",
            &["--store", &store, "policy", "set", &open.to_string_lossy()]
        )
        .status
        .success()
    );
    ok(as_who(
        &t,
        "tok-lander",
        &[
            "--store", &store, "land", "pr/x", "--stack", "--onto", "main",
        ],
    ));
    assert_eq!(pail(&t, &["--store", &direct, "cat", "main:x.txt"]), "x");
}

/// A fake GitHub REST API for `pail auth install`: routes → (status, body). Serves until
/// the test ends; returns its base URL and a log of the paths asked for.
fn fake_github(
    routes: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, (u16, Vec<u8>)>>>,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr().to_ip().unwrap());
    let log = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let l = log.clone();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let url = req.url().to_string();
            let auth = req.headers().iter().any(|h| h.field.equiv("Authorization"));
            l.lock()
                .unwrap()
                .push(format!("{url}{}", if auth { " (auth)" } else { "" }));
            let (code, body) = routes
                .lock()
                .unwrap()
                .get(&url)
                .cloned()
                .unwrap_or((404, br#"{"message":"Not Found"}"#.to_vec()));
            let _ = req.respond(tiny_http::Response::from_data(body).with_status_code(code));
        }
    });
    (base, log)
}

/// Providers live in their own repositories, like gh extensions: `pail auth install <name>`
/// reads ArchAstro/pail-credential-<name>'s manifest at its latest release and installs the
/// script it names; owner/repo@ref pins; binaries come from release assets; upgrade moves
/// unpinned installs to the newest release.
#[test]
fn auth_install_from_github() {
    use std::collections::HashMap;
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let routes = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (api, log) = fake_github(routes.clone());
    let script = |v: &str| {
        format!("#!/bin/sh\n[ \"$1\" = describe ] && echo 'demo {v}' && exit 0\necho tok-{v}\n")
            .into_bytes()
    };
    let set = |rs: &[(&str, u16, Vec<u8>)]| {
        let mut r = routes.lock().unwrap();
        for (k, c, b) in rs {
            r.insert(k.to_string(), (*c, b.clone()));
        }
    };
    let repo = "/repos/ArchAstro/pail-credential-demo";
    let manifest = br#"{"name":"demo","protocol":"pail-credential/1","description":"demo","script":"bin/demo.sh"}"#.to_vec();
    set(&[
        (repo, 200, br#"{"default_branch":"main"}"#.to_vec()),
        (
            &format!("{repo}/releases/latest"),
            200,
            br#"{"tag_name":"v1","assets":[]}"#.to_vec(),
        ),
        (&format!("{repo}/commits/v1"), 200, b"c1".to_vec()),
        (
            &format!("{repo}/contents/pail-extension.json?ref=c1"),
            200,
            manifest.clone(),
        ),
        (
            &format!("{repo}/contents/bin/demo.sh?ref=c1"),
            200,
            script("v1"),
        ),
    ]);
    let run = |args: &[&str]| {
        let o = cmd(t, args)
            .env("PAIL_GITHUB_API", &api)
            .env("PAIL_EXTENSIONS_DIR", t.join("ext"))
            .env("GH_TOKEN", "gh-test")
            .output()
            .unwrap();
        (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        )
    };
    let (ok, out) = run(&["auth", "install", "demo"]);
    assert!(ok, "{out}");
    assert!(
        out.contains(
            "installed demo from ArchAstro/pail-credential-demo@v1 (c1, script bin/demo.sh)"
        ),
        "{out}"
    );
    let exe = t.join("ext/pail-credential-demo/pail-credential-demo");
    assert_eq!(
        String::from_utf8(std::process::Command::new(&exe).output().unwrap().stdout).unwrap(),
        "tok-v1\n"
    );
    let m: serde_json::Value = serde_json::from_slice(
        &std::fs::read(t.join("ext/pail-credential-demo/manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        (m["commit"].as_str(), m["pinned"].as_bool()),
        (Some("c1"), Some(false))
    );
    assert!(
        log.lock().unwrap().iter().all(|l| l.ends_with("(auth)")),
        "GH_TOKEN is sent"
    );
    let (_, list) = run(&["auth", "list"]);
    assert!(
        list.contains("demo v1") && list.contains("ArchAstro/pail-credential-demo@v1"),
        "{list}"
    );
    // Already installed: refuse without --force.
    assert!(!run(&["auth", "install", "demo"]).0);

    // A new release: upgrade moves to it.
    set(&[
        (
            &format!("{repo}/releases/latest"),
            200,
            br#"{"tag_name":"v2","assets":[]}"#.to_vec(),
        ),
        (&format!("{repo}/commits/v2"), 200, b"c2".to_vec()),
        (
            &format!("{repo}/contents/pail-extension.json?ref=c2"),
            200,
            manifest.clone(),
        ),
        (
            &format!("{repo}/contents/bin/demo.sh?ref=c2"),
            200,
            script("v2"),
        ),
    ]);
    let (ok, out) = run(&["auth", "upgrade", "--all"]);
    assert!(ok && out.contains("demo             v1 -> v2"), "{out}");
    assert_eq!(
        String::from_utf8(std::process::Command::new(&exe).output().unwrap().stdout).unwrap(),
        "tok-v2\n"
    );
    assert!(run(&["auth", "upgrade", "demo"]).1.contains("up to date"));

    // Pinned to a tag: upgrade leaves it there.
    set(&[(
        &format!("{repo}/releases/tags/v1"),
        200,
        br#"{"tag_name":"v1","assets":[]}"#.to_vec(),
    )]);
    assert!(
        run(&[
            "auth",
            "install",
            "ArchAstro/pail-credential-demo@v1",
            "--force"
        ])
        .0
    );
    assert!(run(&["auth", "upgrade", "demo"]).1.contains("up to date"));
    assert_eq!(
        String::from_utf8(std::process::Command::new(&exe).output().unwrap().stdout).unwrap(),
        "tok-v1\n"
    );

    // A binary provider: no script, a release asset for this platform.
    let bin = "/repos/acme/pail-credential-sso";
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    };
    let asset = format!("pail-credential-sso-{os}-{arch}");
    set(&[
        (bin, 200, br#"{"default_branch":"main"}"#.to_vec()),
        (
            &format!("{bin}/releases/latest"),
            200,
            format!(
                r#"{{"tag_name":"v3","assets":[{{"name":"{asset}","url":"{api}/assets/9"}}]}}"#
            )
            .into_bytes(),
        ),
        (&format!("{bin}/commits/v3"), 200, b"c3".to_vec()),
        ("/assets/9", 200, script("sso")),
    ]);
    let (ok, out) = run(&["auth", "install", "acme/pail-credential-sso"]);
    assert!(ok && out.contains(&format!("binary {asset}")), "{out}");
    // Wrong repository name, missing repository.
    assert!(
        run(&["auth", "install", "acme/sso"])
            .1
            .contains("pail-credential-<name>")
    );
    assert!(
        run(&["auth", "install", "nobody/pail-credential-x"])
            .1
            .contains("repository not found")
    );
    let (ok, out) = run(&["auth", "remove", "sso"]);
    assert!(ok && !t.join("ext/pail-credential-sso").exists(), "{out}");
}

/// Anonymous reads over https:// (public repositories): with no credential provider the client
/// sends no Authorization header; a store that needs sign-in gets a one-line hint, and a missing
/// (or private) repository says so.
#[test]
fn anonymous_https_store() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _server = Child(
        cmd(
            &t,
            &[
                "serve",
                "--store",
                &format!("file://{}", t.join("bucket").display()),
                "--addr",
                &format!("127.0.0.1:{port}"),
                "--allow-anonymous",
            ],
        )
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap(),
    );
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let store = format!("http://127.0.0.1:{port}/acme/app");
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "a"]);
    let a = t.join("a");
    write(&a.join("f.txt"), "hi");
    pail(&a, &["add", "-A"]);
    pail(&a, &["commit", "-m", "first"]);

    // A fresh anonymous clone and log work with no provider configured.
    pail(&t, &["clone", &store, "b"]);
    assert_eq!(std::fs::read_to_string(t.join("b/f.txt")).unwrap(), "hi");
    assert!(pail(&t.join("b"), &["log"]).contains("first"));

    // A repository that is not there (or is private: servers answer 404 on purpose).
    let err = pail_fails(
        &t,
        &["clone", &format!("http://127.0.0.1:{port}/acme/nope"), "c"],
    );
    assert!(
        err.contains("doesn't exist, or you don't have access"),
        "{err}"
    );
    assert!(!t.join("c").exists());

    // A store that wants sign-in: no Authorization sent, one-line hint.
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr().to_ip().unwrap());
    let sent_auth = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = sent_auth.clone();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            if req.headers().iter().any(|h| h.field.equiv("Authorization")) {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let _ = req.respond(tiny_http::Response::from_string("no").with_status_code(401));
        }
    });
    let err = pail_fails(&t, &["clone", &format!("{base}/acme/app"), "d"]);
    assert!(
        err.starts_with("error: ")
            && err.contains("needs sign-in (401)")
            && err.contains("pail auth install <provider>")
            && err.trim().lines().count() == 1,
        "{err}"
    );
    assert!(!sent_auth.load(std::sync::atomic::Ordering::SeqCst));
}

/// `pail rebase`: -i through a scripted sequence editor, --autosquash, a plan file, --undo;
/// a stale working copy is told its branch was rewritten; conflicts publish nothing (exit 4);
/// the default branch is refused; the hooks see `rebase`.
#[test]
fn rebase_rewrites_history_and_working_copies_follow() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "a"]);
    let a = t.join("a");
    write(&a.join("base.txt"), "base");
    pail(&a, &["add", "-A"]);
    pail(&a, &["commit", "-m", "base"]);
    pail(&a, &["switch", "-c", "feat"]);
    for (f, msg) in [("one", "one"), ("two", "two"), ("one", "fixup! one")] {
        write(&a.join(f), msg);
        pail(&a, &["add", "-A"]);
        pail(&a, &["commit", "-m", msg]);
    }
    pail(&t, &["clone", &store, "stale", "-b", "feat"]);
    let stale = t.join("stale");

    // The default branch is everyone's: refused without --rewrite-default.
    let err = pail_fails(&a, &["rebase", "-b", "main", "HEAD~1"]);
    assert!(err.contains("default branch"), "{err}");

    // --autosquash, dry run first: nothing moves.
    let tip = pail(&a, &["rev-parse", "feat"]);
    let out = pail(&a, &["rebase", "--autosquash", "--dry-run"]);
    assert!(out.contains("dry run") && out.contains("epoch 1"), "{out}");
    assert_eq!(pail(&a, &["rev-parse", "feat"]), tip);
    let out = pail(&a, &["rebase", "--autosquash"]);
    assert!(out.contains("epoch 1"), "{out}");
    let log = pail(&a, &["log", "--format=%s"]);
    assert_eq!(log, "two\none\nbase\nInitial empty commit\n");
    assert_eq!(
        std::fs::read_to_string(a.join("one")).unwrap(),
        "fixup! one"
    );

    // A stale working copy commits on top: replayed, and told.
    write(&stale.join("s.txt"), "s");
    pail(&stale, &["add", "-A"]);
    let out = pail(&stale, &["commit", "-m", "stale"]);
    assert!(out.contains("note: feat was rewritten"), "{out}");
    pail(&a, &["sync"]);

    // -i: reword through the todo text, reorder, drop.
    let editor = "sed -i.bak -e 's/^pick \\([0-9a-f]*\\) one$/reword \\1 ONE/' -e '/ two$/d'";
    let out = cmd(&a, &["rebase", "-i", "main"])
        .env("PAIL_SEQUENCE_EDITOR", editor)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = pail(&a, &["log", "--format=%s"]);
    assert_eq!(log, "stale\nONE\nbase\nInitial empty commit\n");
    assert!(!a.join("two").exists(), "the working copy followed");

    // --undo puts it back, and is itself a rewrite.
    let out = pail(&a, &["rebase", "--undo"]);
    assert!(out.contains("epoch 3"), "{out}");
    let log = pail(&a, &["log", "--format=%s"]);
    assert_eq!(log, "stale\ntwo\none\nbase\nInitial empty commit\n");

    // A plan file: swap two commits that edit the same file → conflict, nothing published.
    for v in ["1", "2"] {
        write(&a.join("one"), v);
        pail(&a, &["commit", "-a", "-m", &format!("one={v}")]);
    }
    let plan = pail(&a, &["rebase", "--print-plan"]);
    let mut p: serde_json::Value = serde_json::from_str(&plan).unwrap();
    let steps = p["steps"].as_array_mut().unwrap();
    let n = steps.len();
    steps.swap(n - 1, n - 2);
    let file = t.join("plan.json");
    std::fs::write(&file, p.to_string()).unwrap();
    let before = pail(&a, &["rev-parse", "feat"]);
    let out = cmd(&a, &["rebase", "--plan", file.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("one=") && err.contains("one"), "{err}");
    assert_eq!(pail(&a, &["rev-parse", "feat"]), before);

    // The same plan with a squash instead: --json reports the map; hooks see `rebase`.
    let hooks = t.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    hook(&hooks, "pre-publish", "echo \"pre $@\" >> \"$HOOKLOG\"");
    hook(
        &hooks,
        "post-rewrite",
        "echo \"post $@\" >> \"$HOOKLOG\"; cat >> \"$HOOKLOG\"",
    );
    pail(&a, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
    let mut p: serde_json::Value = serde_json::from_str(&plan).unwrap();
    let last = p["steps"].as_array().unwrap().len() - 1;
    let id = p["steps"][last]["pick"].clone();
    p["steps"][last] = serde_json::json!({"squash": id});
    std::fs::write(&file, p.to_string()).unwrap();
    let log_file = t.join("hook.log");
    let out = cmd(&a, &["rebase", "--plan", file.to_str().unwrap(), "--json"])
        .env("HOOKLOG", &log_file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let first = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(first.lines().next().unwrap()).unwrap();
    assert_eq!(v["epoch"], 4);
    assert_eq!(v["map"].as_object().unwrap().len(), 2);
    let hooklog = std::fs::read_to_string(&log_file).unwrap();
    assert!(
        hooklog.contains("pre feat rebase") && hooklog.contains("post rebase"),
        "{hooklog}"
    );
    let msg = pail(&a, &["log", "-1", "--format=%B"]);
    assert!(msg.contains("one=1") && msg.contains("one=2"), "{msg}");
}

/// `pail maintenance repack`: same files, same Git tree ids; a working copy at the old tip
/// syncs over it without touching a path.
#[test]
fn repack_changes_layout_not_content() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "alice"]);
    pail(&t, &["clone", &store, "bob"]);
    let (a, b) = (t.join("alice"), t.join("bob"));
    // A commit per file, each over the 64-byte inline limit: a pack per commit.
    for i in 0..6 {
        write(
            &a.join(format!("src/f{i}.txt")),
            &format!("file {i} {}\n", "x".repeat(200)),
        );
        pail(&a, &["add", "-A"]);
        pail(&a, &["commit", "-m", &format!("add {i}")]);
    }
    pail(&b, &["sync"]);
    let tree = pail(&a, &["rev-parse", "HEAD^{tree}"]);

    let dry = pail(&a, &["maintenance", "repack", "--dry-run"]);
    assert!(dry.contains("would move: 6 files"), "{dry}");
    assert!(dry.contains("6 -> 1"), "{dry}");
    assert_eq!(
        pail(&a, &["rev-parse", "HEAD"]),
        pail(&b, &["rev-parse", "HEAD"])
    );

    let out = pail(&a, &["maintenance", "repack"]);
    assert!(
        out.contains("moved: 6 files") && out.contains("committed"),
        "{out}"
    );
    // Alice's HEAD is her own commit until she syncs; so is bob's.
    assert!(pail(&a, &["sync"]).contains("(0 path(s) updated)"));
    assert!(pail(&b, &["sync"]).contains("(0 path(s) updated)"));
    assert_eq!(pail(&a, &["rev-parse", "HEAD^{tree}"]), tree);
    assert_eq!(pail(&b, &["rev-parse", "HEAD^{tree}"]), tree);
    assert_eq!(
        pail(&b, &["cat", "src/f3.txt"]),
        format!("file 3 {}\n", "x".repeat(200))
    );
    assert!(pail(&a, &["maintenance", "repack"]).contains("nothing to move"));

    // History hides the repack, unless asked.
    let log = pail(&a, &["log", "--oneline"]);
    assert!(!log.contains("repack") && log.lines().count() == 7, "{log}");
    assert!(pail(&a, &["log", "--oneline", "-1"]).contains("add 5"));
    let log = pail(&a, &["log", "--oneline", "--maintenance"]);
    assert!(
        log.lines()
            .next()
            .unwrap()
            .contains("pail maintenance: repack"),
        "{log}"
    );
    assert_eq!(log.lines().count(), 8, "{log}");

    let help = pail(&a, &["help", "maintenance", "repack"]);
    assert!(
        help.contains("--dry-run") && help.contains("--full"),
        "{help}"
    );
}

/// A clone whose tip is spread over many small packs says so, once; `status` repeats it at
/// most daily; `maintenance.warnSmallPacks` sets or silences it.
#[test]
fn small_packs_are_warned_about_once() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "alice"]);
    let a = t.join("alice");
    for i in 0..5 {
        write(
            &a.join(format!("f{i}.txt")),
            &format!("file {i} {}\n", "x".repeat(200)),
        );
        pail(&a, &["add", "-A"]);
        pail(&a, &["commit", "-m", &format!("add {i}")]);
    }
    let global = t.join("gconfig");
    std::fs::write(&global, "[maintenance]\n\twarnSmallPacks = 3\n").unwrap();
    let with = |dir: &Path, args: &[&str]| {
        let out = cmd(dir, args)
            .env("PAIL_CONFIG_GLOBAL", &global)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stderr).to_string()
    };

    let err = with(&t, &["clone", &store, "carol"]);
    assert!(
        err.contains("warning: 5 of this tip's 5 packs") && err.contains("maintenance repack"),
        "{err}"
    );
    let c = t.join("carol");
    assert!(!with(&c, &["status"]).contains("warning"), "once per day");
    std::fs::remove_file(c.join(".pail/repack-advice")).unwrap();
    assert!(with(&c, &["status"]).contains("warning"));
    // Below the threshold, or switched off, it stays quiet.
    std::fs::write(&global, "[maintenance]\n\twarnSmallPacks = 0\n").unwrap();
    assert!(!with(&t, &["clone", &store, "dave"]).contains("warning"));
    pail(&a, &["maintenance", "repack"]);
    std::fs::write(&global, "[maintenance]\n\twarnSmallPacks = 3\n").unwrap();
    assert!(!with(&t, &["clone", &store, "erin"]).contains("warning"));
}

/// `log --json` and `branch --json`: what a UI needs that Git's output cannot carry.
#[test]
fn json_log_and_branches_for_browsers() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "alice"]);
    let a = t.join("alice");
    for i in 0..3 {
        write(
            &a.join(format!("f{i}.txt")),
            &format!("file {i} {}\n", "x".repeat(200)),
        );
        pail(&a, &["add", "-A"]);
        pail(&a, &["commit", "-m", &format!("add {i}\n\nbody {i}")]);
    }
    pail(&a, &["maintenance", "repack"]);
    let lines = |args: &[&str]| -> Vec<serde_json::Value> {
        pail(&a, args)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    // Maintenance commits are hidden unless asked for, and carry their meta key.
    let shown = lines(&["--store", &store, "log", "--json"]);
    assert_eq!(shown.len(), 4, "3 commits and the initial empty one");
    assert_eq!(shown[0]["message"], "add 2\n\nbody 2");
    assert_eq!(shown[0]["id"].as_str().unwrap().len(), 64);
    assert_eq!(shown[0]["parent"], shown[1]["id"]);
    assert!(shown[3]["parent"].is_null());
    let all = lines(&["--store", &store, "log", "--json", "--maintenance"]);
    assert_eq!(all.len(), 5);
    assert!(all[0]["meta"].get("pail.maintenance").is_some(), "{all:?}");
    let one = lines(&[
        "--store",
        &store,
        "log",
        "--json",
        "-1",
        "--maintenance",
        shown[1]["id"].as_str().unwrap(),
    ]);
    assert_eq!(one[0]["message"], "add 1\n\nbody 1");

    let out = pail(&a, &["--store", &store, "branch", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["default"], "main");
    assert_eq!(v["branches"][0]["name"], "main");
    assert_eq!(v["branches"][0]["commit"], all[0]["id"]);
}

/// `pail <name>` runs `pail-<name>` from PATH with the store and the pail binary in its
/// environment, forwards its exit code, and lists it in `pail help` and `pail --list`.
#[cfg(unix)]
#[test]
fn plugins_run_from_path() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let bin = t.join("bin");
    let script = bin.join("pail-hello");
    write(
        &script,
        "#!/bin/sh\necho \"args=$* store=$PAIL_STORE bin=$PAIL_BIN\"\nexit ${HELLO_EXIT:-0}\n",
    );
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Not executable, and a credential provider: neither is a plugin.
    write(&bin.join("pail-plain"), "#!/bin/sh\n");
    write(&bin.join("pail-credential-x"), "#!/bin/sh\n");
    std::fs::set_permissions(
        &bin.join("pail-credential-x"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let store = format!("file://{}", t.join("bucket").display());
    let with_path = |args: &[&str]| {
        let mut c = cmd(&t, args);
        c.env("PATH", &path);
        c
    };
    pail(&t, &["init", &store]);

    // Arguments pass through (including flags pail itself would reject); the store is the
    // --store flag's, and PAIL_BIN is this binary.
    let out = with_path(&["--store", &store, "hello", "a", "--flag", "-m", "x y"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("args=a --flag -m x y"), "{s}");
    assert!(s.contains(&format!("store={store}")), "{s}");
    assert!(s.contains(env!("CARGO_BIN_EXE_pail")), "{s}");

    // PAIL_STORE from the environment, and the store of the working copy we are in.
    let out = with_path(&["hello"])
        .env("PAIL_STORE", &store)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("store={store}")));
    pail(&t, &["clone", &store, "w"]);
    let out = with_path(&["hello"])
        .current_dir(t.join("w"))
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&format!("store={store}")),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // The exit code is the plugin's.
    let out = with_path(&["hello"])
        .env("HELLO_EXIT", "7")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7));

    // Listed, minus non-plugins.
    let help = String::from_utf8_lossy(&with_path(&[]).output().unwrap().stdout).to_string();
    assert!(
        help.contains("Plugins") && help.contains("pail-hello"),
        "{help}"
    );
    assert!(
        !help.contains("pail-plain") && !help.contains("credential-x"),
        "{help}"
    );
    let list =
        String::from_utf8_lossy(&with_path(&["--list"]).output().unwrap().stdout).to_string();
    assert!(
        list.starts_with("hello") && list.contains("pail-hello"),
        "{list}"
    );
    assert!(!list.contains("plain"), "{list}");

    // Built-ins win over a plugin of the same name; unknown names are still errors.
    write(&bin.join("pail-log"), "#!/bin/sh\necho shadowed\n");
    std::fs::set_permissions(
        &bin.join("pail-log"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let out = with_path(&["--store", &store, "log"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains("shadowed"));
    let out = with_path(&["nosuchthing"]).output().unwrap();
    assert!(!out.status.success());
}

/// Line comments and threads in `pail pr`: anchors are checked, a reply takes its parent's
/// line, a push that changes the line marks the thread outdated, and an approval is of the
/// commit it was given on.
#[test]
fn pr_line_comments_and_stale_approvals() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 not found; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    let ext = Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions");
    let path = format!(
        "{}:{}",
        ext.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |dir: &Path, args: &[&str], author: &str| {
        let out = cmd(dir, args)
            .env("PATH", &path)
            .env("PAIL_AUTHOR", author)
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };
    let ok = |dir: &Path, args: &[&str], author: &str| {
        let (ok, o, e) = run(dir, args, author);
        assert!(ok, "pail {args:?} failed:\n{o}{e}");
        o
    };
    let alice = "Alice <a@x>";
    let bob = "Bob <b@x>";
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "w"]);
    let w = t.join("w");
    write(&w.join("a.txt"), "a\n");
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "base"]);
    pail(&w, &["switch", "-c", "pr/feat"]);
    write(&w.join("b.txt"), "one\ntwo\nthree\n");
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "Add b"]);
    ok(&w, &["pr", "create"], alice);
    let first = pail(&w, &["rev-parse", "pr/feat"]).trim().to_string();

    // Anchors the diff does not have are refused; nothing is written for them.
    for (args, why) in [
        (vec!["--path", "b.txt", "--line", "4"], "no line 4"),
        (vec!["--path", "a.txt", "--line", "1"], "not changed"),
        (
            vec!["--path", "b.txt", "--line", "1", "--side", "old"],
            "no line 1",
        ),
        (
            vec!["--path", "b.txt", "--line", "1", "--commit", "0000"],
            "not one of",
        ),
        (vec!["--reply-to", "0000000000001-Bob"], "no comment"),
    ] {
        let mut a = vec!["pr", "comment", "1", "-m", "x"];
        a.extend(args);
        let (good, _, err) = run(&w, &a, bob);
        assert!(!good && err.contains(why), "{why}: {err}");
    }
    let files = pail(&t, &["--store", &store, "ls", "-r", "_meta:"]);
    assert!(!files.contains("comments/"), "{files}");

    ok(
        &w,
        &[
            "pr", "comment", "1", "-m", "why one?", "--path", "b.txt", "--line", "1",
        ],
        bob,
    );
    ok(
        &w,
        &[
            "pr", "comment", "1", "-m", "why two?", "--path", "b.txt", "--line", "2",
        ],
        bob,
    );
    let view: serde_json::Value =
        serde_json::from_str(&ok(&w, &["pr", "view", "1", "--json"], alice)).unwrap();
    let root = view["comments"][0]["file"].as_str().unwrap().to_string();
    ok(
        &w,
        &["pr", "comment", "1", "-m", "because", "--reply-to", &root],
        alice,
    );
    let view: serde_json::Value =
        serde_json::from_str(&ok(&w, &["pr", "view", "1", "--json"], alice)).unwrap();
    let reply = &view["comments"][2];
    assert_eq!(reply["in_reply_to"], root.as_str());
    assert_eq!(
        (
            reply["path"].clone(),
            reply["line"].clone(),
            reply["commit"].clone()
        ),
        ("b.txt".into(), 1.into(), first.as_str().into())
    );
    assert_eq!(reply["thread"], root.as_str());
    assert!(
        view["comments"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["outdated"] == false)
    );

    // Bob approves the first commit; then a push changes line 1.
    ok(&w, &["pr", "review", "1", "--approve"], bob);
    write(&w.join("b.txt"), "ONE\ntwo\nthree\n");
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "Shout"]);
    let view: serde_json::Value =
        serde_json::from_str(&ok(&w, &["pr", "view", "1", "--json"], alice)).unwrap();
    let outdated: Vec<bool> = view["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["outdated"].as_bool().unwrap())
        .collect();
    assert_eq!(outdated, [true, false, true], "{view}");
    let text = ok(&w, &["pr", "view", "1"], alice);
    assert!(
        text.contains("b.txt:1 (new)  (outdated)") && text.contains("b.txt:2 (new)"),
        "{text}"
    );
    assert_eq!(view["reviews"][0]["stale"], true);

    // The approval was of the first commit: it does not count for the new head.
    let (good, _, err) = run(&w, &["pr", "merge", "1"], alice);
    assert!(
        !good && err.contains("0 of 1 approvals (1 on an older commit)"),
        "{err}"
    );
    ok(&w, &["pr", "review", "1", "--approve"], bob);
    ok(&w, &["pr", "merge", "1"], alice);
    // Merged: no more line comments, replies still fine.
    let (good, _, err) = run(
        &w,
        &[
            "pr", "comment", "1", "-m", "x", "--path", "b.txt", "--line", "1",
        ],
        bob,
    );
    assert!(!good && err.contains("not one of"), "{err}");
    ok(
        &w,
        &["pr", "comment", "1", "-m", "thanks", "--reply-to", &root],
        bob,
    );
}

/// `pail pr` end to end through plugin dispatch: create (title from the commit), draft,
/// comment, review, edit, close/reopen and merge, with the records as documented.
#[cfg(unix)]
#[test]
fn pr_plugin_commands() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 not found; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let store = format!("file://{}", t.join("bucket").display());
    let ext = Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions");
    let path = format!(
        "{}:{}",
        ext.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |dir: &Path, args: &[&str], author: &str| {
        let out = cmd(dir, args)
            .env("PATH", &path)
            .env("PAIL_AUTHOR", author)
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };
    let ok = |dir: &Path, args: &[&str], author: &str| {
        let (ok, o, e) = run(dir, args, author);
        assert!(ok, "pail {args:?} failed:\n{o}{e}");
        o
    };
    let alice = "Alice <a@x>";
    let bob = "Bob <b@x>";
    pail(&t, &["init", &store]);
    pail(&t, &["clone", &store, "w"]);
    let w = t.join("w");
    write(&w.join("a.txt"), "a\n");
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "base"]);
    pail(&w, &["switch", "-c", "pr/feat"]);

    // Nothing to propose yet.
    let (good, _, err) = run(&w, &["pr", "create"], alice);
    assert!(!good && err.contains("no commits between"), "{err}");

    write(&w.join("b.txt"), "b\n");
    pail(&w, &["add", "-A"]);
    pail(&w, &["commit", "-m", "Add b\n\nBecause b is good."]);
    // Title and body default to the first commit; --draft is kept.
    let rec: serde_json::Value =
        serde_json::from_str(&ok(&w, &["pr", "create", "--draft", "--json"], alice)).unwrap();
    assert_eq!(rec["id"], 1);
    assert_eq!(rec["title"], "Add b");
    assert_eq!(rec["body"], "Because b is good.");
    assert_eq!(
        (rec["draft"].clone(), rec["base"].clone()),
        (true.into(), "main".into())
    );
    // The record is a plain file on _meta.
    let raw = pail(&t, &["--store", &store, "cat", "_meta:prs/1.json"]);
    assert!(raw.contains("\"head\": \"pr/feat\""), "{raw}");
    let (good, _, err) = run(&w, &["pr", "create"], alice);
    assert!(!good && err.contains("already open"), "{err}");

    ok(&w, &["pr", "comment", "1", "-m", "ping"], bob);
    ok(&w, &["pr", "review", "1", "--approve", "-m", "lgtm"], bob);
    let files = pail(&t, &["--store", &store, "ls", "-r", "_meta:prs"]);
    assert!(files.contains("prs/1/reviews/Bob.json"), "{files}");
    assert!(
        files.contains("-Bob.json") && files.contains("prs/1/comments/"),
        "{files}"
    );
    let view: serde_json::Value =
        serde_json::from_str(&ok(&w, &["pr", "view", "1", "--json"], alice)).unwrap();
    assert_eq!(view["comments"][0]["body"], "ping");
    assert_eq!(view["reviews"][0]["verdict"], "approve");

    // A draft does not merge, even with --force; ready, then policy (no CI file: approvals only).
    let (good, _, err) = run(&w, &["pr", "merge", "1", "--force"], alice);
    assert!(!good && err.contains("draft"), "{err}");
    ok(&w, &["pr", "ready", "1"], alice);
    ok(&w, &["pr", "edit", "1", "--title", "Add the b file"], alice);

    // Close, reopen, then merge.
    ok(&w, &["pr", "close", "1", "-m", "wait"], alice);
    assert!(ok(&w, &["pr", "list"], alice).trim().is_empty());
    assert!(ok(&w, &["pr", "list", "-s", "closed"], alice).contains("Add the b file"));
    let (good, _, err) = run(&w, &["pr", "merge", "1"], alice);
    assert!(!good && err.contains("is closed"), "{err}");
    ok(&w, &["pr", "reopen", "1"], alice);
    let diff = ok(&w, &["pr", "diff", "1"], alice);
    assert!(diff.contains("+++ b/b.txt"), "{diff}");
    ok(&w, &["pr", "merge", "1"], alice);
    assert_eq!(pail(&t, &["--store", &store, "cat", "main:b.txt"]), "b\n");
    let rec: serde_json::Value =
        serde_json::from_str(&pail(&t, &["--store", &store, "cat", "_meta:prs/1.json"])).unwrap();
    assert_eq!(rec["state"], "merged");
    assert!(rec["merged_commit"].is_string() && rec["merged_at"].is_number());
    assert!(rec["updated"].is_number() && rec["closed_at"].is_null());

    // checkout switches the working copy to the PR's branch.
    pail(&w, &["switch", "main"]);
    ok(&w, &["pr", "checkout", "1"], alice);
    assert!(pail(&w, &["status"]).contains("pr/feat"));
}
