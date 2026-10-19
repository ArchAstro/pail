use std::path::Path;
use std::process::Command;

fn pail(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_pail"))
        .args(args)
        .current_dir(dir)
        .env("PAIL_AUTHOR", "Tester <t@example.com>")
        .env("PAIL_CACHE", "off")
        .output()
        .unwrap();
    let s = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "pail {args:?} failed:\n{s}");
    s
}

fn pail_fails(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_pail")).args(args).current_dir(dir).env("PAIL_CACHE", "off").output().unwrap();
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
    let log = pail(&a, &["log"]);
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
