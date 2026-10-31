//! `pail events` (`src/events.rs`; the design is modelled in `verify/events/`, and
//! `tests/events_refinement.rs` checks the watcher step by step). These run the real `watch`
//! loop and the command: resuming from a cursor, the safety poll, injected hints, objects of
//! the sibling area, a crash between the line and the cursor, and the bucket cursor's CAS.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use pail::events::{Cursor, Event, Hint, HintSource, Source, Spec, watch};
use pail::hash::Hash;
use pail::repo::Repo;
use pail::store::Store;
use tokio::sync::mpsc;

struct Env {
    _dir: tempfile::TempDir,
    repo: Repo,
    area: Store,
    cursor: std::path::PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file://{}", dir.path().join("repo").display());
    let repo = Repo::with_store(Store::open(&url).unwrap(), None);
    let area = Store::open(&format!("{url}.actions")).unwrap();
    let cursor = dir.path().join("cursor.json");
    Env {
        _dir: dir,
        repo,
        area,
        cursor,
    }
}

async fn set_ref(repo: &Repo, name: &str, n: u32) -> String {
    let h = Hash::of(format!("{name}/{n}").as_bytes());
    repo.store
        .put(
            &format!("refs/{name}"),
            format!(r#"{{"commit":"{}"}}"#, h.hex()).into(),
        )
        .await
        .unwrap();
    h.hex()
}

/// Hints a test sends by hand.
struct Channel(mpsc::UnboundedReceiver<Vec<Hint>>);

#[async_trait]
impl HintSource for Channel {
    fn name(&self) -> &'static str {
        "hint"
    }
    async fn pull(&mut self) -> anyhow::Result<Vec<Hint>> {
        match self.0.recv().await {
            Some(h) => Ok(h),
            None => std::future::pending().await,
        }
    }
    async fn ack(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

fn spec(refs: bool, prefix: Option<&str>) -> Spec {
    Spec {
        globs: vec![],
        refs,
        prefix: prefix.map(String::from),
    }
}

fn json(e: &Event) -> serde_json::Value {
    serde_json::to_value(e).unwrap()
}

/// Run `watch` until it has produced `n` events (or `wait` passes), let it finish saving the
/// cursor, stop it, and return what it wrote.
async fn collect(
    repo: &Repo,
    source: Source,
    spec: &Spec,
    cursor: Option<Cursor>,
    safety: Duration,
    n: usize,
    wait: Duration,
) -> Vec<serde_json::Value> {
    let got = std::sync::Mutex::new(vec![]);
    let task = watch(repo, source, spec, cursor, false, safety, |e| {
        got.lock().unwrap().push(json(&e));
        Ok(())
    });
    let enough = async {
        let start = std::time::Instant::now();
        while got.lock().unwrap().len() < n && start.elapsed() < wait {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // a batch may still be saving its cursor
        tokio::time::sleep(Duration::from_millis(if n > 0 { 150 } else { 0 })).await;
    };
    tokio::select! {
        r = task => panic!("watch ended: {r:?}"),
        _ = enough => {}
    }
    got.into_inner().unwrap()
}

#[tokio::test]
async fn a_restart_emits_what_changed_while_down_deletions_included() {
    let e = env();
    let a1 = set_ref(&e.repo, "a", 1).await;
    set_ref(&e.repo, "b", 1).await;
    let cursor = || Some(Cursor::File(e.cursor.clone()));
    let poll = || Source::Poll {
        interval: Duration::from_millis(50),
    };
    // the first run with a new cursor emits what exists
    let first = collect(
        &e.repo,
        poll(),
        &spec(true, None),
        cursor(),
        Duration::ZERO,
        2,
        Duration::from_secs(5),
    )
    .await;
    let mut names: Vec<_> = first
        .iter()
        .map(|v| v["branch"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["a", "b"]);
    assert!(first.iter().all(|v| v["source"] == "resume"));
    // while no one watches: a moves, b goes, c appears
    let a2 = set_ref(&e.repo, "a", 2).await;
    e.repo.store.delete("refs/b").await.unwrap();
    let c1 = set_ref(&e.repo, "c", 1).await;
    let second = collect(
        &e.repo,
        poll(),
        &spec(true, None),
        cursor(),
        Duration::ZERO,
        3,
        Duration::from_secs(5),
    )
    .await;
    let by = |b: &str| second.iter().find(|v| v["branch"] == b).unwrap().clone();
    assert_eq!(second.len(), 3, "{second:?}");
    assert_eq!(by("a")["commit"], a2);
    assert_eq!(by("a")["old"], a1);
    assert!(by("b")["commit"].is_null(), "{:?}", by("b"));
    assert_eq!(by("c")["commit"], c1);
    // and a third start finds nothing: the cursor took them
    let third = collect(
        &e.repo,
        poll(),
        &spec(true, None),
        cursor(),
        Duration::ZERO,
        1,
        Duration::from_millis(400),
    )
    .await;
    assert!(third.is_empty(), "{third:?}");
}

#[tokio::test]
async fn a_lost_notification_only_delays_the_event() {
    let e = env();
    // no hint is ever sent: the safety poll finds the ref
    let (tx, rx) = mpsc::unbounded_channel::<Vec<Hint>>();
    let repo = &e.repo;
    let sp = spec(true, None);
    let task = collect(
        repo,
        Source::Hints(Box::new(Channel(rx))),
        &sp,
        None,
        Duration::from_millis(100),
        1,
        Duration::from_secs(5),
    );
    let writer = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        set_ref(repo, "y", 1).await
    };
    let (got, y) = tokio::join!(task, writer);
    drop(tx);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["branch"], "y");
    assert_eq!(got[0]["commit"], y);
    assert_eq!(got[0]["source"], "poll");
}

#[tokio::test]
async fn a_hint_is_re_read_not_believed() {
    let e = env();
    let (tx, rx) = mpsc::unbounded_channel::<Vec<Hint>>();
    let area = e.area.clone();
    let repo = &e.repo;
    let sp = spec(true, Some("actions/queue/"));
    let task = collect(
        repo,
        Source::Hints(Box::new(Channel(rx))),
        &sp,
        None,
        Duration::ZERO,
        2,
        Duration::from_secs(5),
    );
    let driver = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        area.put("queue/a.json", "1".into()).await.unwrap();
        // the hints are reordered, duplicated, and name keys that are not there or not watched
        tx.send(vec![
            Hint::Object("queue/a.json".into()),
            Hint::Object("queue/a.json".into()),
            Hint::Object("queue/ghost.json".into()),
            Hint::Object("leases/other".into()),
        ])
        .unwrap();
        tx.send(vec![Hint::Object("queue/a.json".into())]).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        area.delete("queue/a.json").await.unwrap();
        tx.send(vec![Hint::Object("queue/a.json".into())]).unwrap();
    };
    let (got, _) = tokio::join!(task, driver);
    assert_eq!(got.len(), 2, "one put, one delete: {got:?}");
    assert_eq!(got[0]["op"], "put");
    assert_eq!(got[0]["key"], "queue/a.json");
    assert_eq!(got[0]["source"], "hint");
    assert!(got[0]["old"].is_null());
    assert_eq!(got[1]["op"], "delete");
    assert!(got[1]["version"].is_null());
    assert_eq!(got[1]["old"], got[0]["version"]);
}

#[tokio::test]
async fn a_crash_between_the_line_and_the_cursor_repeats_the_line() {
    let e = env();
    let h = set_ref(&e.repo, "a", 1).await;
    let cursor = || Some(Cursor::File(e.cursor.clone()));
    // the sink writes the line, then the process dies (the sink errs before the cursor moves)
    let mut lines = vec![];
    let r = watch(
        &e.repo,
        Source::Poll {
            interval: Duration::from_millis(50),
        },
        &spec(true, None),
        cursor(),
        false,
        Duration::ZERO,
        |ev| {
            lines.push(json(&ev));
            anyhow::bail!("killed");
        },
    )
    .await;
    assert!(r.is_err());
    assert_eq!(lines.len(), 1);
    let on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&e.cursor).unwrap()).unwrap();
    assert_eq!(on_disk["dirty"], serde_json::json!(["ref:a"]));
    assert!(
        on_disk["heads"].as_object().unwrap().is_empty(),
        "{on_disk}"
    );
    // the key goes back to nothing while the consumer is down
    e.repo.store.delete("refs/a").await.unwrap();
    let again = collect(
        &e.repo,
        Source::Poll {
            interval: Duration::from_millis(50),
        },
        &spec(true, None),
        cursor(),
        Duration::ZERO,
        1,
        Duration::from_secs(5),
    )
    .await;
    // the readers saw a put: the restart must say it is gone, though the cursor knows nothing of it
    assert_eq!(again.len(), 1, "{again:?}");
    assert_eq!(again[0]["branch"], "a");
    assert!(again[0]["commit"].is_null());
    assert_eq!(lines[0]["commit"], h);
}

#[tokio::test]
async fn the_bucket_cursor_is_compare_and_swapped_and_never_an_event() {
    let e = env();
    let bucket = |e: &Env| Cursor::Bucket {
        store: e.area.clone(),
        key: "cursors/o.json".into(),
        version: None,
    };
    e.area.put("queue/a", "1".into()).await.unwrap();
    let got = collect(
        &e.repo,
        Source::Poll {
            interval: Duration::from_millis(50),
        },
        &spec(false, Some("actions/")),
        Some(bucket(&e)),
        Duration::ZERO,
        2,
        Duration::from_millis(800),
    )
    .await;
    // the area holds the cursor now, and it is not reported
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["key"], "queue/a");
    assert!(e.area.get_opt("cursors/o.json").await.unwrap().is_some());
    // a second consumer on the same cursor object is refused when it saves over the first
    let mut c1 = bucket(&e);
    let mut c2 = bucket(&e);
    let mut d = c1.load().await.unwrap();
    c2.load().await.unwrap();
    let theirs = d.clone();
    d.heads.insert("obj:z".into(), "1".into());
    c1.save(&d).await.unwrap();
    let d = theirs;
    let err = c2.save(&d).await.unwrap_err();
    assert!(err.to_string().contains("another consumer"), "{err}");
}

fn pail() -> std::process::Command {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_pail"));
    c.env("PAIL_AUTHOR", "Tester <t@example.com>")
        .env_remove("PAIL_CLONE")
        .env_remove("PAIL_STORE")
        .env_remove("PAIL_DIR");
    c
}

fn read_lines(child: &mut std::process::Child, n: usize) -> Vec<serde_json::Value> {
    use std::io::BufRead;
    let out = child.stdout.as_mut().unwrap();
    let mut r = std::io::BufReader::new(out);
    (0..n)
        .map(|_| {
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            serde_json::from_str(&l).unwrap_or_else(|e| panic!("{e}: {l:?}"))
        })
        .collect()
}

/// The command: flags, JSON lines, the cursor file, a resume after the process is killed.
#[test]
fn the_command_resumes_from_its_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let t = std::fs::canonicalize(dir.path()).unwrap();
    let store = format!("file://{}", t.join("repo").display());
    let run = |args: &[&str]| {
        let o = pail()
            .current_dir(&t)
            .env("PAIL_CONFIG_GLOBAL", t.join("cfg"))
            .args(["--store", &store])
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };
    let init = pail()
        .current_dir(&t)
        .env("PAIL_CONFIG_GLOBAL", t.join("cfg"))
        .args(["init", &store])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let cursor = t.join("c.json");
    let events = |t: &Path| {
        pail()
            .current_dir(t)
            .env("PAIL_CONFIG_GLOBAL", t.join("cfg"))
            .args([
                "--store",
                &store,
                "events",
                "--interval-ms",
                "50",
                "--branch",
                "_*",
            ])
            .args(["--cursor", cursor.to_str().unwrap()])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let put = |file: &str, rec: &str| {
        std::fs::write(t.join("rec.json"), rec).unwrap();
        run(&[
            "put",
            "_meta",
            "-m",
            "rec",
            "--file",
            &format!("{file}={}", t.join("rec.json").display()),
        ]);
    };
    put("a.json", "1");
    let mut first = events(&t);
    let got = read_lines(&mut first, 1);
    assert_eq!(got[0]["branch"], "_meta");
    // the cursor moves just after the line is out: wait for it, then kill the process
    for _ in 0..200 {
        let saved = std::fs::read_to_string(&cursor).unwrap_or_default();
        if saved.contains("ref:_meta") && !saved.contains("dirty") {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    first.kill().unwrap();
    first.wait().unwrap();
    // changes while it is down
    put("b.json", "2");
    let mut second = events(&t);
    let got = read_lines(&mut second, 1);
    second.kill().unwrap();
    second.wait().unwrap();
    assert_eq!(got[0]["branch"], "_meta");
    assert_eq!(got[0]["source"], "resume");
    assert!(
        !got[0]["old"].is_null(),
        "it knew _meta from the cursor: {got:?}"
    );
}
