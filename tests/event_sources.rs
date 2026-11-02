//! The queue sources of `pail events` (`src/event_sources.rs`) against in-process HTTP fakes
//! of SQS, an Azure Storage Queue and the Cloudflare Queues pull API. No cloud is called.
//!
//! What they pin down, for each of the three:
//! - a message naming a ref is turned into an event, and its message is deleted only after
//!   the line is out and the cursor is saved (the fake looks at the cursor file when the
//!   delete arrives);
//! - a message that does not parse still makes the watcher re-list, and is deleted too;
//! - a message for another bucket, or a key outside the repository, is deleted and ignored.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pail::creds::AwsKeys;
use pail::event_sources::{
    AwsAuth, AzureQueue, Bearer, Names, QueueSource, R2Queue, SqsQueue, parse_event_grid, parse_r2,
    parse_s3,
};
use pail::events::{Cursor, Source, Spec, watch};
use pail::hash::Hash;
use pail::repo::Repo;
use pail::store::Store;

// ---------- a fake HTTP service ----------

struct Req {
    method: String,
    /// Path and query.
    url: String,
    headers: HashMap<String, String>,
    body: String,
}

struct Fake {
    base: String,
    server: Arc<tiny_http::Server>,
    thread: Option<std::thread::JoinHandle<()>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Fake {
    /// Serve `handler`: `(status, body)` for each request.
    fn start(handler: impl Fn(&Req) -> (u16, String) + Send + 'static) -> Fake {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s, st) = (server.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !st.load(std::sync::atomic::Ordering::Relaxed) {
                let Ok(Some(mut r)) = s.recv_timeout(Duration::from_millis(50)) else {
                    continue;
                };
                let mut body = String::new();
                r.as_reader().read_to_string(&mut body).unwrap();
                let req = Req {
                    method: r.method().to_string(),
                    url: r.url().to_string(),
                    headers: r
                        .headers()
                        .iter()
                        .map(|h| {
                            (
                                h.field.as_str().as_str().to_ascii_lowercase(),
                                h.value.to_string(),
                            )
                        })
                        .collect(),
                    body,
                };
                let (status, out) = handler(&req);
                let _ = r.respond(tiny_http::Response::from_string(out).with_status_code(status));
            }
        });
        Fake {
            base,
            server,
            thread: Some(thread),
            stop,
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What the fake queues share with the test: the messages to hand out, what was deleted,
/// and what the cursor held at each delete.
#[derive(Default)]
struct Shared {
    pending: Mutex<VecDeque<(String, String)>>,
    /// Handed out, not yet deleted.
    inflight: Mutex<Vec<String>>,
    /// `(handle, the cursor file's text at that moment)`.
    deleted: Mutex<Vec<(String, String)>>,
    cursor: std::path::PathBuf,
}

impl Shared {
    fn send(&self, body: &str) {
        let mut p = self.pending.lock().unwrap();
        let n = p.len() + self.inflight.lock().unwrap().len() + self.deleted.lock().unwrap().len();
        p.push_back((format!("h{n}"), body.to_string()));
    }
    fn take(&self) -> Vec<(String, String)> {
        let batch: Vec<_> = self.pending.lock().unwrap().drain(..).collect();
        self.inflight
            .lock()
            .unwrap()
            .extend(batch.iter().map(|(h, _)| h.clone()));
        batch
    }
    fn delete(&self, handle: &str) {
        let cursor = std::fs::read_to_string(&self.cursor).unwrap_or_default();
        self.inflight.lock().unwrap().retain(|h| h != handle);
        self.deleted
            .lock()
            .unwrap()
            .push((handle.to_string(), cursor));
    }
}

fn sqs_fake(sh: Arc<Shared>) -> Fake {
    Fake::start(move |r| {
        let auth = r.headers.get("authorization").cloned().unwrap_or_default();
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKID/")
                && auth.contains("/us-east-1/sqs/aws4_request"),
            "{auth}"
        );
        assert_eq!(r.headers["x-amz-security-token"], "session");
        let body: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        match r.headers["x-amz-target"].as_str() {
            "AmazonSQS.ReceiveMessage" => {
                assert!(
                    body["QueueUrl"]
                        .as_str()
                        .unwrap()
                        .ends_with("/000000000000/pail-ci")
                );
                let msgs: Vec<_> = sh
                    .take()
                    .into_iter()
                    .map(
                        |(h, b)| serde_json::json!({"MessageId": h, "ReceiptHandle": h, "Body": b}),
                    )
                    .collect();
                if msgs.is_empty() {
                    std::thread::sleep(Duration::from_millis(30));
                    return (200, "{}".into());
                }
                (200, serde_json::json!({"Messages": msgs}).to_string())
            }
            "AmazonSQS.DeleteMessageBatch" => {
                for e in body["Entries"].as_array().unwrap() {
                    sh.delete(e["ReceiptHandle"].as_str().unwrap());
                }
                (200, r#"{"Successful":[],"Failed":[]}"#.into())
            }
            t => panic!("unexpected target {t}"),
        }
    })
}

fn azure_fake(sh: Arc<Shared>) -> Fake {
    Fake::start(move |r| {
        assert!(r.url.starts_with("/pail-ci/messages"), "{}", r.url);
        assert!(
            r.url.contains("sig=SECRET"),
            "the SAS token rides along: {}",
            r.url
        );
        assert!(r.headers.contains_key("x-ms-version"));
        if r.method == "GET" {
            let msgs: String = sh
                .take()
                .into_iter()
                .map(|(h, b)| {
                    let esc = b.replace('&', "&amp;").replace('<', "&lt;").replace('"', "&quot;");
                    format!(
                        "<QueueMessage><MessageId>{h}</MessageId><PopReceipt>r-{h}</PopReceipt><MessageText>{esc}</MessageText></QueueMessage>"
                    )
                })
                .collect();
            return (
                200,
                format!(
                    "<?xml version=\"1.0\" encoding=\"utf-8\"?><QueueMessagesList>{msgs}</QueueMessagesList>"
                ),
            );
        }
        assert_eq!(r.method, "DELETE");
        let id = r.url["/pail-ci/messages/".len()..]
            .split('?')
            .next()
            .unwrap();
        assert!(r.url.contains(&format!("popreceipt=r-{id}")), "{}", r.url);
        sh.delete(id);
        (204, String::new())
    })
}

fn r2_fake(sh: Arc<Shared>) -> Fake {
    Fake::start(move |r| {
        assert_eq!(r.headers["authorization"], "Bearer cf-token");
        let ok = |v: serde_json::Value| (200, v.to_string());
        if r.method == "GET" {
            assert!(r.url.starts_with("/accounts/acct/queues"), "{}", r.url);
            return ok(serde_json::json!({"success": true, "result": [
                {"queue_name": "other", "queue_id": "0".repeat(32)},
                {"queue_name": "pail-ci", "queue_id": "q".repeat(32)}]}));
        }
        let id = "q".repeat(32);
        if r.url == format!("/accounts/acct/queues/{id}/messages/pull") {
            let msgs: Vec<_> = sh
                .take()
                .into_iter()
                // a JSON body arrives as an object for some producers, a string for others
                .map(|(h, b)| {
                    let body = if h.ends_with('0') {
                        serde_json::from_str(&b).unwrap_or(serde_json::Value::String(b))
                    } else {
                        serde_json::Value::String(b)
                    };
                    serde_json::json!({"id": h, "lease_id": h, "body": body, "attempts": 1})
                })
                .collect();
            return ok(serde_json::json!({"success": true, "errors": [],
                "result": {"message_backlog_count": 0, "messages": msgs}}));
        }
        assert_eq!(r.url, format!("/accounts/acct/queues/{id}/messages/ack"));
        let body: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        for a in body["acks"].as_array().unwrap() {
            sh.delete(a["lease_id"].as_str().unwrap());
        }
        ok(serde_json::json!({"success": true, "result": {"ackCount": 1}}))
    })
}

// ---------- the store and the watcher ----------

struct Env {
    _dir: tempfile::TempDir,
    repo: Repo,
    cursor: std::path::PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file://{}", dir.path().join("repo").display());
    let repo = Repo::with_store(Store::open(&url).unwrap(), None);
    let cursor = dir.path().join("cursor.json");
    Env {
        _dir: dir,
        repo,
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

fn names() -> Names {
    Names {
        bucket: Some("acme".into()),
        refs: "team/app/refs/".into(),
        area: None,
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Sqs,
    Azure,
    R2,
}

impl Kind {
    fn queue_source(self, f: &Fake) -> QueueSource {
        match self {
            Kind::Sqs => QueueSource::new(
                "sqs",
                Box::new(
                    SqsQueue::new(
                        &format!("{}/000000000000/pail-ci", f.base),
                        AwsAuth::Fixed(AwsKeys {
                            access_key_id: "AKID".into(),
                            secret_access_key: "SECRET".into(),
                            session_token: Some("session".into()),
                        }),
                        0,
                    )
                    .unwrap(),
                ),
                names(),
                parse_s3,
            ),
            Kind::Azure => QueueSource::new(
                "azure",
                Box::new(
                    AzureQueue::new(
                        &format!("{}/pail-ci?sv=2022-11-02&sig=SECRET", f.base),
                        Bearer::None,
                        Duration::from_millis(20),
                    )
                    .unwrap(),
                ),
                names(),
                parse_event_grid,
            ),
            Kind::R2 => QueueSource::new(
                "r2",
                Box::new(R2Queue::new(
                    &f.base,
                    "acct",
                    "pail-ci",
                    Bearer::Fixed("cf-token".into()),
                    Duration::from_millis(20),
                )),
                names(),
                parse_r2,
            ),
        }
    }

    fn fake(self, sh: Arc<Shared>) -> Fake {
        match self {
            Kind::Sqs => sqs_fake(sh),
            Kind::Azure => azure_fake(sh),
            Kind::R2 => r2_fake(sh),
        }
    }

    /// A message of this service saying `action` happened to `key` of `bucket`.
    fn message(self, bucket: &str, key: &str) -> String {
        match self {
            Kind::Sqs => serde_json::json!({"Records": [{"eventSource": "aws:s3",
                "s3": {"bucket": {"name": bucket}, "object": {"key": key.replace('/', "%2F")}}}]})
            .to_string(),
            Kind::Azure => serde_json::json!({"eventType": "Microsoft.Storage.BlobCreated",
                "subject": format!("/blobServices/default/containers/{bucket}/blobs/{key}"),
                "data": {}})
            .to_string(),
            Kind::R2 => serde_json::json!({"account": "acct", "action": "PutObject",
                "bucket": bucket, "object": {"key": key}})
            .to_string(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Sqs => "sqs",
            Kind::Azure => "azure",
            Kind::R2 => "r2",
        }
    }
}

/// Run `watch` with the queue as its source and a cursor file until `done(events, shared)`
/// holds (or ten seconds pass); `act` runs once the watcher has started (its first, resume
/// events are in `events` already).
async fn run(
    kind: Kind,
    act: impl AsyncFnOnce(&Repo, &Shared),
    done: impl Fn(&[serde_json::Value], &Shared) -> bool,
) -> (Vec<serde_json::Value>, Arc<Shared>) {
    let e = env();
    let sh = Arc::new(Shared {
        cursor: e.cursor.clone(),
        ..Shared::default()
    });
    let fake = kind.fake(sh.clone());
    let source = Source::Hints(Box::new(kind.queue_source(&fake)));
    let spec = Spec {
        globs: vec![],
        refs: true,
        prefix: None,
    };
    let got = Mutex::new(vec![]);
    let task = watch(
        &e.repo,
        source,
        &spec,
        Some(Cursor::File(e.cursor.clone())),
        false,
        Duration::ZERO, // no safety poll: only the queue can wake it
        |ev| {
            got.lock().unwrap().push(serde_json::to_value(&ev).unwrap());
            Ok(())
        },
    );
    let driver = async {
        tokio::time::sleep(Duration::from_millis(300)).await; // started, resumed
        act(&e.repo, &sh).await;
        let start = std::time::Instant::now();
        loop {
            if done(&got.lock().unwrap(), &sh) || start.elapsed() > Duration::from_secs(10) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::select! {
        r = task => panic!("watch ended: {r:?}"),
        _ = driver => {}
    }
    drop(fake);
    let events = got.into_inner().unwrap();
    (events, sh)
}

// ---------- the tests ----------

async fn acks_only_after_the_cursor_is_saved(kind: Kind) {
    let commit = Arc::new(Mutex::new(String::new()));
    let c2 = commit.clone();
    let (events, sh) = run(
        kind,
        async move |repo, sh| {
            // the repo has a ref; the watcher's first cursor run saw nothing. Now it moves,
            // and the service says so.
            *c2.lock().unwrap() = set_ref(repo, "pr/12", 1).await;
            sh.send(&kind.message("acme", "team/app/refs/pr/12"));
        },
        |ev, sh| !ev.is_empty() && !sh.deleted.lock().unwrap().is_empty(),
    )
    .await;
    let commit = commit.lock().unwrap().clone();
    assert_eq!(events.len(), 1, "{kind:?}: {events:?}");
    assert_eq!(events[0]["branch"], "pr/12");
    assert_eq!(events[0]["commit"], commit.as_str());
    assert_eq!(events[0]["source"], kind.label());
    let deleted = sh.deleted.lock().unwrap();
    assert_eq!(deleted.len(), 1, "{kind:?}: deleted once");
    assert!(
        deleted[0].1.contains(&commit),
        "{kind:?}: the cursor held the commit when the message was deleted: {}",
        deleted[0].1
    );
}

async fn an_unparseable_message_still_scans(kind: Kind) {
    let commit = Arc::new(Mutex::new(String::new()));
    let c2 = commit.clone();
    let (events, sh) = run(
        kind,
        async move |repo, sh| {
            *c2.lock().unwrap() = set_ref(repo, "main", 1).await;
            sh.send("this is not a notification");
        },
        |ev, sh| !ev.is_empty() && !sh.deleted.lock().unwrap().is_empty(),
    )
    .await;
    assert_eq!(events.len(), 1, "{kind:?}: {events:?}");
    assert_eq!(events[0]["branch"], "main");
    assert_eq!(events[0]["commit"], commit.lock().unwrap().as_str());
    assert_eq!(
        sh.deleted.lock().unwrap().len(),
        1,
        "{kind:?}: and it is deleted"
    );
}

async fn messages_for_elsewhere_are_deleted_and_ignored(kind: Kind) {
    let (events, sh) = run(
        kind,
        async move |repo, sh| {
            set_ref(repo, "main", 1).await; // changed, but no message says so
            sh.send(&kind.message("other-bucket", "team/app/refs/main"));
            sh.send(&kind.message("acme", "team/app/commits/ab/cd"));
        },
        |_, sh| sh.deleted.lock().unwrap().len() == 2,
    )
    .await;
    assert!(
        events.is_empty(),
        "{kind:?}: nothing named main: {events:?}"
    );
    assert_eq!(sh.deleted.lock().unwrap().len(), 2);
}

macro_rules! per_service {
    ($($name:ident: $f:ident($kind:expr);)*) => {$(
        #[tokio::test]
        async fn $name() { $f($kind).await }
    )*};
}

per_service! {
    sqs_acks_only_after_the_cursor_is_saved: acks_only_after_the_cursor_is_saved(Kind::Sqs);
    azure_acks_only_after_the_cursor_is_saved: acks_only_after_the_cursor_is_saved(Kind::Azure);
    r2_acks_only_after_the_cursor_is_saved: acks_only_after_the_cursor_is_saved(Kind::R2);
    sqs_unparseable_message_still_scans: an_unparseable_message_still_scans(Kind::Sqs);
    azure_unparseable_message_still_scans: an_unparseable_message_still_scans(Kind::Azure);
    r2_unparseable_message_still_scans: an_unparseable_message_still_scans(Kind::R2);
    sqs_messages_for_elsewhere_are_ignored: messages_for_elsewhere_are_deleted_and_ignored(Kind::Sqs);
    azure_messages_for_elsewhere_are_ignored: messages_for_elsewhere_are_deleted_and_ignored(Kind::Azure);
    r2_messages_for_elsewhere_are_ignored: messages_for_elsewhere_are_deleted_and_ignored(Kind::R2);
}

#[tokio::test]
async fn an_r2_queue_given_by_name_is_looked_up() {
    // covered by r2_fake: the first request is the listing, then the id is used (the fake
    // asserts both). Here: a queue given as an id skips the listing.
    let sh = Arc::new(Shared::default());
    let calls = Arc::new(Mutex::new(vec![]));
    let c = calls.clone();
    let fake = Fake::start(move |r| {
        c.lock().unwrap().push(format!("{} {}", r.method, r.url));
        (
            200,
            r#"{"success":true,"result":{"messages":[{"lease_id":"l1","body":"{}"}]}}"#.into(),
        )
    });
    let id = "ab".repeat(16);
    let mut q = R2Queue::new(
        &fake.base,
        "acct",
        &id,
        Bearer::Fixed("t".into()),
        Duration::ZERO,
    );
    use pail::event_sources::Queue;
    let got = q.receive().await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(
        *calls.lock().unwrap(),
        [format!("POST /accounts/acct/queues/{id}/messages/pull")]
    );
    drop(sh);
}

#[tokio::test]
async fn a_missing_credential_is_an_error_not_a_silent_nothing() {
    let fake = Fake::start(|_| (200, "{}".into()));
    let mut q = R2Queue::new(&fake.base, "a", "q", Bearer::None, Duration::ZERO);
    use pail::event_sources::Queue;
    let err = q.receive().await.unwrap_err().to_string();
    assert!(err.contains("CLOUDFLARE_API_TOKEN"), "{err}");
    let mut q = AzureQueue::new(&format!("{}/q", fake.base), Bearer::None, Duration::ZERO).unwrap();
    let err = q.receive().await.unwrap_err().to_string();
    assert!(err.contains("SAS"), "{err}");
}

// ---------- the command ----------

fn pail() -> std::process::Command {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_pail"));
    c.env("PAIL_CONFIG_GLOBAL", "/dev/null")
        .env_remove("PAIL_CLONE")
        .env_remove("PAIL_STORE")
        .env_remove("PAIL_DIR")
        .env_remove("AWS_ENDPOINT")
        .env_remove("AWS_ENDPOINT_URL");
    c
}

/// `setup` prints by default. (`--apply` is only tested against a fake runner, in
/// `event_setup`'s unit tests: it creates billable resources.)
#[test]
fn setup_prints_the_commands_and_runs_nothing() {
    let out = pail()
        .args([
            "--store",
            "gs://acme/app",
            "events",
            "setup",
            "--consumer",
            "ci",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("gcloud pubsub topics create pail-ci"),
        "{text}"
    );
    assert!(text.contains("--object-prefix=app/refs/"), "{text}");
    assert!(text.contains("--object-prefix=app.actions/"), "{text}");
    assert!(text.contains("pail events --gcs-subscription"), "{text}");

    let out = pail()
        .args([
            "--store",
            "s3://acme/app",
            "events",
            "setup",
            "--consumer",
            "ci",
            "--yes",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "--yes alone is not --apply");
}

#[test]
fn one_source_flag_at_a_time_and_a_bad_environment_source_is_named() {
    let out = pail()
        .args(["events", "--sqs-queue", "x", "--r2-queue", "a/b"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let dir = tempfile::tempdir().unwrap();
    let store = format!("file://{}", dir.path().join("repo").display());
    let out = pail()
        .env("PAIL_EVENTS_SOURCE", "kafka:x")
        .args(["--store", &store, "events"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("PAIL_EVENTS_SOURCE") && err.contains("kafka"),
        "{err}"
    );
}
