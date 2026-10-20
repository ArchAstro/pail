use std::path::Path;
use std::process::Command;

/// One machine-local pail home (blob store, worktree registry) shared by this test process.
fn home() -> &'static Path {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

fn cmd(dir: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_pail"));
    c.args(args)
        .current_dir(dir)
        .env("PAIL_AUTHOR", "Tester <t@example.com>")
        .env("PAIL_CACHE", "off")
        .env("PAIL_HOME", home())
        .env("PAIL_BLOBS", home().join("blobs"))
        .env_remove("PAIL_CLONE");
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

fn blob_path(content: &[u8]) -> std::path::PathBuf {
    let hex = pail::hash::Hash::of(content).hex();
    home().join("blobs").join(&hex[..2]).join(hex)
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
    assert!(blob_path(lib.as_bytes()).exists() && blob_path(&big).exists());
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
    let stored = blob_path(lib.as_bytes());
    assert_eq!(pail::hash::Hash::of(&std::fs::read(&stored).unwrap()), pail::hash::Hash::of(lib.as_bytes()));
    assert!(pail(&b, &["status"]).contains("M src/lib.rs"));

    // Committed content goes into the blob store for the next worktree to clone.
    write(&b.join("new.txt"), &"fresh content ".repeat(10));
    pail(&b, &["add", "new.txt"]);
    pail(&b, &["commit", "-m", "new"]);
    assert!(blob_path("fresh content ".repeat(10).as_bytes()).exists());

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

    // A private cache so this test sees only its own content.
    let home = t.join("home");
    let run = |args: &[&str]| -> std::process::Output {
        let mut full = vec!["--store", bare_s];
        full.extend_from_slice(args);
        cmd(&t, &full).env("PAIL_HOME", &home).env("PAIL_BLOBS", home.join("blobs")).output().unwrap()
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
}
