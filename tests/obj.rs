//! `pail obj`: single objects in the sibling area, driven through the binary on a file://
//! store, the in-process GCS emulator, and any store in `PAIL_TEST_STORES` (CI's moto and
//! emulator jobs).

use std::io::Write;
use std::process::{Command, Output, Stdio};

use pail::gcs_emulator::GcsEmulator;

struct Env {
    store: String,
    gcs_base: Option<String>,
    dir: tempfile::TempDir,
}

impl Env {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_pail"));
        c.arg("--store")
            .arg(&self.store)
            .args(args)
            .current_dir(self.dir.path())
            .env_remove("PAIL_STORE")
            .env_remove("PAIL_DIR");
        if let Some(b) = &self.gcs_base {
            c.env("PAIL_GCS_BASE_URL", b);
        }
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap()
    }
    fn code(&self, args: &[&str]) -> i32 {
        self.run(args).status.code().unwrap()
    }
    fn file(&self, name: &str, body: &str) -> String {
        let p = self.dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        p.to_str().unwrap().to_string()
    }
    fn put_stdin(&self, args: &[&str], body: &str) -> Output {
        let mut ch = self
            .cmd(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        ch.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
        ch.wait_with_output().unwrap()
    }
}

fn suite(env: &Env) {
    let a = env.file("a.json", "{\"n\":1}");
    let b = env.file("b.json", "{\"n\":2}");

    // get of an absent key: exit 3, message on stderr, nothing on stdout.
    let o = env.run(&["obj", "get", "queue/x/1.1.test.json"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(o.stdout.is_empty() && !o.stderr.is_empty());

    // create race: two --if-absent, exactly one wins.
    let results: Vec<Output> = std::thread::scope(|s| {
        let hs: Vec<_> = [&a, &b]
            .into_iter()
            .map(|f| s.spawn(move || env.run(&["obj", "put", "leases/r/1/j", f, "--if-absent"])))
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let codes: Vec<_> = results.iter().map(|o| o.status.code().unwrap()).collect();
    assert_eq!(codes.iter().filter(|c| **c == 0).count(), 1, "{codes:?}");
    assert_eq!(codes.iter().filter(|c| **c == 4).count(), 1, "{codes:?}");

    // put prints a token; --if-version with it succeeds, then the old token is stale.
    let t1 = env
        .ok(&["obj", "put", "queue/x/1.1.test.json", &a])
        .trim()
        .to_string();
    assert!(!t1.is_empty() && !t1.contains('\n'));
    assert_eq!(
        env.ok(&["obj", "get", "queue/x/1.1.test.json"]),
        "{\"n\":1}"
    );
    let t2 = env
        .ok(&[
            "obj",
            "put",
            "queue/x/1.1.test.json",
            &b,
            "--if-version",
            &t1,
        ])
        .trim()
        .to_string();
    assert_ne!(t1, t2);
    assert_eq!(
        env.code(&[
            "obj",
            "put",
            "queue/x/1.1.test.json",
            &a,
            "--if-version",
            &t1
        ]),
        4
    );
    assert_eq!(
        env.ok(&["obj", "get", "queue/x/1.1.test.json"]),
        "{\"n\":2}"
    );
    // --if-absent on an existing key.
    assert_eq!(
        env.code(&["obj", "put", "queue/x/1.1.test.json", &a, "--if-absent"]),
        4
    );
    // --if-version on an absent key.
    assert_eq!(
        env.code(&["obj", "put", "nope/k", &a, "--if-version", &t1]),
        4
    );

    // stdin body; get -o prints the version, which matches put's token.
    let o = env.put_stdin(&["obj", "put", "queue/x/2.1.test.json", "-"], "from stdin");
    assert!(o.status.success());
    let t3 = String::from_utf8(o.stdout).unwrap().trim().to_string();
    let out = env.dir.path().join("got");
    let v = env.ok(&[
        "obj",
        "get",
        "queue/x/2.1.test.json",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "from stdin");
    assert_eq!(v.trim(), t3);

    // ls: JSON shape, sorted by key, prefix scoped; a token from ls works in --if-version.
    env.ok(&["obj", "put", "other/z", &a]);
    let ls = env.ok(&["obj", "ls", "queue"]);
    let rows: Vec<serde_json::Value> = ls
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let keys: Vec<_> = rows.iter().map(|r| r["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["queue/x/1.1.test.json", "queue/x/2.1.test.json"]);
    assert_eq!(rows[0]["size"], 7);
    assert!(rows[0]["modified"].as_i64().unwrap() > 1_600_000_000_000);
    assert_eq!(rows[0]["version"], t2.as_str());
    assert_eq!(rows[1]["version"], t3.as_str());
    let whole = env.ok(&["obj", "ls", ""]);
    assert_eq!(whole.lines().count(), 4); // leases/r/1/j, other/z, two queue
    assert!(env.ok(&["obj", "ls", "empty"]).is_empty());
    let tok = rows[0]["version"].as_str().unwrap();
    env.ok(&[
        "obj",
        "put",
        "queue/x/1.1.test.json",
        &a,
        "--if-version",
        tok,
    ]);

    // rm: stale token refuses, current token deletes, absent is success unless conditional.
    assert_eq!(
        env.code(&["obj", "rm", "queue/x/1.1.test.json", "--if-version", tok]),
        4
    );
    let cur = env.ok(&[
        "obj",
        "get",
        "queue/x/1.1.test.json",
        "-o",
        out.to_str().unwrap(),
    ]);
    env.ok(&[
        "obj",
        "rm",
        "queue/x/1.1.test.json",
        "--if-version",
        cur.trim(),
    ]);
    assert_eq!(env.code(&["obj", "get", "queue/x/1.1.test.json"]), 3);
    env.ok(&["obj", "rm", "queue/x/1.1.test.json"]);
    env.ok(&["obj", "rm", "never/was"]);
    assert_eq!(
        env.code(&["obj", "rm", "never/was", "--if-version", cur.trim()]),
        4
    );

    // areas are separate namespaces.
    env.ok(&["obj", "--area", "scratch", "put", "k", &a]);
    assert_eq!(env.code(&["obj", "get", "k"]), 3);
    assert_eq!(
        env.ok(&["obj", "--area", "scratch", "get", "k"]),
        "{\"n\":1}"
    );

    // bad keys and tokens are plain errors (exit 1).
    assert_eq!(env.code(&["obj", "get", "../x"]), 1);
    assert_eq!(env.code(&["obj", "put", "k", &a, "--if-version", "!!"]), 1);
}

fn env_for(store: String, gcs_base: Option<String>) -> Env {
    Env {
        store,
        gcs_base,
        dir: tempfile::tempdir().unwrap(),
    }
}

#[test]
fn file_store() {
    let d = tempfile::tempdir().unwrap();
    let e = env_for(format!("file://{}", d.path().join("repo").display()), None);
    suite(&e);
}

#[test]
fn gcs_emulator_store() {
    let emu = GcsEmulator::start("127.0.0.1:0").unwrap();
    let e = env_for(
        format!("gs://pail/obj{}/repo", std::process::id()),
        Some(emu.url().to_string()),
    );
    suite(&e);
}

#[test]
fn configured_stores() {
    let Ok(urls) = std::env::var("PAIL_TEST_STORES") else {
        return;
    };
    for url in urls.split_whitespace() {
        let e = env_for(format!("{url}/obj{}/repo", std::process::id()), None);
        suite(&e);
    }
}

#[test]
fn http_store_is_refused() {
    let e = env_for("https://example.invalid/repo".into(), None);
    let o = e.run(&["obj", "get", "k"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("sibling area"));
}
