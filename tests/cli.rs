use std::path::Path;
use std::process::Command;

/// A global config file private to this test process (never the user's).
fn global_config() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap()).path().join("config")
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
    let s = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
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
    assert_eq!(std::fs::read_to_string(b.join("src/lib.rs")).unwrap(), "v1", "bob picked up alice's file");

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
    assert!(log.lines().next().unwrap().contains("Land feature"), "{log}");
    let branches = pail(&a, &["branch"]);
    assert!(branches.contains("* main") && branches.contains("feature"), "{branches}");

    // Symlinks and exec bits survive a fresh clone.
    std::os::unix::fs::symlink("feature.txt", a.join("link")).unwrap();
    write(&a.join("run.sh"), "#!/bin/sh\n");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(a.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    pail(&a, &["add", "-A"]);
    pail(&a, &["commit", "-m", "link + exec"]);
    pail(&t, &["clone", &store, "carol"]);
    let c = t.join("carol");
    assert_eq!(std::fs::read_link(c.join("link")).unwrap().to_str().unwrap(), "feature.txt");
    assert_eq!(std::fs::metadata(c.join("run.sh")).unwrap().permissions().mode() & 0o777, 0o755);
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
    assert_eq!(std::fs::read_to_string(t.join("after/keep.txt")).unwrap(), "k".repeat(500));
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
    let big: Vec<u8> = (0..3u32 << 20).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    write(&a.join("src/lib.rs"), &lib);
    write(&a.join("README.md"), &"readme ".repeat(20));
    write(&a.join("tiny"), "inline");
    std::fs::write(a.join("big.bin"), &big).unwrap();
    write(&a.join("run.sh"), &format!("#!/bin/sh\n{}", "echo hi\n".repeat(20)));
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
    std::fs::File::options().write(true).open(a.join("src/lib.rs")).unwrap().set_modified(old).unwrap();
    // An unstaged edit in the donor: the new worktree gets the base version with a fresh mtime.
    write(&a.join("README.md"), "edited in a");
    let started = std::time::SystemTime::now() - std::time::Duration::from_secs(2);

    let out = pail(&a, &["worktree", "add", "../b", "-b", "feat"]);
    assert!(out.contains("seeded 2 ignored path(s)") && out.contains(".env") && out.contains("target"), "{out}");
    let b = t.join("b");
    assert_eq!(std::fs::read_to_string(b.join("src/lib.rs")).unwrap(), lib);
    assert_eq!(std::fs::read(b.join("big.bin")).unwrap(), big);
    assert_eq!(std::fs::read_to_string(b.join("README.md")).unwrap(), "readme ".repeat(20));
    assert_eq!(std::fs::read_link(b.join("link")).unwrap().to_str().unwrap(), "src/lib.rs");
    assert_eq!(std::fs::metadata(b.join("run.sh")).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(std::fs::metadata(b.join("src/lib.rs")).unwrap().permissions().mode() & 0o777, 0o644);
    assert_eq!(std::fs::read_to_string(b.join("target/debug/app")).unwrap(), "binary".repeat(1000));
    assert_eq!(std::fs::read_to_string(b.join(".env")).unwrap(), "SECRET=1");
    assert!(!b.join("scratch").exists(), "only build output and env files are seeded by default");
    assert_eq!(mtime(&b.join("src/lib.rs")), old, "identical file keeps the donor's mtime");
    assert!(mtime(&b.join("README.md")) >= started, "differing file gets a fresh mtime");
    let repo = a.join(".pail");
    assert!(blob_path(&repo, lib.as_bytes()).exists() && blob_path(&repo, &big).exists());
    assert_eq!(std::fs::read_to_string(b.join(".pail/repo")).unwrap(), repo.to_str().unwrap(), "linked to a's repository");
    if cfg!(target_vendor = "apple") {
        assert!(out.contains("MB cloned"), "APFS should clone: {out}");
    }
    let st = pail(&b, &["status"]);
    assert!(st.contains("on feat") && st.contains("clean"), "{st}");

    // Editing a clone in place touches neither the blob store nor the donor.
    let mut f = std::fs::File::options().write(true).open(b.join("src/lib.rs")).unwrap();
    f.seek(std::io::SeekFrom::Start(0)).unwrap();
    f.write_all(b"XXXX").unwrap();
    drop(f);
    let mut f = std::fs::File::options().write(true).open(b.join("target/debug/app")).unwrap();
    f.write_all(b"REBUILT").unwrap();
    drop(f);
    assert_eq!(std::fs::read_to_string(a.join("src/lib.rs")).unwrap(), lib);
    assert_eq!(std::fs::read_to_string(a.join("target/debug/app")).unwrap(), "binary".repeat(1000));
    let stored = blob_path(&repo, lib.as_bytes());
    assert_eq!(pail::hash::Hash::of(&std::fs::read(&stored).unwrap()), pail::hash::Hash::of(lib.as_bytes()));
    assert!(pail(&b, &["status"]).contains("M src/lib.rs"));

    // Committed content goes into the blob store for the next worktree to clone.
    write(&b.join("new.txt"), &"fresh content ".repeat(10));
    pail(&b, &["add", "new.txt"]);
    pail(&b, &["commit", "-m", "new"]);
    assert!(blob_path(&repo, "fresh content ".repeat(10).as_bytes()).exists());

    // Without clone support, the same result by copying.
    let out = cmd(&a, &["worktree", "add", "../c", "--branch", "feat"]).env("PAIL_CLONE", "off").output().unwrap();
    let out = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.contains("0 KB cloned") && out.contains("0 cloned, 2 copied"), "{out}");
    let c = t.join("c");
    assert_eq!(std::fs::read_to_string(c.join("new.txt")).unwrap(), "fresh content ".repeat(10));
    assert_eq!(std::fs::read(c.join("big.bin")).unwrap(), big);
    assert_eq!(std::fs::read_to_string(c.join("target/debug/app")).unwrap(), "binary".repeat(1000));

    let list = pail(&a, &["worktree", "list"]);
    assert!(list.contains(&format!("* {}", a.display())), "{list}");
    assert!(list.contains("feat") && list.contains(&c.display().to_string()), "{list}");
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
    assert_eq!(subjects, "third: util\nsecond\nfirst\nInitial empty commit\n");
    assert_eq!(pail(&w, &["log", "--format=%s", "-2"]), "third: util\nsecond\n");
    let medium = bare_cmd(&["log", "-1", "--skip=2"]);
    assert!(medium.starts_with("commit ") && medium.contains("Author: Tester <t@example.com>\nDate:   "), "{medium}");
    assert!(medium.ends_with("\n\n    first\n    \n    body line\n"), "{medium:?}");
    assert_eq!(bare_cmd(&["log", "--format=%s", "--", "src/util"]), "third: util\n");
    let ns = bare_cmd(&["log", "--format=%s", "--name-status", "-1", "HEAD~1"]);
    assert_eq!(ns, "second\n\nM\tsrc/lib.rs\n");
    let patch = bare_cmd(&["show", "--format=%s", "HEAD~1"]);
    assert!(patch.contains("@@ -1 +1,2 @@\n fn a() {}\n+fn b() {}\n"), "{patch}");
    // Git object ids computed from pail content.
    assert_eq!(bare_cmd(&["rev-parse", "HEAD:README.md"]).trim(), "ce013625030ba8dba906f756967f9e9ca394464a");
    let tree = bare_cmd(&["ls-tree", "HEAD"]);
    assert_eq!(tree.lines().count(), 2, "{tree}");
    assert!(tree.starts_with("100644 blob ce013625030ba8dba906f756967f9e9ca394464a\tREADME.md\n040000 tree "), "{tree}");
    assert_eq!(bare_cmd(&["cat-file", "-p", "HEAD:src/util/mod.rs"]), "pub mod x;\n");
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
    assert!(out.status.success() && s.starts_with("fetched 3 file version(s)"), "{s}{}", String::from_utf8_lossy(&out.stderr));
    // With the store's packs gone, contents still come from the cache.
    std::fs::remove_dir_all(bare.join("packs")).unwrap();
    let out = run(&["log", "-p", "--format=%s"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && s.contains("+alpha two alpha two") && s.contains("+beta beta"), "{s}{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&run(&["cat", "HEAD~1:a.txt"]).stdout), "alpha ".repeat(100));
    let out = run(&["cache", "prune", "--max-size", "0"]);
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("removed 3 file(s)"), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(!run(&["cat", "a.txt"]).status.success(), "pruned content must be refetched, and the packs are gone");
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
        let mut full = if local { vec!["--repo", r.to_str().unwrap()] } else { vec!["--store", bare_s] };
        full.extend_from_slice(args);
        let out = cmd(&t, &full).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let cases: [&[&str]; 4] = [&["log"], &["log", "--oneline", "--name-status"], &["log", "-p", "-3", "HEAD~1"], &["log", "--format=%h %p %s", "--", "f1.txt"]];
    for args in cases {
        assert_eq!(read(true, args), read(false, args), "{args:?}");
    }
    assert!(r.join("graph/commits2.idx").exists(), "the repository's commit graph was built");
    // New commits after the graph exists.
    commit(6);
    commit(7);
    for args in cases {
        assert_eq!(read(true, args), read(false, args), "{args:?} after new commits");
    }
    // Abbreviated ids resolve through the graph.
    let short = read(true, &["log", "-1", "--format=%h", "HEAD~3"]);
    assert_eq!(read(true, &["log", "-1", "--format=%s", short.trim()]), "c4\n");
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
    assert_eq!(std::fs::read_to_string(t.join("from-bare/a.txt")).unwrap(), "aaaa".repeat(50));
    assert_eq!(std::fs::read_to_string(t.join("from-bare/.pail/repo")).unwrap(), r.to_str().unwrap());
    assert!(blob_path(&r, "aaaa".repeat(50).as_bytes()).exists(), "contents live in the bare repository");
    let list = pail(&r, &["worktree", "list"]);
    assert!(list.contains("(bare)") && list.contains("from-bare"), "{list}");
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
    assert!(out.contains("landed 3 commit(s)") && out.contains("fast-forward"), "{out}");
    assert_eq!(read(&["log", "--format=%H", "-3", "main"]), stack_ids, "the stack's own commits");
    assert_eq!(read(&["log", "--format=%s", "main"]), "s3: add f2, drop y\ns2: extend f1\ns1: add f1\nbase\nInitial empty commit\n");

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
    assert!(out.contains("landed 2 commit(s)") && out.contains("replayed"), "{out}");
    assert_eq!(read(&["log", "--format=%s", "-4", "main"]), "t2: extend g\nt1: add g\nmain moved\ns3: add f2, drop y\n");
    assert_eq!(read(&["log", "--format=%s|%an|%ad", "-2", "main"]), before, "authors, dates and messages kept");
    assert_eq!(read(&["log", "--format=%s", "--name-status", "-2", "main"]), diffs_before, "each commit changes what it changed");
    assert_eq!(read(&["cat", "main:g.txt"]), "g1\ng2\n");
    assert_eq!(read(&["cat", "main:other.txt"]), "theirs\n");
    // Landing again is a no-op; a commit added to the stack later lands alone, on top.
    let out = pail(&a, &["land", "feature2", "--stack"]);
    assert!(out.contains("nothing to land"), "{out}");
    write(&a.join("g.txt"), "g1\ng2\ng3\n");
    commit(&a, "t3: extend g again");
    let out = pail(&a, &["land", "feature2", "--stack"]);
    assert!(out.contains("landed 1 commit(s)"), "{out}");
    assert_eq!(read(&["log", "--format=%s", "-4", "main"]), "t3: extend g again\nt2: extend g\nt1: add g\nmain moved\n");
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
    assert_eq!(read(&["log", "--format=%H", "-1", "main"]), main_before, "nothing landed");
    assert!(!read(&["ls", "-r", "main"]).contains("h.txt"), "not even the commits before the conflicting one");
}

/// Git-style config: identity from config (required), local over global, and the default
/// branch from `init.defaultBranch` / the store's HEAD instead of an assumed `main`.
#[test]
fn config_identity_and_default_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(tmp.path()).unwrap();
    let global = t.join("global-config");
    let run = |dir: &Path, args: &[&str]| -> std::process::Output {
        cmd(dir, args).env_remove("PAIL_AUTHOR").env("PAIL_CONFIG_GLOBAL", &global).output().unwrap()
    };
    let ok = |dir: &Path, args: &[&str]| -> String {
        let o = run(dir, args);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let store = t.join("store");
    let s = store.to_str().unwrap();
    // No identity: refuse, as Git does.
    let o = run(&t, &["init", s]);
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("Please tell me who you are"));
    ok(&t, &["config", "--global", "user.name", "Ada Lovelace"]);
    ok(&t, &["config", "--global", "user.email", "ada@example.com"]);
    ok(&t, &["config", "--global", "init.defaultBranch", "trunk"]);
    assert_eq!(ok(&t, &["config", "user.name"]), "Ada Lovelace\n");
    assert!(std::fs::read_to_string(&global).unwrap().contains("[user]"), "Git-format file");

    // init.defaultBranch names the first branch; clone checks out the store's default.
    let out = ok(&t, &["init", s]);
    assert!(out.contains("(trunk @"), "{out}");
    ok(&t, &["clone", s, "w"]);
    let w = t.join("w");
    assert!(ok(&w, &["status"]).starts_with("on trunk"));
    assert_eq!(ok(&w, &["branch", "--default"]), "trunk\n");
    // Local identity overrides global; commits carry it.
    ok(&w, &["config", "user.name", "Grace Hopper"]);
    assert!(ok(&w, &["config", "--list", "--show-scope"]).contains("local\tuser.name=Grace Hopper"));
    write(&w.join("a.txt"), "a\n");
    ok(&w, &["add", "-A"]);
    ok(&w, &["commit", "-m", "a"]);
    assert_eq!(ok(&w, &["log", "-1", "--format=%an <%ae>|%cn"]), "Grace Hopper <ada@example.com>|Grace Hopper\n");
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
    assert!(ok(&w, &["log", "-1", "--format=%ad", "--date=unix"]).trim().parse::<u64>().is_ok());
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
    let mut child = cmd(&w, &["inspect", "--no-open", "--addr", &addr]).spawn().unwrap();
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
    assert_eq!(f["spine"].as_array().unwrap().last().unwrap()["level"], 0, "the descent ends at a leaf page");
    assert!(f["preview"].as_str().unwrap().starts_with("fn a()"));
    let pack = f["entry"]["loc"]["pack"].as_str().unwrap();
    let p = get(&format!("/api/pack/{pack}"));
    assert!(p["blobs"].as_array().unwrap().iter().any(|b| b["paths"][0] == "src/lib.rs"));
    assert_eq!(get("/api/file?path=tiny")["entry"]["loc"]["type"], "inline");
    // Logical → physical: the pages and packs behind a folder.
    let cover = get("/api/cover?path=src");
    assert_eq!(cover["files"], 1);
    assert!(cover["storage"].as_array().unwrap().iter().any(|s| s["kind"] == "pack"));
    assert_eq!(cover["pages"].as_array().unwrap().iter().filter(|p| p["level"] == 0).count(), 1);
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
    hook(&hooks, "pre-commit", &format!(r#"
echo "pre-commit $PAIL_HOOK $PAIL_HOOK_BRANCH dir=${{PAIL_DIR:-unset}} cwd=$(pwd)" >> {l}
while IFS= read -r -d '' f; do
  [[ "$f" == bad.txt ]] && {{ echo "bad.txt is not allowed" >&2; exit 1; }}
  tr a-z A-Z < "$f" > "$f.tmp" && mv "$f.tmp" "$f"
  pail add "$f"
done < <(pail diff --staged --name-only -z --diff-filter=AM)"#));
    hook(&hooks, "commit-msg", &format!("echo commit-msg >> {l}\nprintf '\\nHook-Checked: yes\\n' >> \"$1\""));
    hook(&hooks, "pre-publish", &format!(r#"read -r branch old new; echo "pre-publish $1 $2 $branch $old $new" >> {l}
[[ "${{BLOCK_PUBLISH:-0}}" != 1 ]]"#));
    hook(&hooks, "post-commit", &format!("echo post-commit >> {l}"));
    hook(&hooks, "post-checkout", &format!("echo \"post-checkout $1 $2 $3 orig=$(pail rev-parse ORIG_HEAD 2>/dev/null || echo none)\" >> {l}"));
    hook(&hooks, "post-sync", &format!("echo \"post-sync $1 $2\" >> {l}"));
    hook(&hooks, "post-rewrite", &format!("echo \"post-rewrite $1 $(cat | tr '\\n' ';')\" >> {l}"));
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
    let out = cmd(&a, &["commit", "-m", "add a"]).env("PAIL_TRACE_HOOKS", &trace).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(pail(&a, &["cat", "src/a.txt"]), "HELLO");
    assert_eq!(std::fs::read_to_string(a.join("src/a.txt")).unwrap(), "HELLO");
    assert!(pail(&a, &["status"]).contains("nothing to commit"));
    let msg = pail(&a, &["log", "-1", "--format=%B"]);
    assert!(msg.contains("add a\n\nHook-Checked: yes"), "{msg}");
    let head = pail(&a, &["rev-parse", "HEAD"]).trim().to_string();
    let h = read_log();
    assert!(h.contains(&format!("pre-commit pre-commit main dir=unset cwd={}", a.display())), "{h}");
    assert!(h.contains(&format!("pre-publish main commit main {} {head}", "0".repeat(64))) || h.contains(&format!("{head}\n")), "{h}");
    let order: Vec<&str> = h.lines().map(|l| l.split(' ').next().unwrap()).collect();
    assert_eq!(order, ["pre-commit", "commit-msg", "pre-publish", "post-commit"], "{h}");
    let tr = std::fs::read_to_string(&trace).unwrap();
    assert!(tr.lines().any(|l| l.contains(r#""hook":"pre-commit""#) && l.contains(r#""exit":0"#)), "{tr}");

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
    let out = cmd(&a, &["commit", "-m", "b"]).env("BLOCK_PUBLISH", "1").output().unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(pail(&a, &["rev-parse", "main"]).trim(), head);
    assert!(!read_log().contains("post-commit"));

    // 4. --no-verify skips pre-commit, commit-msg and pre-publish (post-commit still runs).
    clear_log();
    let out = cmd(&a, &["commit", "-n", "-m", "b unchecked"]).env("BLOCK_PUBLISH", "1").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
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
    assert_eq!(read_log(), format!("post-checkout {feature} {head2} 1 orig={feature}\n"));

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
    assert!(h.contains(&format!("pre-publish main commit main {bob_head} {published}")), "{h}");
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
    assert!(h.contains(&format!("pre-publish main land main {tip} ")), "{h}");
    assert!(h.contains(&format!("post-rewrite land {feature} ")), "{h}");
    // A squash land is guarded the same way.
    pail(&a, &["switch", "-c", "squash"]);
    write(&a.join("src/s.txt"), "s");
    pail(&a, &["add", "src/s.txt"]);
    pail(&a, &["commit", "-n", "-m", "s"]);
    let main_tip = pail(&a, &["rev-parse", "main"]).trim().to_string();
    let out = cmd(&a, &["land", "squash", "--onto", "main"]).env("BLOCK_PUBLISH", "1").output().unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stderr));
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
    assert!(top.contains("Branches and landing") && top.contains("Exit codes:"), "{top}");
    assert_eq!(pail(&t, &[]), top, "bare `pail` prints the help");
    let land = pail(&t, &["land", "--help"]);
    assert!(land.contains("Examples:") && land.contains("pail land feature --stack"), "{land}");
    assert!(pail(&t, &["help", "worktree", "add"]).contains("Usage: pail worktree add"));
    assert!(pail(&a, &["log", "--help"]).contains("Usage: pail log"));
    let bare_group = cmd(&t, &["worktree"]).output().unwrap();
    assert!(String::from_utf8_lossy(&bare_group.stderr).contains("Commands:"));

    let skill = pail(&t, &["--skill"]);
    assert!(skill.starts_with("---\nname: pail\ndescription: "), "{skill}");
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
    let out = cmd(&a, &["--", "pail", "config", "core.ho"]).env("COMPLETE", "bash").env("_CLAP_COMPLETE_INDEX", "2").env("_CLAP_IFS", "\n").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "core.hooksPath");

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
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let auth = t.join("auth.sh");
    write(&auth, "#!/bin/sh\ncase \"$PAIL_AUTH_TOKEN:$PAIL_AUTH_REPO:$PAIL_AUTH_OP\" in\n  tok-alice:team/app:*) echo alice;;\n  tok-bob:team/app:read) echo bob;;\n  *) exit 1;;\nesac\n");
    let calls = t.join("helper-calls");
    // The company's provider, shipped as an extension: `describe`, and `get` answering with
    // the token the user holds (TOK stands in for their login).
    let helper = t.join("pail-credential-acme");
    write(&helper, &format!("#!/bin/sh\n[ \"$1\" = describe ] && {{ echo acme sso; exit 0; }}\nread req\necho \"$req\" >> {}\necho \"{{\\\"token\\\":\\\"$TOK\\\",\\\"expires_at\\\":4102444800}}\"\n", calls.display()));
    for f in [&auth, &helper] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _server = Child(
        cmd(&t, &["serve", "--store", &format!("file://{}", t.join("bucket").display()), "--addr", &format!("127.0.0.1:{port}"), "--auth-command", &auth.to_string_lossy()])
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
        c.env("TOK", token).env("XDG_CACHE_HOME", t.join(format!("cache-{token}"))).env("PAIL_EXTENSIONS_DIR", &ext);
        c.output().unwrap()
    };
    let ok = |o: std::process::Output| {
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).to_string()
    };

    // Install the provider and point the proxy's URL at it.
    let out = ok(as_user(&t, "tok-alice", &["auth", "install", &helper.to_string_lossy()]));
    assert!(out.contains("installed acme") && out.contains("acme sso"), "{out}");
    assert!(ok(as_user(&t, "tok-alice", &["auth", "list"])).contains("acme             acme sso"));
    ok(as_user(&t, "tok-alice", &["auth", "use", "acme", "--for", &base, "--global"]));
    ok(as_user(&t, "tok-alice", &["init", &store]));
    ok(as_user(&t, "tok-alice", &["clone", &store, "a"]));
    let a = t.join("a");
    write(&a.join("src/lib.rs"), "fn main() {}");
    ok(as_user(&a, "tok-alice", &["add", "-A"]));
    ok(as_user(&a, "tok-alice", &["commit", "-m", "through the proxy"]));
    assert!(t.join("bucket/team/app/refs/main").is_file(), "objects land in the backing store");
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
    assert!(String::from_utf8_lossy(&out.stderr).contains("403"), "{}", String::from_utf8_lossy(&out.stderr));

    // Unknown token: nothing.
    let out = as_user(&t, "tok-eve", &["clone", &store, "e"]);
    assert!(!out.status.success());

    // The helper got the pail-credential/1 request, and its answer was reused from the cache.
    let reqs = std::fs::read_to_string(&calls).unwrap();
    assert!(reqs.contains(r#""protocol":"pail-credential/1""#) && reqs.contains(&store), "{reqs}");
    let alice_calls = reqs.lines().count();
    ok(as_user(&a, "tok-alice", &["log", "-1"]));
    assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), alice_calls, "cached");
    assert!(ok(as_user(&a, "tok-alice", &["auth", "logout"])).contains("forgot"));
}
