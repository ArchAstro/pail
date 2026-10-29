//! `Repo::commit`, `Repo::rewrite_ref` and `gc` checked against the Quint model of pail
//! (`verify/pail.qnt`).
//!
//! A model action is one request an actor (a writer, or the collector) makes to the bucket.
//! Each actor is a real `Repo` over a shared in-memory bucket, behind a gate that holds the
//! requests the model counts as steps until the scenario releases them, so the actors
//! interleave exactly as the scenario says. A swap can be released with its reply lost
//! (applied, or never received). After every action the bucket, each writer's progress and
//! the collector's, as read off the requests they made, must equal the model's state.
//! `verify/README.md` has the mapping.

#[path = "refinement/generated.rs"]
mod generated;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as StoreResult,
};
use pail::clock::Clock;
use pail::enc::Reader;
use pail::hash::Hash;
use pail::manifest::{Kind, Loc, Page, decode_page};
use pail::objects::{Lineage, RefValue, Rewrite};
use pail::pack::decode_blob;
use pail::repo::{
    CommitFields, CommitOptions, CommitRequest, ConflictError, Delta, FileInput, KnownHead,
    MovedError, Repo, RewriteRequest, Upload,
};
use pail::store::Store;
use quint_refinements::{NormalizedRuntimeEvidence, RuntimeValue};
use tokio::sync::oneshot;

const BRANCH: &str = "main";
const DATE: &str = "1700000000 +0000";
/// The collector's actor name: `GC` in `world.qnt`.
const GC: &str = "gc";
/// gc's grace period. `graceElapses` ages every object by more than this.
const GRACE: i64 = 1000;
/// How long a released actor may take to reach its next request.
const SETTLE: Duration = Duration::from_secs(20);
/// A tick of the model's clock (`bucket.qnt`): a sixth of `LEASE_TTL`.
const TICK_SECS: i64 = 100;
/// The store's clock at tick 0, in unix seconds.
const EPOCH: i64 = 1_700_000_000;
/// `SKEW` in `bucket.qnt`: how far ahead of the store's clock the lister's runs, in ticks.
const SKEW: i64 = 1;

/// The clock every actor's store judges leases by: it stands still until the scenario
/// `tick`s, and wakes the refreshers whose sleeps are over.
#[derive(Default)]
struct TestClock {
    inner: Mutex<ClockState>,
}

#[derive(Default)]
struct ClockState {
    /// Seconds since tick 0.
    secs: i64,
    sleepers: Vec<(i64, oneshot::Sender<()>)>,
}

impl TestClock {
    fn ticks(&self) -> i64 {
        self.inner.lock().unwrap().secs / TICK_SECS
    }

    /// One tick on; how many sleeps ended.
    fn advance(&self) -> usize {
        let mut c = self.inner.lock().unwrap();
        c.secs += TICK_SECS;
        let now = c.secs;
        let (due, later): (Vec<_>, Vec<_>) = c.sleepers.drain(..).partition(|(at, _)| *at <= now);
        c.sleepers = later;
        due.into_iter()
            .filter(|(_, tx)| !tx.is_closed())
            .map(|(_, tx)| tx.send(()).is_ok())
            .filter(|woke| *woke)
            .count()
    }

    /// Sleeps that are waiting (not abandoned by an aborted refresher).
    fn sleeping(&self) -> usize {
        let c = self.inner.lock().unwrap();
        c.sleepers.iter().filter(|(_, tx)| !tx.is_closed()).count()
    }
}

impl Clock for TestClock {
    fn instant(&self) -> Duration {
        Duration::from_secs(self.inner.lock().unwrap().secs as u64)
    }

    fn unix_secs(&self) -> i64 {
        EPOCH + self.inner.lock().unwrap().secs + SKEW * TICK_SECS
    }

    fn sleep(
        &self,
        d: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let (tx, rx) = oneshot::channel();
        let mut c = self.inner.lock().unwrap();
        let at = c.secs + d.as_secs() as i64;
        c.sleepers.push((at, tx));
        Box::pin(async move {
            let _ = rx.await;
        })
    }
}

#[test]
fn pail_refines_the_model() {
    let results = generated::refine_all::<Harness>().unwrap_or_else(|e| panic!("{e}"));
    for r in &results {
        eprintln!(
            "{} refined {} obligations",
            r.scenario, r.evaluated_obligations
        );
    }
    assert_eq!(
        results.len(),
        65,
        "every scenario in verify/scenarios.qnt ran"
    );
}

// ---------- the gate ----------

/// A request the model sees as one step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    PutLease,
    ListLeases,
    ReadRef,
    PutCommit,
    Swap,
    /// The first write of the combining path: the writer gave up swapping directly.
    PutIntent,
    /// A rewrite's record.
    PutRewrite,
    ListIntents,
    GetIntent,
    DeleteIntent,
    ListCommits,
    DeleteCommit,
    ListPages,
    DeletePage,
    ListPacks,
    DeletePack,
    /// gc releasing its lease: its last request.
    DeleteLease,
    /// A holder's refresher rewriting its lease, which the store applies on release...
    RefreshLease,
    /// ... and whose reply then comes back (`refreshAck`).
    AckLease,
}

/// How the gate lets a held request through: `Delivery` in `store.qnt`.
#[derive(Clone, Copy, Debug)]
enum Delivery {
    Answered,
    /// Performed, but the reply never arrives.
    Lost,
    /// Never reaches the store.
    Dropped,
}

impl Delivery {
    fn of(v: &RuntimeValue) -> Result<Delivery, String> {
        let tag = match v {
            RuntimeValue::Text(t) => t.as_str(),
            RuntimeValue::Variant { tag, .. } => tag.as_str(),
            other => return Err(format!("expected a Delivery, got {other:?}")),
        };
        match tag {
            "Answered" => Ok(Delivery::Answered),
            "Lost" => Ok(Delivery::Lost),
            "Dropped" => Ok(Delivery::Dropped),
            other => Err(format!("unknown Delivery {other}")),
        }
    }
}

enum Event {
    Held {
        actor: String,
        request: Request,
        key: String,
        go: oneshot::Sender<Delivery>,
    },
    Answered {
        actor: String,
        request: Request,
        key: String,
        etag: Option<String>,
        /// A listing's keys.
        listed: Vec<String>,
    },
    Finished {
        actor: String,
        outcome: &'static str,
    },
    /// The head a writer's publish left it, sent before its `Finished`: kept, as
    /// `Worktree::commit` keeps it, only when its commit is the ref's tip.
    Known {
        actor: String,
        known: Option<KnownHead>,
    },
    /// gc took its exclusive lease.
    GcHeld { probe: pail::lease::Probe },
    /// What a rebase built, before it leases.
    Planned {
        actor: String,
        tip: Hash,
        map: BTreeMap<Hash, Option<Hash>>,
        commits: Vec<(Hash, Bytes)>,
    },
}

/// What every gate records about writes, for the refinement map: each object's version
/// (one per create, overwrite or delete), the grace epoch it was last written in, and
/// which actor wrote each lease.
#[derive(Default)]
struct Book {
    epoch: i64,
    versions: HashMap<String, i64>,
    written: HashMap<String, i64>,
    holders: HashMap<String, String>,
    /// The tick each lease was last written (its LastModified).
    leased_at: HashMap<String, i64>,
    clock: Arc<TestClock>,
}

/// A writer's page and pack puts wait for its commit uploads to be released: the model
/// uploads a commit, its page and its pack in one step. Pages stay open until the writer's next other request
/// (a combining round stores several commits in one wave).
#[derive(Default)]
struct Uploads {
    open: bool,
    parked: Vec<oneshot::Sender<()>>,
}

/// One actor's view of the bucket: every request passes through, and the ones the model
/// counts as steps wait for the scenario to release them.
#[derive(Clone)]
struct Gate {
    inner: Arc<InMemory>,
    actor: String,
    events: mpsc::Sender<Event>,
    book: Arc<Mutex<Book>>,
    uploads: Arc<Mutex<Uploads>>,
    /// Holds nothing: a step the model takes whole (making a stack).
    free: bool,
}

impl fmt::Debug for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Gate({})", self.actor)
    }
}

impl fmt::Display for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gate for {}", self.actor)
    }
}

/// What a rewrite returned, as the model's `Outcome`.
fn rewrite_outcome(result: anyhow::Result<()>) -> &'static str {
    match result {
        Ok(()) => "Rewritten",
        Err(e) if e.downcast_ref::<MovedError>().is_some() => "Moved",
        Err(e) if e.downcast_ref::<ConflictError>().is_some() => "Conflicted",
        Err(_) => "Unknown",
    }
}

fn lost_reply() -> object_store::Error {
    object_store::Error::Generic {
        store: "gate",
        source: "request timed out".into(),
    }
}

/// A failure `Store::put` returns at once instead of retrying.
fn unretried(key: &str) -> object_store::Error {
    object_store::Error::PermissionDenied {
        path: key.to_owned(),
        source: "request timed out".into(),
    }
}

impl Gate {
    fn write_request(key: &str) -> Option<Request> {
        if key == format!("refs/{BRANCH}") {
            Some(Request::Swap)
        } else if key.starts_with("commits/") {
            Some(Request::PutCommit)
        } else if key.starts_with("intents/") {
            Some(Request::PutIntent)
        } else if key.starts_with("reflog/") {
            Some(Request::PutRewrite)
        } else if key.starts_with("locks/") {
            Some(Request::PutLease)
        } else {
            None
        }
    }

    fn list_request(prefix: &str) -> Option<Request> {
        if prefix == format!("intents/{BRANCH}") {
            return Some(Request::ListIntents);
        }
        match prefix {
            "locks" => Some(Request::ListLeases),
            "commits" => Some(Request::ListCommits),
            "pages" => Some(Request::ListPages),
            "packs" => Some(Request::ListPacks),
            _ => None,
        }
    }

    fn delete_request(&self, key: &str) -> Option<Request> {
        if key.starts_with("commits/") {
            Some(Request::DeleteCommit)
        } else if key.starts_with("pages/") {
            Some(Request::DeletePage)
        } else if key.starts_with("packs/") {
            Some(Request::DeletePack)
        } else if key.starts_with("locks/") && self.actor == GC {
            Some(Request::DeleteLease)
        } else if key.starts_with("intents/") {
            Some(Request::DeleteIntent)
        } else {
            None
        }
    }

    async fn hold(&self, request: Request, key: &str) -> Delivery {
        if self.free {
            return Delivery::Answered;
        }
        if !matches!(
            request,
            Request::PutCommit | Request::RefreshLease | Request::AckLease
        ) {
            self.uploads.lock().unwrap().open = false;
        }
        let (go, released) = oneshot::channel();
        let _ = self.events.send(Event::Held {
            actor: self.actor.clone(),
            request,
            key: key.to_owned(),
            go,
        });
        // A harness that went away fails the request rather than performing it.
        let how = released.await.unwrap_or(Delivery::Dropped);
        if request == Request::PutCommit {
            let mut u = self.uploads.lock().unwrap();
            u.open = true;
            for p in u.parked.drain(..) {
                let _ = p.send(());
            }
        }
        how
    }

    fn answered(&self, request: Request, key: &str, etag: Option<String>) {
        self.answered_listing(request, key, etag, vec![]);
    }

    fn answered_listing(
        &self,
        request: Request,
        key: &str,
        etag: Option<String>,
        listed: Vec<String>,
    ) {
        if self.free {
            return;
        }
        let _ = self.events.send(Event::Answered {
            actor: self.actor.clone(),
            request,
            key: key.to_owned(),
            etag,
            listed,
        });
    }

    fn wrote(&self, key: &str) {
        let mut b = self.book.lock().unwrap();
        *b.versions.entry(key.to_owned()).or_default() += 1;
        let epoch = b.epoch;
        b.written.insert(key.to_owned(), epoch);
        if key.starts_with("locks/") {
            b.holders.insert(key.to_owned(), self.actor.clone());
            let now = b.clock.ticks();
            b.leased_at.insert(key.to_owned(), now);
        }
    }

    fn removed(&self, key: &str) {
        *self
            .book
            .lock()
            .unwrap()
            .versions
            .entry(key.to_owned())
            .or_default() += 1;
    }

    /// Objects as old as the grace epochs since they were written make them: each epoch
    /// ages an object by more than gc's grace period.
    fn aged(&self, mut meta: ObjectMeta) -> ObjectMeta {
        let b = self.book.lock().unwrap();
        let written = b.written.get(meta.location.as_ref()).copied().unwrap_or(0);
        let age = (b.epoch - written) * (GRACE + 1);
        meta.last_modified -= chrono::Duration::seconds(age);
        meta
    }

    /// A lease as the store stamped it: written at the tick it landed. The lister reads
    /// its clock when it sends the request and the model lists at the tick the scenario
    /// releases it, so the stamp moves back by the ticks the request waited: the lease is
    /// then as old to the lister as the model's listing finds it. (Reading the clock early
    /// only makes a lease look younger, so the real lister is the more cautious.)
    fn stamped(&self, mut meta: ObjectMeta, waited: i64) -> ObjectMeta {
        let b = self.book.lock().unwrap();
        let at = b
            .leased_at
            .get(meta.location.as_ref())
            .copied()
            .unwrap_or(0);
        meta.last_modified = chrono::DateTime::from_timestamp(EPOCH + (at - waited) * TICK_SECS, 0)
            .expect("a timestamp");
        meta
    }

    async fn put(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> StoreResult<PutResult> {
        let r = self.inner.put_opts(location, payload, opts).await;
        if r.is_ok() {
            self.wrote(location.as_ref());
        }
        r
    }

    /// A refresher's put: held until released (`refreshLand`); if the store applies it and
    /// answers, the reply is held again until `refreshAck`.
    async fn refresh(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> StoreResult<PutResult> {
        let key = location.as_ref();
        let result = match self.hold(Request::RefreshLease, key).await {
            Delivery::Answered => {
                let r = self.put(location, payload, opts).await;
                self.answered(Request::RefreshLease, key, None);
                self.hold(Request::AckLease, key).await;
                self.answered(Request::AckLease, key, None);
                return r;
            }
            // An error `Store::put` does not retry: a retry would be another request.
            Delivery::Lost => {
                let _ = self.put(location, payload, opts).await;
                Err(unretried(key))
            }
            Delivery::Dropped => Err(unretried(key)),
        };
        self.answered(Request::RefreshLease, key, None);
        result
    }
}

#[async_trait]
impl ObjectStore for Gate {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> StoreResult<PutResult> {
        let key = location.as_ref();
        if (key.starts_with("pages/") || key.starts_with("packs/")) && !self.free {
            let wait = {
                let mut u = self.uploads.lock().unwrap();
                if u.open {
                    None
                } else {
                    let (go, wait) = oneshot::channel();
                    u.parked.push(go);
                    Some(wait)
                }
            };
            if let Some(wait) = wait {
                let _ = wait.await;
            }
        }
        let Some(request) = Gate::write_request(key) else {
            return self.put(location, payload, opts).await;
        };
        if request == Request::PutLease && self.book.lock().unwrap().versions.contains_key(key) {
            // A lease written already: its refresher rewriting it.
            return self.refresh(location, payload, opts).await;
        }
        let result = match self.hold(request, key).await {
            Delivery::Answered => self.put(location, payload, opts).await,
            Delivery::Lost => {
                let _ = self.put(location, payload, opts).await;
                Err(lost_reply())
            }
            Delivery::Dropped => Err(lost_reply()),
        };
        self.answered(request, key, None);
        result
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> StoreResult<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> StoreResult<GetResult> {
        if location.as_ref().starts_with("intents/") {
            let release = self.hold(Request::GetIntent, location.as_ref()).await;
            let result = match release {
                Delivery::Answered => self.inner.get_opts(location, options).await,
                Delivery::Lost | Delivery::Dropped => Err(lost_reply()),
            };
            self.answered(Request::GetIntent, location.as_ref(), None);
            return result;
        }
        if location.as_ref() != format!("refs/{BRANCH}") {
            return self.inner.get_opts(location, options).await;
        }
        let release = self.hold(Request::ReadRef, location.as_ref()).await;
        let result = match release {
            Delivery::Answered => self.inner.get_opts(location, options).await,
            Delivery::Lost | Delivery::Dropped => Err(lost_reply()),
        };
        let etag = result.as_ref().ok().and_then(|r| r.meta.e_tag.clone());
        self.answered(Request::ReadRef, location.as_ref(), etag);
        result
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, StoreResult<Path>>,
    ) -> BoxStream<'static, StoreResult<Path>> {
        let gate = self.clone();
        locations
            .then(move |location| {
                let gate = gate.clone();
                async move {
                    let location = location?;
                    let key = location.to_string();
                    let request = gate.delete_request(&key);
                    if let Some(request) = request {
                        match gate.hold(request, &key).await {
                            Delivery::Answered => {}
                            _ => return Err(lost_reply()),
                        }
                    }
                    // Deleting what is gone changes nothing (`store.qnt`'s `deleted`).
                    let existed =
                        object_store::ObjectStoreExt::head(gate.inner.as_ref(), &location)
                            .await
                            .is_ok();
                    let one = futures::stream::once(async { Ok(location) }).boxed();
                    let out = gate.inner.delete_stream(one).next().await.unwrap();
                    if out.is_ok() && existed {
                        gate.removed(&key);
                    }
                    if let Some(request) = request {
                        gate.answered(request, &key, None);
                    }
                    out
                }
            })
            .boxed()
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, StoreResult<ObjectMeta>> {
        let request = prefix.and_then(|p| Gate::list_request(p.as_ref()));
        let ages = matches!(
            request,
            Some(Request::ListCommits) | Some(Request::ListPages) | Some(Request::ListPacks)
        );
        let stamps = request == Some(Request::ListLeases);
        let gate = self.clone();
        let prefix = prefix.cloned();
        futures::stream::once(async move {
            let key = prefix.as_ref().map(|p| p.to_string()).unwrap_or_default();
            let sent = gate.book.lock().unwrap().clock.ticks();
            if let Some(request) = request
                && !matches!(gate.hold(request, &key).await, Delivery::Answered)
            {
                return futures::stream::iter(vec![Err(lost_reply())]);
            }
            let items: Vec<_> = gate.inner.list(prefix.as_ref()).collect().await;
            if let Some(request) = request {
                let keys = items
                    .iter()
                    .filter_map(|m| m.as_ref().ok().map(|m| m.location.to_string()))
                    .collect();
                gate.answered_listing(request, &key, None, keys);
            }
            let waited = gate.book.lock().unwrap().clock.ticks() - sent;
            let items = items
                .into_iter()
                .map(|m| {
                    m.map(|m| {
                        if ages {
                            gate.aged(m)
                        } else if stamps {
                            gate.stamped(m, waited)
                        } else {
                            m
                        }
                    })
                })
                .collect::<Vec<_>>();
            futures::stream::iter(items)
        })
        .flatten()
        .boxed()
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, StoreResult<ObjectMeta>> {
        self.inner.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> StoreResult<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> StoreResult<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

// ---------- model values ----------

/// A commit as the model names it: the writers of every commit from the root to it.
type Id = Vec<String>;
/// A tree: path to content.
type Tree = BTreeMap<String, String>;

/// A page as `bucket.qnt` keys it: the tree, and the pack each big file is in.
type PageKey = (Tree, BTreeMap<String, Id>);

/// The model's clients, whose own commits use the pack the client prepared (`packOf`).
const CLIENTS: [&str; 4] = ["alice", "bob", "carol", "dave"];

/// The path of the decoy file each pack holds (see `Harness::files`), left out of every
/// delta.
const DECOY: &str = "\u{1}decoy";

/// Contents the model stores in packs (`BIG`): bigger than a manifest entry holds.
fn is_big(content: &str) -> bool {
    matches!(content, "p1" | "p2" | "p3")
}

/// A model content as the file's bytes: a big one is padded past `INLINE_MAX`.
fn real(content: &str) -> Vec<u8> {
    let mut bytes = content.as_bytes().to_vec();
    if is_big(content) {
        bytes.push(b'\n');
        bytes.extend(std::iter::repeat_n(b'.', 80));
    }
    bytes
}

/// A file's bytes as the model's content: `real` undone.
fn unpad(data: &[u8]) -> String {
    let text = String::from_utf8_lossy(data);
    text.split('\n').next().unwrap_or_default().to_owned()
}

/// The pack a commit named `id` stores (`packOf`).
fn pack_of(id: &Id) -> Id {
    match id.last() {
        Some(last) if CLIENTS.contains(&last.as_str()) => vec![last.clone()],
        _ => id.clone(),
    }
}

/// The contents a pack holds (its footer indexes the blobs; the decoy is no content).
fn pack_contents(pack: &[u8]) -> Result<BTreeSet<String>, String> {
    let bad = |e: anyhow::Error| e.to_string();
    let end = pack
        .len()
        .checked_sub(12)
        .ok_or("a pack shorter than its trailer")?;
    let footer = u32::from_le_bytes(pack[end..end + 4].try_into().unwrap()) as usize;
    let mut r = Reader::new(&pack[end - footer..end]);
    let mut out = BTreeSet::new();
    for _ in 0..r.varint().map_err(bad)? {
        r.hash().map_err(bad)?;
        let (off, len, raw) = (
            r.varint().map_err(bad)? as usize,
            r.varint().map_err(bad)? as usize,
            r.varint().map_err(bad)?,
        );
        let blob = decode_blob(&pack[off..off + len], raw).map_err(bad)?;
        let content = unpad(&blob);
        if !content.starts_with("decoy:") {
            out.insert(content);
        }
    }
    Ok(out)
}

fn id_value(id: &Id) -> RuntimeValue {
    RuntimeValue::List(id.iter().map(|a| RuntimeValue::Text(a.clone())).collect())
}

/// The writer or intent whose change a commit makes: its name's last part, before any
/// rewriter's mark (`tagOf`).
/// The tags of the commits melded into `id`, oldest first (`world.qnt` `meldedTags`).
fn melded(id: &Id) -> Vec<String> {
    let own = match id.as_slice() {
        [.., mark] if mark.starts_with('~') => id.len() - 2,
        _ => id.len().saturating_sub(1),
    };
    let mut out = vec![];
    let mut k = own;
    while k >= 2 && id[k - 1] == "&" {
        out.push(id[k - 2].clone());
        k -= 2;
    }
    out.reverse();
    out
}

fn tag(id: &Id) -> String {
    match id.as_slice() {
        [.., t, mark] if mark.starts_with('~') => t.clone(),
        [.., t] => t.clone(),
        [] => String::new(),
    }
}

fn text(v: &RuntimeValue) -> Result<&str, String> {
    match v {
        RuntimeValue::Text(t) => Ok(t),
        other => Err(format!("expected text, got {other:?}")),
    }
}

fn field<'a>(v: &'a RuntimeValue, name: &str) -> Result<&'a RuntimeValue, String> {
    match v {
        RuntimeValue::Record(fields) => fields
            .get(name)
            .ok_or_else(|| format!("record has no field {name}: {v:?}")),
        other => Err(format!("expected a record, got {other:?}")),
    }
}

fn id_of(v: &RuntimeValue) -> Result<Id, String> {
    let RuntimeValue::List(items) = v else {
        return Err(format!("expected a commit id (a list of writers): {v:?}"));
    };
    items.iter().map(|a| Ok(text(a)?.to_owned())).collect()
}

fn tree_of(v: &RuntimeValue) -> Result<Tree, String> {
    let RuntimeValue::Map(m) = v else {
        return Err(format!("expected a map of paths: {v:?}"));
    };
    m.iter()
        .map(|(k, v)| Ok((text(k)?.to_owned(), text(v)?.to_owned())))
        .collect()
}

fn tree_value(t: &Tree) -> RuntimeValue {
    RuntimeValue::Map(
        t.iter()
            .map(|(k, v)| (RuntimeValue::Text(k.clone()), RuntimeValue::Text(v.clone())))
            .collect(),
    )
}

fn record<const N: usize>(fields: [(&str, RuntimeValue); N]) -> RuntimeValue {
    RuntimeValue::Record(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn set<I: IntoIterator<Item = RuntimeValue>>(items: I) -> RuntimeValue {
    RuntimeValue::Set(items.into_iter().collect())
}

/// A `Versioned` object of `store.qnt`.
fn versioned(body: RuntimeValue, version: i64, live: bool) -> RuntimeValue {
    record([
        ("body", body),
        ("version", RuntimeValue::Int(version)),
        ("live", RuntimeValue::Bool(live)),
    ])
}

// ---------- the actors ----------

/// A request an actor is waiting at.
type Held = (Request, String, oneshot::Sender<Delivery>);

/// What an actor is doing, as read off its requests.
#[derive(Default)]
struct Progress {
    /// The requests it is waiting at (a writer makes some together: an upload and a
    /// listing, a ref read and a listing, a wave of commits or deletes).
    held: Vec<Held>,
    /// The refresher's requests, which it makes beside the actor's own (`RefreshLease`,
    /// then `AckLease`).
    refresh: Vec<Held>,
    /// Requests released whose answers the harness has not yet seen.
    awaiting: usize,
    outcome: Option<&'static str>,
}

impl Progress {
    fn holds(&self, request: Request) -> bool {
        self.held.iter().any(|(r, _, _)| *r == request)
    }

    fn refreshing(&self, request: Request) -> bool {
        self.refresh.iter().any(|(r, _, _)| *r == request)
    }
}

/// What the harness knows of a lease holder's refresher: `Timing` in `world.qnt`, less what
/// the lease itself tells (`running`, `refreshed`).
#[derive(Clone, Default)]
struct Refresher {
    done: i64,
    inflight: bool,
    sent: i64,
    landed: bool,
    checked: i64,
}

/// Landed intents by writer: `Recent` in `bucket.qnt`.
type Recent = BTreeMap<String, Id>;

/// One poll-and-combine round of a combining writer: `Comb` in `world.qnt`.
#[derive(Clone, Default)]
struct Round {
    /// The ref as the poll read it: commit, version, landed intents.
    head: Option<(Id, i64, Recent)>,
    /// The lineage in that read.
    lineage: Lineage,
    /// The writers whose intents the poll listed.
    listed: Option<BTreeSet<String>>,
    /// The commits the round stored, in the order stored.
    batch: Vec<Hash>,
}

/// A client rewriting the branch: `Rw` in `world.qnt`.
#[derive(Clone, Default)]
struct Rewriter {
    /// Its record's `op`: `rebase`, `undo` or `reset`; a rebase's plan.
    op: &'static str,
    plan: &'static str,
    /// The tip it was made from, the commit replacing it, and the successors it starts
    /// from (empty until its first read).
    expected: Id,
    new: Id,
    successors: BTreeMap<Id, Id>,
    /// The commits it drops, and the ones it makes.
    dropped: BTreeSet<Id>,
    made: BTreeSet<Id>,
    /// For an undo, the lineage that named its record.
    since: Lineage,
    /// The version its last attempt read; `version` once that attempt wrote its record.
    read: i64,
    version: i64,
    /// What that attempt's swap moves the ref to, and its record.
    tip: Option<Id>,
    id: i64,
}

/// A client landing its stack: `Ld` in `world.qnt`.
#[derive(Clone, Default)]
struct Lander {
    /// Land calls begun.
    calls: i64,
    /// Its stack's commit.
    stack: Id,
    /// What its last attempt read of the branch: version and lineage.
    version: i64,
    lineage: Lineage,
    /// The commit that attempt copied onto the branch, if any, and the tip its swap moves
    /// the branch to.
    stored: Option<Id>,
    tip: Option<Id>,
}

#[derive(Default)]
struct Writer {
    base: Option<Id>,
    delta: Tree,
    progress: Progress,
    /// Its last swap went unanswered, so its next ref read is a recheck.
    rechecking: bool,
    /// Ref reads that began an attempt.
    attempts: i64,
    /// The ref as its last attempt read it: commit, version, landed intents.
    seen: Option<(Id, i64, Recent)>,
    /// The lineage in that read.
    seen_lineage: Lineage,
    /// Set once it begins a rewrite instead of a commit.
    rw: Option<Rewriter>,
    /// Set once it has made a stack; `ld` once it lands it.
    stacked: bool,
    ld: Option<Lander>,
    /// Where it found the commit it read (`Repo::successor`), and the lineage it looked at.
    found: Option<(Id, Lineage)>,
    /// The head its last publish left it (`Event::Known`).
    known: Option<KnownHead>,
    /// Where it is on the combining path (`Intending` ..), once it has written its intent.
    combining: Option<&'static str>,
    /// Its intent's id and the time the scenario gave it.
    intent: Option<(String, i64)>,
    round: Round,
    /// An attempt's ref read was let through and its swap not yet: a ref read next means
    /// the swap never left (the lease check refused it), so the read is a recheck.
    reading: bool,
}

/// What was written since the grace period last elapsed: `Young` in `bucket.qnt`.
#[derive(Clone, Default)]
struct Young {
    pages: BTreeSet<PageKey>,
    commits: BTreeSet<Id>,
    packs: BTreeSet<Id>,
}

/// The collector's run, as read off its requests.
#[derive(Default)]
struct Collector {
    running: bool,
    /// Its lease, once it holds it.
    probe: Option<pail::lease::Probe>,
    progress: Progress,
    marked: Option<(BTreeSet<Id>, BTreeSet<PageKey>)>,
    marked_packs: BTreeSet<Id>,
    doomed_commits: BTreeSet<String>,
    doomed_pages: BTreeSet<String>,
    doomed_packs: BTreeSet<String>,
    spared: Option<Young>,
}

/// Real actors over one bucket, and what the scenario has seen of them.
struct Harness {
    rt: Option<tokio::runtime::Runtime>,
    bucket: Arc<InMemory>,
    book: Arc<Mutex<Book>>,
    /// Reads the bucket directly, past every gate.
    observer: Repo,
    events: mpsc::Receiver<Event>,
    sender: mpsc::Sender<Event>,
    writers: BTreeMap<String, Writer>,
    collector: Collector,
    names: HashMap<Hash, Id>,
    hashes: HashMap<Id, Hash>,
    /// Intent ids by writer, and writers by intent id.
    intent_writers: HashMap<String, String>,
    /// Rewrite record ids, numbered in the order they were made (`nextRecord`).
    record_ids: HashMap<String, i64>,
    /// Each object's body as last seen, so a deleted one keeps it (`store.qnt` tombstones).
    bodies: Mutex<HashMap<String, RuntimeValue>>,
    /// Pages by key: the tree each holds.
    trees: Mutex<HashMap<String, Tree>>,
    /// Each page's big files: the pack they are in.
    page_locs: Mutex<HashMap<Hash, BTreeMap<String, Hash>>>,
    /// Packs by hash, named by the commit that first names them (`packOf`).
    pack_names: Mutex<HashMap<Hash, Id>>,
    /// The ref's ETags in order: version n is the n-th.
    ref_etags: Mutex<Vec<String>>,
    /// The commit each version of the ref named, oldest first.
    ref_commits: Mutex<Vec<Hash>>,
    /// Each version's body.
    ref_values: Mutex<Vec<RefValue>>,
    /// The clock every actor's store judges leases by.
    clock: Arc<TestClock>,
    /// The latest client each actor runs, to read its lease from.
    repos: Mutex<HashMap<String, Arc<Repo>>>,
    /// Each running lease holder's refresher.
    refreshers: Mutex<HashMap<String, Refresher>>,
    /// Refresh requests refreshers have made (and the harness has seen).
    refresh_requests: usize,
    /// The tick each writer's lease put arrived at the gate (`Timing.sent` until it holds
    /// the lease): `Lease::write` takes its instant before the put.
    lease_sent: Mutex<HashMap<String, i64>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Actors still held (a scenario that ends mid-call) are abandoned, not released.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

impl Harness {
    fn rt(&self) -> &tokio::runtime::Runtime {
        self.rt.as_ref().expect("runtime")
    }

    fn gate(&self, actor: &str) -> Store {
        self.gate_with(actor, false)
    }

    fn gate_with(&self, actor: &str, free: bool) -> Store {
        let gate = Gate {
            inner: self.bucket.clone(),
            actor: actor.to_owned(),
            events: self.sender.clone(),
            book: self.book.clone(),
            uploads: Arc::default(),
            free,
        };
        Store::from_object_store("memory://bucket", Arc::new(gate)).with_clock(self.clock.clone())
    }

    /// `actor`'s lease, if it holds one.
    fn probe(&self, actor: &str) -> Option<pail::lease::Probe> {
        if actor == GC {
            self.collector.probe.clone()
        } else {
            self.repos
                .lock()
                .unwrap()
                .get(actor)
                .and_then(|r| r.lease_probe())
        }
    }

    /// Start (or stop) tracking `actor`'s refresher as its lease comes (or goes): it sleeps
    /// from the moment the lease is held.
    fn note_running(&self, actor: &str) {
        let running = self.probe(actor).is_some();
        let mut refreshers = self.refreshers.lock().unwrap();
        if running {
            refreshers
                .entry(actor.to_owned())
                .or_insert_with(|| Refresher {
                    done: self.clock.ticks(),
                    ..Refresher::default()
                });
        } else {
            refreshers.remove(actor);
        }
    }

    /// A client for `actor` behind its gate, remembered for its lease.
    fn client(&self, actor: &str) -> Arc<Repo> {
        let repo = Arc::new(Repo::with_store(self.gate(actor), None));
        self.repos
            .lock()
            .unwrap()
            .insert(actor.to_owned(), repo.clone());
        repo
    }

    /// `tree`'s files as a delta, and the pack to upload with it (none if all are small
    /// enough to inline). Writing a big file the tree already has is a repack: the delta
    /// names the file in a pack of the writer's own. A pack holds a decoy blob of the
    /// writer's too, so two writers' packs of one content are not one object.
    fn files(&self, tree: &Tree, actor: &str) -> Result<(Delta, Vec<Upload>), String> {
        let mut inputs: Vec<FileInput> = tree
            .iter()
            .map(|(path, data)| FileInput {
                path: path.clone(),
                kind: Kind::File,
                data: Bytes::from(real(data)),
            })
            .collect();
        if tree.values().any(|v| is_big(v)) {
            let mut decoy = format!("decoy:{actor}\n").into_bytes();
            decoy.extend(std::iter::repeat_n(b'.', 80));
            inputs.push(FileInput {
                path: DECOY.to_owned(),
                kind: Kind::File,
                data: Bytes::from(decoy),
            });
        }
        let (entries, uploads) = self.observer.prepare_files(&inputs, &HashMap::new());
        let delta = entries
            .into_iter()
            .filter(|e| e.path != DECOY)
            .map(|e| (e.path.clone(), Some(e)))
            .collect();
        Ok((delta, uploads))
    }

    fn request(&self, by: &str, base: Option<Hash>, delta: Delta) -> CommitRequest {
        CommitRequest {
            branch: BRANCH.to_owned(),
            base,
            delta,
            author: format!("{by} <{by}@example.com>"),
            author_date: DATE.to_owned(),
            committer: format!("{by} <{by}@example.com>"),
            committer_date: DATE.to_owned(),
            message: format!("{by}'s change"),
            meta: BTreeMap::new(),
        }
    }

    fn writer(&mut self, name: &str) -> Result<&mut Writer, String> {
        self.writers
            .get_mut(name)
            .ok_or_else(|| format!("no writer {name}"))
    }

    fn progress(&mut self, actor: &str) -> Result<&mut Progress, String> {
        if actor == GC {
            Ok(&mut self.collector.progress)
        } else {
            Ok(&mut self.writer(actor)?.progress)
        }
    }

    /// The ref's commit and version, recording a version not seen before.
    fn read_ref(&self) -> Result<(Hash, i64), String> {
        let key = format!("refs/{BRANCH}");
        let (data, etag) = self.object(&key)?.ok_or("the branch ref is gone")?;
        let value: RefValue = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        Ok((value.commit, self.ref_version(&etag, Some(&value))))
    }

    fn ref_version(&self, etag: &str, value: Option<&RefValue>) -> i64 {
        let mut etags = self.ref_etags.lock().unwrap();
        if let Some(i) = etags.iter().position(|e| e == etag) {
            return i as i64 + 1;
        }
        etags.push(etag.to_owned());
        if let Some(v) = value {
            self.ref_commits.lock().unwrap().push(v.commit);
            self.ref_values.lock().unwrap().push(v.clone());
        }
        etags.len() as i64
    }

    fn name(&self, h: &Hash) -> Result<Id, String> {
        self.names
            .get(h)
            .cloned()
            .ok_or_else(|| format!("commit {} has no writer the scenario knows", h.short()))
    }

    /// Every commit the branch reached at `head`, and their pages.
    fn reach(&self, head: Hash) -> Result<(BTreeSet<Id>, BTreeSet<PageKey>), String> {
        let (mut ids, mut trees) = (BTreeSet::new(), BTreeSet::new());
        let mut cur = Some(head);
        while let Some(h) = cur {
            ids.insert(self.name(&h)?);
            let c = self
                .rt()
                .block_on(self.observer.commit_obj(&h))
                .map_err(|e| e.to_string())?;
            trees.insert(self.page(&format!("pages/{}", c.root.hex()))?);
            cur = c.parent;
        }
        Ok((ids, trees))
    }

    /// The model name of the writer whose intent has `key` (`intents/<branch>/<id>.json`).
    fn intent_writer(&self, id: &str) -> Result<String, String> {
        let id = id
            .rsplit('/')
            .next()
            .unwrap_or(id)
            .trim_end_matches(".json");
        self.intent_writers
            .get(id)
            .cloned()
            .ok_or_else(|| format!("intent {id} has no writer the scenario knows"))
    }

    /// A ref's landed intents as the model's `Recent`.
    fn recent(&self, value: &RefValue) -> Result<Recent, String> {
        self.landed(&value.recent)
    }

    fn handle(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::Held {
                actor,
                request,
                key,
                go,
            } => {
                self.note_running(&actor);
                let now = self.clock.ticks();
                if request == Request::PutLease && actor != GC {
                    self.lease_sent.lock().unwrap().insert(actor.clone(), now);
                }
                if matches!(request, Request::RefreshLease | Request::AckLease) {
                    if request == Request::RefreshLease {
                        let mut r = self.refreshers.lock().unwrap();
                        let e = r.entry(actor.clone()).or_default();
                        (e.inflight, e.sent) = (true, now);
                        self.refresh_requests += 1;
                    }
                    self.progress(&actor)?.refresh.push((request, key, go));
                    return Ok(());
                }
                if matches!(
                    request,
                    Request::Swap
                        | Request::DeleteCommit
                        | Request::DeletePage
                        | Request::DeletePack
                ) && let Some(e) = self.refreshers.lock().unwrap().get_mut(&actor)
                {
                    // A request a passed `Lease::check` let go.
                    e.checked = now;
                }
                if actor != GC {
                    let w = self.writer(&actor)?;
                    if request == Request::ReadRef && w.reading && w.combining.is_none() {
                        // The swap never left: the lease check refused it.
                        w.rechecking = true;
                        w.reading = false;
                        if let Some(ld) = w.ld.as_mut() {
                            ld.tip = Some(ld.stored.take().unwrap_or_else(|| ld.stack.clone()));
                        }
                    }
                    if request == Request::ReadRef
                        && !w.rechecking
                        && w.combining.is_none()
                        && w.rw.is_none()
                        && w.ld.is_none()
                    {
                        w.attempts += 1;
                    }
                    if request == Request::Swap
                        && let Some(ld) = w.ld.as_mut()
                    {
                        ld.tip = Some(ld.stored.take().unwrap_or_else(|| ld.stack.clone()));
                    }
                    if request == Request::PutIntent {
                        w.combining = Some("Intending");
                    }
                }
                let p = self.progress(&actor)?;
                if p.holds(request)
                    && !matches!(
                        request,
                        Request::PutCommit | Request::GetIntent | Request::DeleteIntent
                    )
                {
                    return Err(format!("{actor} made a second {request:?} request at once"));
                }
                p.held.push((request, key, go));
            }
            Event::Answered {
                actor,
                request,
                key,
                etag,
                listed,
            } => {
                let p = self.progress(&actor)?;
                p.awaiting = p.awaiting.saturating_sub(1);
                self.answered(&actor, request, &key, etag, listed)?
            }
            Event::Planned {
                actor,
                tip,
                map,
                commits,
            } => self.planned(&actor, tip, map, commits)?,
            Event::Known { actor, known } => self.writer(&actor)?.known = known,
            Event::Finished { actor, outcome } => {
                if actor == GC {
                    self.collector = Collector::default();
                } else {
                    let p = &mut self.writer(&actor)?.progress;
                    p.outcome = Some(outcome);
                    p.refresh.clear();
                    self.lease_sent.lock().unwrap().remove(&actor);
                }
                self.note_running(&actor);
            }
            Event::GcHeld { probe } => {
                self.collector.probe = Some(probe);
                self.note_running(GC);
            }
        }
        Ok(())
    }

    fn answered(
        &mut self,
        actor: &str,
        request: Request,
        key: &str,
        etag: Option<String>,
        listed: Vec<String>,
    ) -> Result<(), String> {
        match request {
            Request::ReadRef => {
                let seen = match etag {
                    Some(etag) => {
                        self.read_ref()?;
                        let version = self.ref_version(&etag, None);
                        let commit = self.ref_commits.lock().unwrap()[version as usize - 1];
                        Some((commit, version))
                    }
                    None => None,
                };
                if actor == GC {
                    let (commit, _) = seen.ok_or("gc's ref read failed")?;
                    // Then the reflog: every record's old and new tips are roots too.
                    let (mut ids, mut trees) = self.reach(commit)?;
                    // Every stack too (read past the gate: nothing moves them).
                    for (branch, data) in self.live("refs/")? {
                        if branch != BRANCH {
                            let v: RefValue =
                                serde_json::from_slice(&data).map_err(|e| e.to_string())?;
                            let (i, t) = self.reach(v.commit)?;
                            ids.extend(i);
                            trees.extend(t);
                        }
                    }
                    for (_, data) in self.live("reflog/")? {
                        let r: Rewrite =
                            serde_json::from_slice(&data).map_err(|e| e.to_string())?;
                        for h in [r.old, r.new] {
                            let (i, t) = self.reach(h)?;
                            ids.extend(i);
                            trees.extend(t);
                        }
                    }
                    self.collector.marked_packs = self.packs_named_by(&ids)?;
                    self.collector.marked = Some((ids, trees));
                    return Ok(());
                }
                if self.writer(actor)?.rw.is_some() {
                    return self.rewriter_read(actor, seen);
                }
                if self.writer(actor)?.ld.is_some() {
                    return self.lander_read(actor, seen);
                }
                // The ref as read: the version names the body, which only that version had.
                let seen = match seen {
                    Some((c, v)) => {
                        let value = self.ref_values.lock().unwrap()[v as usize - 1].clone();
                        Some((self.name(&c)?, v, self.recent(&value)?))
                    }
                    None => None,
                };
                let lineage = match &seen {
                    Some((_, v, _)) => self.ref_values.lock().unwrap()[*v as usize - 1]
                        .lineage
                        .clone(),
                    None => Lineage::default(),
                };
                let w = self.writer(actor)?;
                if w.rechecking {
                    w.rechecking = false;
                } else if w.combining.is_some() {
                    w.round.head = seen;
                    w.round.lineage = lineage;
                } else {
                    w.seen = seen;
                    w.seen_lineage = lineage;
                }
            }
            Request::ListIntents => {
                let writers = listed
                    .iter()
                    .map(|k| self.intent_writer(k))
                    .collect::<Result<BTreeSet<_>, _>>()?;
                let w = self.writer(actor)?;
                if w.combining.is_some() {
                    w.round.listed = Some(writers);
                }
            }
            Request::PutIntent => {
                let id = key
                    .rsplit('/')
                    .next()
                    .unwrap_or(key)
                    .trim_end_matches(".json")
                    .to_owned();
                self.intent_writers.insert(id.clone(), actor.to_owned());
                let w = self.writer(actor)?;
                let at = w.intent.as_ref().map(|(_, at)| *at).unwrap_or(0);
                w.intent = Some((id, at));
                self.intents_in_order()?;
            }
            Request::PutCommit => {
                let h = Hash::from_hex(key.trim_start_matches("commits/"))
                    .map_err(|e| e.to_string())?;
                if !self.names.contains_key(&h) {
                    let c = self
                        .rt()
                        .block_on(self.observer.commit_obj(&h))
                        .map_err(|e| e.to_string())?;
                    let parent = c.parent.ok_or("a writer's commit has no parent")?;
                    let mut id = self.name(&parent)?;
                    // A rewrite's commit carries its original's name and the rewriter's mark
                    // (and keeps the original's `pail.intent`); a combining round's commit is
                    // its intent writer's, not the uploader's.
                    match (c.meta.get("rewritten.from"), c.meta.get("pail.intent")) {
                        (Some(from), _) => id = self.rewritten_name(id, from, actor)?,
                        (None, Some(intent)) => {
                            id.push(format!("{}+", self.intent_writer(intent)?))
                        }
                        (None, None) => id.push(actor.to_owned()),
                    }
                    self.names.insert(h, id.clone());
                    self.hashes.insert(id, h);
                }
                let id = self.name(&h)?;
                let w = self.writer(actor)?;
                if w.combining.is_some() && !w.round.batch.contains(&h) {
                    w.round.batch.push(h);
                }
                if let Some(ld) = w.ld.as_mut() {
                    ld.stored = Some(id);
                }
            }
            Request::PutRewrite => {
                let id = key.rsplit('/').next().unwrap_or(key).to_owned();
                let n = self.record_ids.len() as i64 + 1;
                self.record_ids.insert(id, n);
                let (data, _) = self.object(key)?.ok_or("the record vanished")?;
                let r: Rewrite = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
                let tip = self.name(&r.new)?;
                let rw = self
                    .writer(actor)?
                    .rw
                    .as_mut()
                    .ok_or("a record from no rewriter")?;
                (rw.version, rw.tip, rw.id) = (rw.read, Some(tip), n);
            }
            Request::ListCommits => {
                let (marked, _) = self.collector.marked.clone().ok_or("gc listed unmarked")?;
                let young = self.young()?;
                let mut doomed = BTreeSet::new();
                for (hex, _) in self.live("commits/")? {
                    let h = Hash::from_hex(&hex).map_err(|e| e.to_string())?;
                    let id = self.name(&h)?;
                    if !marked.contains(&id) && !young.commits.contains(&id) {
                        doomed.insert(format!("commits/{hex}"));
                    }
                }
                self.collector.doomed_commits = doomed;
                self.collector.spared = Some(young);
            }
            Request::ListPages => {
                let (_, marked) = self.collector.marked.clone().ok_or("gc listed unmarked")?;
                let young = self.young()?;
                let spared = self.collector.spared.clone().unwrap_or_default();
                let mut doomed = BTreeSet::new();
                for (hex, _) in self.live("pages/")? {
                    let key = format!("pages/{hex}");
                    let t = self.page(&key)?;
                    if !marked.contains(&t)
                        && !young.pages.contains(&t)
                        && !spared.pages.contains(&t)
                    {
                        doomed.insert(key);
                    }
                }
                self.collector.doomed_pages = doomed;
            }
            Request::ListPacks => {
                let marked = self.collector.marked_packs.clone();
                let young = self.young()?;
                let spared = self.collector.spared.clone().unwrap_or_default();
                let mut doomed = BTreeSet::new();
                for (name, _) in self.live("packs/")? {
                    let pack = self.pack_name(&Self::pack_hash(&name)?)?;
                    if !marked.contains(&pack)
                        && !young.packs.contains(&pack)
                        && !spared.packs.contains(&pack)
                    {
                        doomed.insert(format!("packs/{name}"));
                    }
                }
                self.collector.doomed_packs = doomed;
            }
            Request::DeleteCommit => {
                self.collector.doomed_commits.remove(key);
            }
            Request::DeletePage => {
                self.collector.doomed_pages.remove(key);
            }
            Request::DeletePack => {
                self.collector.doomed_packs.remove(key);
            }
            Request::Swap
            | Request::GetIntent
            | Request::DeleteIntent
            | Request::PutLease
            | Request::ListLeases
            | Request::DeleteLease
            | Request::RefreshLease
            | Request::AckLease => {}
        }
        Ok(())
    }

    /// A lander's read of the branch: a recheck learns nothing the model keeps; any other
    /// begins an attempt.
    fn lander_read(&mut self, actor: &str, seen: Option<(Hash, i64)>) -> Result<(), String> {
        let (_, version) = seen.ok_or(format!("{actor}'s ref read failed"))?;
        let lineage = self.ref_values.lock().unwrap()[version as usize - 1]
            .lineage
            .clone();
        let w = self.writer(actor)?;
        if w.rechecking {
            w.rechecking = false;
            return Ok(());
        }
        let ld = w.ld.as_mut().ok_or("not a lander")?;
        (ld.version, ld.lineage, ld.stored) = (version, lineage, None);
        Ok(())
    }

    /// `pail rebase` with `plan` on the tip's commits: read the ref, build the plan in memory
    /// (unleased; it reports what it built), then lease, store it and `rewrite_ref`.
    fn rebase(&mut self, name: &str, plan: &'static str) -> Result<(), String> {
        use pail::rebase::{self as rb, Action, Step};
        let repo = self.client(name);
        let events = self.sender.clone();
        let actor = name.to_owned();
        let committer = format!("{name} <{name}@example.com>");
        self.rt().spawn(async move {
            let result: anyhow::Result<()> = async {
                let (cur, _) = repo.read_ref_value(BRANCH).await?.context("no branch")?;
                let t = cur.commit;
                let p = repo.commit_meta(&t).await?.parent.context("the root")?;
                let step = |action, commit| Step {
                    action,
                    commit,
                    message: None,
                };
                let (base, steps) = if plan == "reword" {
                    let reword = Step {
                        message: Some(format!("{actor}'s reword\n")),
                        ..step(Action::Reword, t)
                    };
                    (p, vec![reword])
                } else {
                    let g = repo.commit_meta(&p).await?.parent.context("the root")?;
                    let steps = match plan {
                        "dropParent" => vec![step(Action::Drop, p), step(Action::Pick, t)],
                        "dropTip" => vec![step(Action::Pick, p), step(Action::Drop, t)],
                        "squash" => vec![step(Action::Pick, p), step(Action::Fixup, t)],
                        "squashUnder" => vec![step(Action::Pick, t), step(Action::Fixup, p)],
                        _ => vec![step(Action::Pick, t), step(Action::Pick, p)],
                    };
                    (g, steps)
                };
                let plan = rb::Plan {
                    branch: BRANCH.to_owned(),
                    expected: t,
                    base,
                    onto: base,
                    steps,
                };
                let built = rb::build(&repo, &plan, &committer, DATE).await?;
                let _ = events.send(Event::Planned {
                    actor: actor.clone(),
                    tip: built.tip,
                    map: built.map.clone(),
                    commits: built
                        .prepared()
                        .iter()
                        .map(|p| (p.commit, p.data.clone()))
                        .collect(),
                });
                let writing = repo.writing().await?;
                let out = async {
                    rb::store(&repo, &built).await?;
                    let req = RewriteRequest {
                        branch: BRANCH.to_owned(),
                        expected: t,
                        new: built.tip,
                        op: "rebase".into(),
                        committer: committer.clone(),
                        date: DATE.to_owned(),
                        map: built.map.clone(),
                        since: None,
                    };
                    repo.rewrite_ref(req, None).await
                }
                .await;
                writing.done().await?;
                out.map(|_| ())
            }
            .await;
            let _ = events.send(Event::Finished {
                actor,
                outcome: rewrite_outcome(result),
            });
        });
        self.began_rewrite(name, "rebase")?;
        self.writer(name)?.rw.as_mut().unwrap().plan = plan;
        Ok(())
    }

    /// The model's name for a commit `actor`'s rewrite made on `parent` of the commits
    /// `rewritten.from` names (`world.qnt` `remade`, `meldedId`): the commits melded into
    /// the first as `[tag, "&"]` pairs (theirs, and what was melded into each, in order),
    /// then the first's own, then the rewriter's mark.
    fn rewritten_name(&self, mut id: Id, from: &str, actor: &str) -> Result<Id, String> {
        let sources: Vec<Id> = from
            .split_whitespace()
            .map(|h| self.name(&Hash::from_hex(h).map_err(|e| e.to_string())?))
            .collect::<Result<_, _>>()?;
        let (lead, members) = sources
            .split_first()
            .ok_or("rewritten.from names nothing")?;
        let mut tags = vec![];
        for m in members {
            tags.extend(melded(m));
            tags.push(tag(m));
        }
        tags.extend(melded(lead));
        for t in tags {
            id.push(t);
            id.push("&".to_owned());
        }
        id.push(tag(lead));
        id.push(format!("~{actor}"));
        Ok(id)
    }

    /// What a rebase built: its commits named (as stored ones are), its tip, map and the
    /// commits it makes.
    fn planned(
        &mut self,
        actor: &str,
        tip: Hash,
        map: BTreeMap<Hash, Option<Hash>>,
        commits: Vec<(Hash, Bytes)>,
    ) -> Result<(), String> {
        let mut made = BTreeSet::new();
        for (h, data) in commits {
            let c = pail::objects::Commit::decode(&h, &data).map_err(|e| e.to_string())?;
            let from = c
                .meta
                .get("rewritten.from")
                .ok_or("a planned commit names no original")?;
            let parent = self.name(&c.parent.ok_or("a planned root")?)?;
            let id = self.rewritten_name(parent, from, actor)?;
            self.names.insert(h, id.clone());
            self.hashes.insert(id.clone(), h);
            made.insert(id);
        }
        let mut successors = BTreeMap::new();
        let mut dropped = BTreeSet::new();
        for (old, new) in &map {
            match new {
                Some(n) => {
                    successors.insert(self.name(old)?, self.name(n)?);
                }
                None => {
                    dropped.insert(self.name(old)?);
                }
            }
        }
        let new = self.name(&tip)?;
        let rw = self
            .writer(actor)?
            .rw
            .as_mut()
            .ok_or("a plan from no rewriter")?;
        (rw.new, rw.successors, rw.dropped, rw.made) = (new, successors, dropped, made);
        Ok(())
    }

    /// A rewriter spawned: it is one from now on, and its first ref read is its begin.
    /// `commit_opts` by `name`, reporting the head its publish leaves (`Event::Known`), then
    /// its outcome.
    fn spawn_commit(
        &self,
        name: &str,
        req: CommitRequest,
        uploads: Vec<Upload>,
        known_head: Option<KnownHead>,
    ) {
        let repo = self.client(name);
        let events = self.sender.clone();
        let actor = name.to_owned();
        self.rt().spawn(async move {
            let opts = CommitOptions {
                uploads,
                known_head,
                ..CommitOptions::default()
            };
            let outcome = match repo.commit_opts(req, opts).await {
                Ok(out) => {
                    let known = (out.head == out.commit).then(|| KnownHead {
                        commit: out.commit,
                        version: out.version,
                        recent: out.recent,
                        lineage: out.lineage,
                    });
                    let _ = events.send(Event::Known {
                        actor: actor.clone(),
                        known,
                    });
                    "Published"
                }
                Err(e) if e.downcast_ref::<ConflictError>().is_some() => "Conflicted",
                Err(_) => "Unknown",
            };
            let _ = events.send(Event::Finished { actor, outcome });
        });
    }

    fn began_rewrite(&mut self, name: &str, op: &'static str) -> Result<(), String> {
        let w = self.writer(name)?;
        w.base = Some(vec!["root".to_owned()]);
        w.found = None;
        w.rw = Some(Rewriter {
            op,
            ..Rewriter::default()
        });
        self.release(name, Request::ReadRef, None, Delivery::Answered)
    }

    /// A rewriter's ref read: its first is the one its `begin*` makes, fixing what it
    /// rewrites; a recheck learns nothing the model keeps; any other begins an attempt.
    fn rewriter_read(&mut self, actor: &str, seen: Option<(Hash, i64)>) -> Result<(), String> {
        let (commit, version) = seen.ok_or(format!("{actor}'s ref read failed"))?;
        let w = self.writer(actor)?;
        if w.rechecking {
            w.rechecking = false;
            return Ok(());
        }
        let rw = w.rw.as_ref().ok_or("not a rewriter")?;
        if !rw.expected.is_empty() {
            w.rw.as_mut().unwrap().read = version;
            return Ok(());
        }
        let op = rw.op;
        let parent = || -> Result<Id, String> {
            let c = self
                .rt()
                .block_on(self.observer.commit_obj(&commit))
                .map_err(|e| e.to_string())?;
            self.name(&c.parent.ok_or("rewriting the root")?)
        };
        let mut since = Lineage::default();
        let (expected, new, successors) = match op {
            // What the plan makes comes with `Planned`.
            "rebase" => {
                let expected = self.name(&commit)?;
                self.writer(actor)?.rw.as_mut().unwrap().expected = expected;
                return Ok(());
            }
            "reset" => (self.name(&commit)?, parent()?, BTreeMap::new()),
            _ => {
                // The record the version read names, as the undo read it next.
                let value = self.ref_values.lock().unwrap()[version as usize - 1].clone();
                let id = value
                    .lineage
                    .rewrite
                    .clone()
                    .ok_or("undoing a branch never rewritten")?;
                since = value.lineage;
                let r = self
                    .rt()
                    .block_on(self.observer.read_rewrite(BRANCH, &id))
                    .map_err(|e| e.to_string())?;
                let mut successors = BTreeMap::new();
                for (old, new) in &r.map {
                    if let Some(new) = new {
                        successors.insert(self.name(new)?, self.name(old)?);
                    }
                }
                (self.name(&r.new)?, self.name(&r.old)?, successors)
            }
        };
        let rw = self.writer(actor)?.rw.as_mut().unwrap();
        (rw.expected, rw.new, rw.successors, rw.since) = (expected, new, successors, since);
        Ok(())
    }

    /// Intent ids sort by creation time; the model sorts intents by the time the scenario
    /// gave them (`at`, ties broken by `RANK`). The scenario must give times in the order
    /// the intents are made.
    fn intents_in_order(&self) -> Result<(), String> {
        let rank = |w: &str| {
            ["alice", "bob", "carol", "dave"]
                .iter()
                .position(|n| *n == w)
        };
        let mut made: Vec<(&String, &str, i64)> = self
            .writers
            .iter()
            .filter_map(|(n, w)| w.intent.as_ref().map(|(id, at)| (id, n.as_str(), *at)))
            .collect();
        made.sort();
        let by_model: Vec<&str> = {
            let mut m = made.clone();
            m.sort_by_key(|(_, n, at)| (*at, rank(n)));
            m.into_iter().map(|(_, n, _)| n).collect()
        };
        let by_id: Vec<&str> = made.into_iter().map(|(_, n, _)| n).collect();
        if by_id != by_model {
            return Err(format!(
                "intents were made in the order {by_id:?}, but the scenario's times sort them {by_model:?}"
            ));
        }
        Ok(())
    }

    /// Process requests until `actor` waits at a request or has returned.
    fn settle(&mut self, actor: &str) -> Result<(), String> {
        self.settle_until(actor, |p| !p.held.is_empty())
    }

    /// Process requests until `ready` holds of `actor`'s progress, or it has returned.
    fn settle_until(
        &mut self,
        actor: &str,
        ready: impl Fn(&Progress) -> bool,
    ) -> Result<(), String> {
        loop {
            let done = if actor == GC {
                !self.collector.running
            } else {
                self.writer(actor)?.progress.outcome.is_some()
            };
            let p = self.progress(actor)?;
            if p.awaiting == 0 && (ready(p) || done) {
                return Ok(());
            }
            let event = self
                .events
                .recv_timeout(SETTLE)
                .map_err(|_| format!("{actor} made no request within {SETTLE:?}"))?;
            self.handle(event)?;
        }
    }

    /// Let `actor`'s held request through, which the model says is `expected` (and, if
    /// given, on `key`), waiting for it if the actor makes it alongside another.
    fn release(
        &mut self,
        actor: &str,
        expected: Request,
        key: Option<&str>,
        how: Delivery,
    ) -> Result<(), String> {
        self.release_one(actor, expected, key, how)?;
        self.settle(actor)
    }

    fn release_one(
        &mut self,
        actor: &str,
        expected: Request,
        key: Option<&str>,
        how: Delivery,
    ) -> Result<(), String> {
        let matches = |r: &Request, k: &String| *r == expected && key.is_none_or(|key| key == k);
        self.settle_until(actor, |p| p.held.iter().any(|(r, k, _)| matches(r, k)))?;
        let p = self.progress(actor)?;
        let Some(i) = p.held.iter().position(|(r, k, _)| matches(r, k)) else {
            let held: Vec<_> = p
                .held
                .iter()
                .map(|(r, k, _)| format!("{r:?} {k}"))
                .collect();
            return Err(format!(
                "the model's next step for {actor} is {expected:?} {key:?}, but it is waiting at \
                 {held:?} (outcome {:?})",
                p.outcome
            ));
        };
        if actor != GC {
            let w = self.writer(actor)?;
            match expected {
                Request::ReadRef if !w.rechecking && w.combining.is_none() => w.reading = true,
                Request::ReadRef | Request::Swap | Request::PutLease | Request::ListLeases => {
                    w.reading = false
                }
                _ => {}
            }
        }
        if actor != GC && expected == Request::Swap {
            // A lander rereads the branch after any swap it did not win, answered or not.
            let w = self.writer(actor)?;
            if !matches!(how, Delivery::Answered) || w.ld.is_some() {
                w.rechecking = true;
            }
        }
        self.send(actor, i, how)
    }

    /// Let `actor`'s `i`th held request through.
    fn send(&mut self, actor: &str, i: usize, how: Delivery) -> Result<(), String> {
        let p = self.progress(actor)?;
        let (_, _, go) = p.held.remove(i);
        p.awaiting += 1;
        go.send(how).map_err(|_| format!("{actor} stopped waiting"))
    }

    /// Release every request of `kinds` `actor` makes, as it makes them, until it waits at
    /// another kind or returns.
    fn drain(&mut self, actor: &str, kinds: &[Request]) -> Result<(), String> {
        loop {
            self.settle(actor)?;
            let p = self.progress(actor)?;
            let Some(i) = p.held.iter().position(|(r, _, _)| kinds.contains(r)) else {
                return Ok(());
            };
            self.send(actor, i, Delivery::Answered)?;
        }
    }

    /// `actor` starts a poll: `NO_COMB`.
    fn new_round(&mut self, actor: &str) -> Result<(), String> {
        let w = self.writer(actor)?;
        w.combining = Some("Polling");
        w.round = Round::default();
        Ok(())
    }

    fn returned_or_new_round(&mut self, actor: &str) -> Result<(), String> {
        if self.writer(actor)?.progress.outcome.is_some() {
            Ok(())
        } else {
            self.new_round(actor)
        }
    }

    /// After a poll request: once both are answered, the writer withdraws (landed or
    /// conflicted) or goes on deciding.
    fn polled(&mut self, actor: &str) -> Result<(), String> {
        let w = self.writer(actor)?;
        if w.round.head.is_some() && w.round.listed.is_some() {
            w.combining = Some(if w.progress.holds(Request::DeleteIntent) {
                "Withdrawing"
            } else {
                "Deciding"
            });
        }
        Ok(())
    }

    /// Process requests until `h`'s refresher waits at `request`.
    fn refresher_waits(&mut self, h: &str, request: Request) -> Result<(), String> {
        loop {
            let p = self.progress(h)?;
            if p.refreshing(request) && p.awaiting == 0 {
                return Ok(());
            }
            let event = self
                .events
                .recv_timeout(SETTLE)
                .map_err(|_| format!("{h}'s refresher made no {request:?} within {SETTLE:?}"))?;
            self.handle(event)?;
        }
    }

    fn release_refresh(&mut self, h: &str, request: Request, how: Delivery) -> Result<(), String> {
        let p = self.progress(h)?;
        let i = p
            .refresh
            .iter()
            .position(|(r, _, _)| *r == request)
            .ok_or_else(|| format!("{h}'s refresher is not waiting at {request:?}"))?;
        let (_, _, go) = p.refresh.remove(i);
        p.awaiting += 1;
        go.send(how)
            .map_err(|_| format!("{h}'s refresher stopped waiting"))
    }

    /// Process requests until the refresher's request is answered and it sleeps again
    /// (`sleepers` sleeps are waiting).
    fn refresher_sleeps(&mut self, sleepers: usize) -> Result<(), String> {
        let started = std::time::Instant::now();
        loop {
            while let Ok(event) = self.events.try_recv() {
                self.handle(event)?;
            }
            if self.clock.sleeping() >= sleepers {
                return Ok(());
            }
            if started.elapsed() > SETTLE {
                return Err("a refresher did not sleep again".into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn actor_arg(arguments: &[RuntimeValue]) -> Result<String, String> {
        Ok(text(arguments.first().ok_or("missing actor argument")?)?.to_owned())
    }
}

#[derive(Clone)]
struct Snapshot(RuntimeValue);

impl NormalizedRuntimeEvidence for Snapshot {
    fn resolve_name(&self, name: &str) -> Result<RuntimeValue, String> {
        match name {
            "state" => Ok(self.0.clone()),
            other => Err(format!("unknown model name {other}")),
        }
    }

    fn resolve_call(
        &self,
        _operator: &str,
        _arguments: &[RuntimeValue],
    ) -> Option<Result<RuntimeValue, String>> {
        None
    }
}

// ---------- the refinement map: real state to model state ----------

impl Harness {
    /// One object, read past every gate: its bytes and ETag.
    fn object(&self, key: &str) -> Result<Option<(Bytes, String)>, String> {
        let got = self
            .rt()
            .block_on(self.observer.store.get_opt(key))
            .map_err(|e| e.to_string())?;
        got.map(|(data, v)| Ok((data, v.etag.ok_or(format!("{key} has no ETag"))?)))
            .transpose()
    }

    /// The live objects under `prefix`: key (prefix removed) and bytes.
    fn live(&self, prefix: &str) -> Result<Vec<(String, Bytes)>, String> {
        let listed = self
            .rt()
            .block_on(self.observer.store.list(prefix))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for (key, _) in listed {
            let (data, _) = self.object(&key)?.ok_or(format!("{key} vanished"))?;
            out.push((key.trim_start_matches(prefix).to_owned(), data));
        }
        Ok(out)
    }

    /// Store layer: every object ever written under `prefix`, live or deleted, with its
    /// version from the book, mapped to the model's key and body.
    fn space(
        &self,
        prefix: &str,
        mut model: impl FnMut(&str, &Bytes) -> Result<(RuntimeValue, RuntimeValue), String>,
    ) -> Result<BTreeMap<RuntimeValue, RuntimeValue>, String> {
        let live: HashMap<String, Bytes> = self.live(prefix)?.into_iter().collect();
        let book = self.book.lock().unwrap();
        let mut out = BTreeMap::new();
        for (key, version) in book.versions.iter().filter(|(k, _)| k.starts_with(prefix)) {
            let name = key.trim_start_matches(prefix);
            let mut bodies = self.bodies.lock().unwrap();
            let (k, body) = match live.get(name) {
                Some(data) => {
                    let (k, body) = model(name, data)?;
                    bodies.insert(key.clone(), record([("k", k.clone()), ("b", body.clone())]));
                    (k, body)
                }
                None => {
                    let kept = bodies
                        .get(key)
                        .ok_or(format!("{key} was never seen live"))?;
                    (field(kept, "k")?.clone(), field(kept, "b")?.clone())
                }
            };
            out.insert(k, versioned(body, *version, live.contains_key(name)));
        }
        Ok(out)
    }

    /// The tree a page holds.
    fn tree(&self, key: &str) -> Result<Tree, String> {
        if let Some(t) = self.trees.lock().unwrap().get(key) {
            return Ok(t.clone());
        }
        let rt = self.rt();
        let h = Hash::from_hex(key.trim_start_matches("pages/")).map_err(|e| e.to_string())?;
        let entries = rt
            .block_on(self.observer.entries(h))
            .map_err(|e| e.to_string())?;
        let data = rt
            .block_on(self.observer.read_files(&entries))
            .map_err(|e| e.to_string())?;
        let tree: Tree = entries
            .iter()
            .zip(data)
            .map(|(e, d)| (e.path.clone(), unpad(&d)))
            .collect();
        self.trees
            .lock()
            .unwrap()
            .insert(key.to_owned(), tree.clone());
        Ok(tree)
    }

    /// A pack object's hash from its key without `packs/`.
    fn pack_hash(name: &str) -> Result<Hash, String> {
        Hash::from_hex(name.trim_end_matches(".pack")).map_err(|e| e.to_string())
    }

    /// The pack each big file of the page at `root` is in.
    fn locs_of(&self, root: &Hash) -> Result<BTreeMap<String, Hash>, String> {
        if let Some(l) = self.page_locs.lock().unwrap().get(root) {
            return Ok(l.clone());
        }
        let entries = self
            .rt()
            .block_on(self.observer.entries(*root))
            .map_err(|e| e.to_string())?;
        let locs: BTreeMap<String, Hash> = entries
            .iter()
            .filter_map(|e| match &e.loc {
                Loc::Pack { pack, .. } => Some((e.path.clone(), *pack)),
                _ => None,
            })
            .collect();
        self.page_locs.lock().unwrap().insert(*root, locs.clone());
        Ok(locs)
    }

    /// A pack's model name: the commit that first names it (its page has the pack, its
    /// parent's does not) stores it (`packOf`).
    fn pack_name(&self, pack: &Hash) -> Result<Id, String> {
        if let Some(n) = self.pack_names.lock().unwrap().get(pack) {
            return Ok(n.clone());
        }
        let mut commits: Vec<(Hash, Id)> =
            self.names.iter().map(|(h, id)| (*h, id.clone())).collect();
        commits.sort_by_key(|(_, id)| id.len());
        for (h, id) in commits {
            // A commit gc deleted names nothing new: its packs were named before.
            let Ok(c) = self.rt().block_on(self.observer.commit_obj(&h)) else {
                continue;
            };
            let mine = self.locs_of(&c.root)?;
            let parents: BTreeSet<Hash> = match c.parent {
                Some(p) => match self.rt().block_on(self.observer.commit_obj(&p)) {
                    Ok(p) => self.locs_of(&p.root)?.into_values().collect(),
                    Err(_) => BTreeSet::new(),
                },
                None => BTreeSet::new(),
            };
            for pk in mine.into_values().filter(|pk| !parents.contains(pk)) {
                self.pack_names
                    .lock()
                    .unwrap()
                    .entry(pk)
                    .or_insert_with(|| pack_of(&id));
            }
        }
        self.pack_names
            .lock()
            .unwrap()
            .get(pack)
            .cloned()
            .ok_or_else(|| format!("pack {} is named by no commit", pack.short()))
    }

    /// A page as the model keys it: its tree and the pack of each big file.
    fn page(&self, key: &str) -> Result<PageKey, String> {
        let root = Hash::from_hex(key.trim_start_matches("pages/")).map_err(|e| e.to_string())?;
        let locs = self
            .locs_of(&root)?
            .into_iter()
            .map(|(path, pack)| Ok((path, self.pack_name(&pack)?)))
            .collect::<Result<_, String>>()?;
        Ok((self.tree(key)?, locs))
    }

    /// The packs the pages of the commits `ids` name.
    fn packs_named_by(&self, ids: &BTreeSet<Id>) -> Result<BTreeSet<Id>, String> {
        let mut packs = BTreeSet::new();
        for id in ids {
            let h = self.hashes.get(id).ok_or(format!("no commit {id:?}"))?;
            let c = self
                .rt()
                .block_on(self.observer.commit_obj(h))
                .map_err(|e| e.to_string())?;
            for pack in self.locs_of(&c.root)?.into_values() {
                packs.insert(self.pack_name(&pack)?);
            }
        }
        Ok(packs)
    }

    /// Pail layer: a commit object as the model's `Commit`.
    fn commit_value(&self, h: &Hash) -> Result<RuntimeValue, String> {
        let rt = self.rt();
        let c = rt
            .block_on(self.observer.commit_obj(h))
            .map_err(|e| e.to_string())?;
        let tree = self.tree(&format!("pages/{}", c.root.hex()))?;
        let mut ancestors = BTreeSet::new();
        let mut parent = c.parent;
        while let Some(p) = parent {
            ancestors.insert(id_value(&self.name(&p)?));
            parent = rt
                .block_on(self.observer.commit_obj(&p))
                .map_err(|e| e.to_string())?
                .parent;
        }
        let locs = self
            .locs_of(&c.root)?
            .into_iter()
            .map(|(path, pack)| Ok((RuntimeValue::Text(path), id_value(&self.pack_name(&pack)?))))
            .collect::<Result<_, String>>()?;
        Ok(record([
            ("tree", tree_value(&tree)),
            ("ancestors", RuntimeValue::Set(ancestors)),
            ("locs", RuntimeValue::Map(locs)),
        ]))
    }

    /// A page as `bucket.qnt`'s `Page`.
    fn page_value((tree, locs): &PageKey) -> RuntimeValue {
        record([
            ("tree", tree_value(tree)),
            (
                "locs",
                RuntimeValue::Map(
                    locs.iter()
                        .map(|(p, id)| (RuntimeValue::Text(p.clone()), id_value(id)))
                        .collect(),
                ),
            ),
        ])
    }

    /// Commits, pages and packs written in this grace epoch: `Young`.
    fn young(&self) -> Result<Young, String> {
        let young: Vec<String> = {
            let book = self.book.lock().unwrap();
            book.written
                .iter()
                .filter(|(_, e)| **e == book.epoch)
                .map(|(k, _)| k.clone())
                .collect()
        };
        let mut out = Young::default();
        for key in young {
            if key.starts_with("pages/") {
                out.pages.insert(self.page(&key)?);
            } else if let Some(hex) = key.strip_prefix("commits/") {
                let h = Hash::from_hex(hex).map_err(|e| e.to_string())?;
                out.commits.insert(self.name(&h)?);
            } else if let Some(name) = key.strip_prefix("packs/") {
                out.packs.insert(self.pack_name(&Self::pack_hash(name)?)?);
            }
        }
        Ok(out)
    }

    /// Leases by holder: each holder's lease is one model object, written once per lease
    /// taken and once per release.
    fn locks_value(&self) -> Result<RuntimeValue, String> {
        let live: BTreeSet<String> = self
            .live("locks/")?
            .into_iter()
            .map(|(k, _)| format!("locks/{k}"))
            .collect();
        let book = self.book.lock().unwrap();
        let mut at: HashMap<String, i64> = HashMap::new();
        let mut holders: BTreeMap<String, (i64, Option<&'static str>, &'static str)> =
            BTreeMap::new();
        for (key, holder) in &book.holders {
            let kind = if key.starts_with("locks/exclusive-") {
                "Exclusive"
            } else {
                "Shared"
            };
            let h = holders.entry(holder.clone()).or_insert((0, None, kind));
            h.0 += book.versions.get(key).copied().unwrap_or(0);
            h.2 = kind;
            let latest = at.entry(holder.clone()).or_default();
            *latest = (*latest).max(book.leased_at.get(key).copied().unwrap_or(0));
            if live.contains(key) {
                h.1 = Some(kind);
            }
        }
        Ok(RuntimeValue::Map(
            holders
                .into_iter()
                .map(|(holder, (version, live, kind))| {
                    let written = at.get(&holder).copied().unwrap_or(0);
                    (
                        RuntimeValue::Text(holder),
                        versioned(
                            record([
                                ("kind", RuntimeValue::Text(live.unwrap_or(kind).to_owned())),
                                ("at", RuntimeValue::Int(written)),
                            ]),
                            version,
                            live.is_some(),
                        ),
                    )
                })
                .collect(),
        ))
    }

    /// Pail layer: the bucket as `bucket.qnt` lays it out.
    fn bucket_value(&self) -> Result<RuntimeValue, String> {
        let commits = self.space("commits/", |hex, _| {
            let h = Hash::from_hex(hex).map_err(|e| e.to_string())?;
            Ok((id_value(&self.name(&h)?), self.commit_value(&h)?))
        })?;
        let pages = self.space("pages/", |hex, _| {
            Ok((
                Self::page_value(&self.page(&format!("pages/{hex}"))?),
                RuntimeValue::Bool(true),
            ))
        })?;
        let packs = self.space("packs/", |name, data| {
            let pack = self.pack_name(&Self::pack_hash(name)?)?;
            let contents = pack_contents(data)?;
            Ok((
                id_value(&pack),
                set(contents.into_iter().map(RuntimeValue::Text)),
            ))
        })?;
        self.read_ref()?;
        let mut refs = BTreeMap::new();
        for (branch, data) in self.live("refs/")? {
            let value: RefValue = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
            let (_, etag) = self
                .object(&format!("refs/{branch}"))?
                .ok_or("the ref vanished")?;
            let body = record([
                ("commit", id_value(&self.name(&value.commit)?)),
                ("recent", self.recent_value(&self.recent(&value)?)),
                ("lineage", self.lineage_value(&value.lineage)?),
            ]);
            // The branch's versions are its ETags in order; a stack is written once.
            let version = if branch == BRANCH {
                self.ref_version(&etag, None)
            } else {
                self.book.lock().unwrap().versions[&format!("refs/{branch}")]
            };
            refs.insert(RuntimeValue::Text(branch), versioned(body, version, true));
        }
        let intents = self.space(&format!("intents/{BRANCH}/"), |name, data| {
            let writer = self.intent_writer(name)?;
            Ok((
                RuntimeValue::Text(writer.clone()),
                self.intent_value(&writer, data)?,
            ))
        })?;
        let reflog = self.space(&format!("reflog/{BRANCH}/"), |id, data| {
            let r: Rewrite = serde_json::from_slice(data).map_err(|e| e.to_string())?;
            let n = *self
                .record_ids
                .get(id)
                .ok_or(format!("record {id} was never seen written"))?;
            Ok((RuntimeValue::Int(n), self.rewrite_value(&r)?))
        })?;
        let young = self.young()?;
        Ok(record([
            ("pages", RuntimeValue::Map(pages)),
            ("commits", RuntimeValue::Map(commits)),
            ("refs", RuntimeValue::Map(refs)),
            ("locks", self.locks_value()?),
            ("intents", RuntimeValue::Map(intents)),
            ("reflog", RuntimeValue::Map(reflog)),
            ("packs", RuntimeValue::Map(packs)),
            ("young", Self::young_value(&young)),
        ]))
    }

    fn young_value(young: &Young) -> RuntimeValue {
        record([
            ("pages", set(young.pages.iter().map(Self::page_value))),
            ("commits", set(young.commits.iter().map(id_value))),
            ("packs", set(young.packs.iter().map(id_value))),
        ])
    }

    fn lineage_value(&self, l: &Lineage) -> Result<RuntimeValue, String> {
        let rewrite = match &l.rewrite {
            Some(id) => *self
                .record_ids
                .get(id)
                .ok_or(format!("record {id} was never seen written"))?,
            None => 0,
        };
        Ok(record([
            ("epoch", RuntimeValue::Int(l.epoch as i64)),
            ("rewrite", RuntimeValue::Int(rewrite)),
        ]))
    }

    /// A reflog record as the model's `Rewrite`.
    fn rewrite_value(&self, r: &Rewrite) -> Result<RuntimeValue, String> {
        let prev = self.lineage_value(&Lineage {
            epoch: 0,
            rewrite: r.prev.clone(),
        })?;
        let by = r.by.split(" <").next().unwrap_or_default().to_owned();
        let mut map = BTreeMap::new();
        let mut dropped = vec![];
        for (old, new) in &r.map {
            match new {
                Some(n) => {
                    map.insert(id_value(&self.name(old)?), id_value(&self.name(n)?));
                }
                None => dropped.push(id_value(&self.name(old)?)),
            }
        }
        Ok(record([
            ("op", RuntimeValue::Text(r.op.clone())),
            ("old", id_value(&self.name(&r.old)?)),
            ("new", id_value(&self.name(&r.new)?)),
            ("epoch", RuntimeValue::Int(r.epoch as i64)),
            ("prev", field(&prev, "rewrite")?.clone()),
            ("by", RuntimeValue::Text(by)),
            ("successors", RuntimeValue::Map(map)),
            ("dropped", set(dropped)),
        ]))
    }

    fn recent_value(&self, recent: &Recent) -> RuntimeValue {
        RuntimeValue::Map(
            recent
                .iter()
                .map(|(w, id)| (RuntimeValue::Text(w.clone()), id_value(id)))
                .collect(),
        )
    }

    /// An intent as the model's `Intent`: its base, its delta decoded from the leaf page it
    /// carries, and the time the scenario gave it.
    fn intent_value(&self, writer: &str, data: &Bytes) -> Result<RuntimeValue, String> {
        #[derive(serde::Deserialize)]
        struct Stored {
            base: Option<Hash>,
            puts: String,
        }
        let i: Stored = serde_json::from_slice(data).map_err(|e| e.to_string())?;
        let page = hex::decode(&i.puts).map_err(|e| e.to_string())?;
        let Page::Leaf(entries) = decode_page(&page).map_err(|e| e.to_string())? else {
            return Err(format!("{writer}'s intent carries no entry list"));
        };
        let data = self
            .rt()
            .block_on(self.observer.read_files(&entries))
            .map_err(|e| e.to_string())?;
        let delta: Tree = entries
            .iter()
            .zip(data)
            .map(|(e, d)| (e.path.clone(), String::from_utf8_lossy(&d).into_owned()))
            .collect();
        let base = i.base.ok_or(format!("{writer}'s intent has no base"))?;
        let at = self
            .writers
            .get(writer)
            .and_then(|w| w.intent.as_ref())
            .map(|(_, at)| *at)
            .ok_or(format!("{writer} has no intent"))?;
        Ok(record([
            ("base", id_value(&self.name(&base)?)),
            ("delta", tree_value(&delta)),
            ("at", RuntimeValue::Int(at)),
        ]))
    }

    fn ref_seen_value(&self, seen: &Option<(Id, i64, Recent)>, fallback: &Id) -> RuntimeValue {
        let (commit, version) = seen
            .as_ref()
            .map(|(c, v, _)| (c.clone(), *v))
            .unwrap_or((fallback.clone(), 0));
        record([
            ("commit", id_value(&commit)),
            ("version", RuntimeValue::Int(version)),
        ])
    }

    /// A known head as the model's `Known`.
    fn known_value(&self, k: Option<&KnownHead>) -> Result<RuntimeValue, String> {
        let Some(k) = k else {
            return Ok(record([
                ("has", RuntimeValue::Bool(false)),
                ("head", self.ref_seen_value(&None, &vec!["root".to_owned()])),
                ("recent", self.recent_value(&Recent::new())),
                ("lineage", self.lineage_value(&Lineage::default())?),
            ]));
        };
        let recent = self.landed(&k.recent)?;
        Ok(record([
            ("has", RuntimeValue::Bool(true)),
            (
                "head",
                self.ref_seen_value(&Some(self.known_seen(k)?), &vec![]),
            ),
            ("recent", self.recent_value(&recent)),
            ("lineage", self.lineage_value(&k.lineage)?),
        ]))
    }

    /// A known head as a writer's read of the ref: commit, version, landed intents.
    fn known_seen(&self, k: &KnownHead) -> Result<(Id, i64, Recent), String> {
        let etag = k
            .version
            .etag
            .as_deref()
            .ok_or("a known head has no ETag")?;
        Ok((
            self.name(&k.commit)?,
            self.ref_version(etag, None),
            self.landed(&k.recent)?,
        ))
    }

    fn landed(&self, recent: &[pail::objects::Landed]) -> Result<Recent, String> {
        recent
            .iter()
            .map(|l| Ok((self.intent_writer(&l.intent)?, self.name(&l.commit)?)))
            .collect()
    }

    /// A combining writer's round as the model's `Comb`.
    fn comb_value(&self, name: &str, w: &Writer) -> Result<RuntimeValue, String> {
        let root: Id = vec!["root".to_owned()];
        let r = &w.round;
        let recent = r
            .head
            .as_ref()
            .map(|(_, _, rc)| rc.clone())
            .unwrap_or_default();
        let mut entries = Vec::new();
        let mut tip = root.clone();
        let mut longest = 0;
        for h in &r.batch {
            let id = self.name(h)?;
            let commit = self.commit_value(h)?;
            let ancestors = field(&commit, "ancestors")?.clone();
            let depth = match &ancestors {
                RuntimeValue::Set(a) => a.len(),
                _ => 0,
            };
            if depth >= longest {
                (longest, tip) = (depth, id.clone());
            }
            let intent = id
                .last()
                .map(|a| a.trim_end_matches('+').to_owned())
                .unwrap_or_default();
            entries.push(record([
                ("intent", RuntimeValue::Text(intent)),
                ("commit", id_value(&id)),
                ("tree", field(&commit, "tree")?.clone()),
                ("ancestors", ancestors),
            ]));
        }
        // Withdrawing: landed (its intent in `recent`), or else conflicted.
        let then = match w.combining {
            Some("Withdrawing") | Some("Done") if r.head.is_some() => {
                if recent.contains_key(name) {
                    "Published"
                } else {
                    "Conflicted"
                }
            }
            _ => "Pending",
        };
        Ok(record([
            ("polledRef", RuntimeValue::Bool(r.head.is_some())),
            ("head", self.ref_seen_value(&r.head, &root)),
            ("recent", self.recent_value(&recent)),
            ("polledList", RuntimeValue::Bool(r.listed.is_some())),
            (
                "listed",
                set(r
                    .listed
                    .iter()
                    .flatten()
                    .map(|w| RuntimeValue::Text(w.clone()))),
            ),
            ("batch", set(entries)),
            ("tip", id_value(&tip)),
            ("then", RuntimeValue::Text(then.to_owned())),
            ("lineage", self.lineage_value(&r.lineage)?),
        ]))
    }

    fn rw_value(&self, w: &Writer) -> Result<RuntimeValue, String> {
        let root: Id = vec!["root".to_owned()];
        let rw = w.rw.clone().unwrap_or_default();
        let or_root = |id: Id| if id.is_empty() { root.clone() } else { id };
        Ok(record([
            ("op", RuntimeValue::Text(rw.op.to_owned())),
            ("plan", RuntimeValue::Text(rw.plan.to_owned())),
            ("expected", id_value(&or_root(rw.expected))),
            ("new", id_value(&or_root(rw.new))),
            (
                "successors",
                RuntimeValue::Map(
                    rw.successors
                        .iter()
                        .map(|(k, v)| (id_value(k), id_value(v)))
                        .collect(),
                ),
            ),
            ("dropped", set(rw.dropped.iter().map(id_value))),
            ("made", set(rw.made.iter().map(id_value))),
            ("since", self.lineage_value(&rw.since)?),
            ("version", RuntimeValue::Int(rw.version)),
            ("tip", id_value(&rw.tip.unwrap_or(root.clone()))),
            ("id", RuntimeValue::Int(rw.id)),
        ]))
    }

    fn ld_value(&self, w: &Writer) -> Result<RuntimeValue, String> {
        let root: Id = vec!["root".to_owned()];
        let ld = w.ld.clone().unwrap_or_default();
        Ok(record([
            ("calls", RuntimeValue::Int(ld.calls)),
            ("tip", id_value(&ld.tip.unwrap_or(root))),
            ("version", RuntimeValue::Int(ld.version)),
            ("lineage", self.lineage_value(&ld.lineage)?),
        ]))
    }

    fn writer_value(&self, name: &str, w: &Writer) -> Result<RuntimeValue, String> {
        let root: Id = vec!["root".to_owned()];
        let p = &w.progress;
        let phase = match (&p.outcome, w.combining, w.base.is_some()) {
            (_, _, false) if w.stacked => "Stacked",
            (_, _, false) => "Idle",
            (Some(_), _, _) => "Done",
            (None, _, true) if w.rw.is_some() => match p.held.first().map(|(r, _, _)| *r) {
                Some(Request::PutLease) => "Leasing",
                Some(Request::ListLeases) => "CheckingLeases",
                Some(Request::PutCommit) => "RwStoring",
                Some(Request::ReadRef) if w.rechecking => "RwRechecking",
                Some(Request::ReadRef) => "RwReading",
                Some(Request::Swap) => "RwSwapping",
                other => return Err(format!("{name} is between rewrite requests ({other:?})")),
            },
            (None, _, true) if w.ld.is_some() => match p.held.first().map(|(r, _, _)| *r) {
                Some(Request::PutLease) => "Leasing",
                Some(Request::ListLeases) => "CheckingLeases",
                Some(Request::ReadRef) if w.rechecking => "LdRechecking",
                Some(Request::ReadRef) => "LdReading",
                Some(Request::Swap) => "LdSwapping",
                other => return Err(format!("{name} is between landing requests ({other:?})")),
            },
            (None, Some(phase), _) => phase,
            (None, None, true) if p.holds(Request::PutLease) => "Leasing",
            (None, None, true) if p.holds(Request::ListLeases) => "CheckingLeases",
            (None, None, true) if p.holds(Request::ReadRef) && w.rechecking => "Rechecking",
            (None, None, true) if p.holds(Request::ReadRef) => "Reading",
            (None, None, true) if p.holds(Request::PutCommit) || p.holds(Request::ListIntents) => {
                "Uploading"
            }
            (None, None, true) if p.holds(Request::Swap) => "Swapping",
            (None, None, true) => {
                let held: Vec<_> = p.held.iter().map(|(r, _, _)| *r).collect();
                return Err(format!("{name} is between requests ({held:?})"));
            }
        };
        let outcome = p.outcome.unwrap_or("Pending");
        let base = w.base.clone().unwrap_or_else(|| root.clone());
        let seen_recent = w
            .seen
            .as_ref()
            .map(|(_, _, r)| r.clone())
            .unwrap_or_default();
        Ok(record([
            ("base", id_value(&base)),
            ("delta", tree_value(&w.delta)),
            ("phase", RuntimeValue::Text(phase.to_owned())),
            ("seen", self.ref_seen_value(&w.seen, &base)),
            ("seenRecent", self.recent_value(&seen_recent)),
            ("seenLineage", self.lineage_value(&w.seen_lineage)?),
            ("attempt", RuntimeValue::Int(w.attempts.max(1))),
            ("comb", self.comb_value(name, w)?),
            ("rw", self.rw_value(w)?),
            ("ld", self.ld_value(w)?),
            ("outcome", RuntimeValue::Text(outcome.to_owned())),
            ("located", RuntimeValue::Bool(w.found.is_some())),
            (
                "found",
                id_value(w.found.as_ref().map_or(&root, |(id, _)| id)),
            ),
            ("known", self.known_value(w.known.as_ref())?),
            (
                "foundAt",
                self.lineage_value(w.found.as_ref().map_or(&Lineage::default(), |(_, l)| l))?,
            ),
        ]))
    }

    fn collector_value(&self) -> Result<RuntimeValue, String> {
        let c = &self.collector;
        let held = c.progress.held.first().map(|(r, _, _)| *r);
        let phase = match (c.running, held) {
            (false, _) | (true, Some(Request::PutLease)) => "GcIdle",
            (true, Some(Request::ListLeases)) => "GcDraining",
            (true, Some(Request::ReadRef)) => "GcLocked",
            (true, Some(Request::ListCommits)) => "GcMarked",
            (true, Some(Request::DeleteCommit | Request::ListPages)) => "GcSweepingCommits",
            (true, Some(Request::DeletePage | Request::ListPacks)) => "GcSweepingPages",
            (true, Some(Request::DeletePack | Request::DeleteLease)) => "GcSweepingPacks",
            (true, other) => return Err(format!("gc is between requests ({other:?})")),
        };
        let (live_commits, live_pages) = c.marked.clone().unwrap_or_default();
        let spared = c.spared.clone().unwrap_or_default();
        // gc gives up at a delete its lease check refused, and releases its lease: what it
        // listed is no longer its to delete.
        let given_up = held == Some(Request::DeleteLease);
        let doomed_commits = c
            .doomed_commits
            .iter()
            .filter(|_| !given_up)
            .map(|k| {
                let h =
                    Hash::from_hex(k.trim_start_matches("commits/")).map_err(|e| e.to_string())?;
                Ok(id_value(&self.name(&h)?))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let doomed_pages = c
            .doomed_pages
            .iter()
            .filter(|_| !given_up)
            .map(|k| self.page(k).map(|p| Self::page_value(&p)))
            .collect::<Result<Vec<_>, String>>()?;
        let doomed_packs = c
            .doomed_packs
            .iter()
            .filter(|_| !given_up)
            .map(|k| {
                let name = k.trim_start_matches("packs/");
                Ok(id_value(&self.pack_name(&Self::pack_hash(name)?)?))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(record([
            ("phase", RuntimeValue::Text(phase.to_owned())),
            ("liveCommits", set(live_commits.iter().map(id_value))),
            ("livePages", set(live_pages.iter().map(Self::page_value))),
            ("livePacks", set(c.marked_packs.iter().map(id_value))),
            ("doomedCommits", set(doomed_commits)),
            ("doomedPages", set(doomed_pages)),
            ("doomedPacks", set(doomed_packs)),
            ("spared", Self::young_value(&spared)),
        ]))
    }

    /// The whole model state.
    fn state(&self) -> Result<RuntimeValue, String> {
        let bucket = self.bucket_value()?;
        let history = self
            .ref_commits
            .lock()
            .unwrap()
            .iter()
            .map(|h| self.name(h).map(|id| id_value(&id)))
            .collect::<Result<Vec<_>, _>>()?;
        let mut clients = BTreeMap::new();
        for (name, w) in &self.writers {
            clients.insert(
                RuntimeValue::Text(name.clone()),
                self.writer_value(name, w)?,
            );
        }
        let mut timing = BTreeMap::new();
        for holder in self.writers.keys().map(String::as_str).chain([GC]) {
            timing.insert(
                RuntimeValue::Text(holder.to_owned()),
                self.timing_value(holder),
            );
        }
        Ok(record([
            ("bucket", bucket),
            ("clients", RuntimeValue::Map(clients)),
            ("collector", self.collector_value()?),
            ("now", RuntimeValue::Int(self.clock.ticks())),
            ("timing", RuntimeValue::Map(timing)),
            ("history", RuntimeValue::List(history)),
        ]))
    }

    /// A holder's `Timing`: its lease's age from the lease itself, the rest as its
    /// refresher's requests were let through.
    fn timing_value(&self, holder: &str) -> RuntimeValue {
        self.note_running(holder);
        let refreshed = self
            .probe(holder)
            .map(|p| p.refreshed().as_secs() as i64 / TICK_SECS);
        let r = self
            .refreshers
            .lock()
            .unwrap()
            .get(holder)
            .cloned()
            .unwrap_or_default();
        let sent = match refreshed {
            Some(_) => r.sent,
            None => self
                .lease_sent
                .lock()
                .unwrap()
                .get(holder)
                .copied()
                .unwrap_or(0),
        };
        record([
            ("running", RuntimeValue::Bool(refreshed.is_some())),
            ("refreshed", RuntimeValue::Int(refreshed.unwrap_or(0))),
            ("done", RuntimeValue::Int(r.done)),
            ("inflight", RuntimeValue::Bool(r.inflight)),
            ("sent", RuntimeValue::Int(sent)),
            ("landed", RuntimeValue::Bool(r.landed)),
            ("checked", RuntimeValue::Int(r.checked)),
        ])
    }
}

impl generated::Implementation for Harness {
    type Evidence = Snapshot;

    /// A bucket holding the model's root commit, old, and an idle writer per model client.
    fn from_initial_state(initial: &RuntimeValue) -> Result<Self, String> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let bucket = Arc::new(InMemory::new());
        let observer = Repo::with_store(
            Store::from_object_store("memory://bucket", bucket.clone()),
            None,
        );
        let clock = Arc::new(TestClock::default());
        let (sender, events) = mpsc::channel();
        let RuntimeValue::Map(clients) = field(initial, "clients")? else {
            return Err("expected a map of clients".into());
        };
        let writers = clients
            .keys()
            .map(|k| Ok((text(k)?.to_owned(), Writer::default())))
            .collect::<Result<_, String>>()?;
        let mut h = Harness {
            rt: Some(rt),
            bucket,
            book: Arc::new(Mutex::new(Book {
                clock: clock.clone(),
                ..Book::default()
            })),
            observer,
            events,
            sender,
            writers,
            collector: Collector::default(),
            names: HashMap::new(),
            hashes: HashMap::new(),
            intent_writers: HashMap::new(),
            record_ids: HashMap::new(),
            bodies: Mutex::default(),
            trees: Mutex::default(),
            page_locs: Mutex::default(),
            pack_names: Mutex::default(),
            ref_etags: Mutex::default(),
            ref_commits: Mutex::default(),
            ref_values: Mutex::default(),
            clock,
            repos: Mutex::default(),
            refreshers: Mutex::default(),
            refresh_requests: 0,
            lease_sent: Mutex::default(),
        };
        let bucket = field(initial, "bucket")?;
        let RuntimeValue::Map(refs) = field(bucket, "refs")? else {
            return Err("expected a map of refs".into());
        };
        let tip = refs
            .get(&RuntimeValue::Text(BRANCH.to_owned()))
            .ok_or("the model has no branch ref")?;
        let root = id_of(field(field(tip, "body")?, "commit")?)?;
        let RuntimeValue::Map(commits) = field(bucket, "commits")? else {
            return Err("expected a map of commits".into());
        };
        let root_commit = commits
            .get(&id_value(&root))
            .ok_or("the model's head is not among its commits")?;
        let (delta, uploads) = h.files(
            &tree_of(field(field(root_commit, "body")?, "tree")?)?,
            &root[0],
        )?;
        if !uploads.is_empty() {
            return Err("the model's root must be small enough to inline".into());
        }
        let req = h.request(&root[0], None, delta);
        let made = h
            .rt()
            .block_on(h.observer.commit(req))
            .map_err(|e| e.to_string())?;
        h.names.insert(made.commit, root.clone());
        h.hashes.insert(root, made.commit);
        h.read_ref()?;
        // The root's objects were written past the gates: one version each, and old.
        let keys: Vec<String> = ["commits/", "pages/"]
            .iter()
            .map(|p| h.rt().block_on(h.observer.store.list(p)))
            .collect::<anyhow::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?
            .into_iter()
            .flatten()
            .map(|(k, _)| k)
            .collect();
        let mut book = h.book.lock().unwrap();
        for k in keys {
            book.versions.insert(k.clone(), 1);
            book.written.insert(k, 0);
        }
        book.epoch = 1;
        drop(book);
        Ok(h)
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot(self.state().unwrap_or_else(|e| panic!("snapshot: {e}")))
    }

    fn begin(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [c, base, delta] = arguments else {
            return Err(format!("begin takes 3 arguments: {arguments:?}"));
        };
        let name = text(c)?.to_owned();
        let base = id_of(base)?;
        let delta = tree_of(delta)?;
        let base_hash = *self
            .hashes
            .get(&base)
            .ok_or_else(|| format!("no commit {base:?}"))?;
        let (files, uploads) = self.files(&delta, &name)?;
        let req = self.request(&name, Some(base_hash), files);
        self.spawn_commit(&name, req, uploads, None);
        let w = self.writer(&name)?;
        w.base = Some(base);
        w.delta = delta;
        w.found = None;
        self.settle(&name)
    }

    /// `commit` by `c` on the head `from`'s publish left it: the model's `seen` is that
    /// head from the start, and its first attempt reads nothing.
    fn begin_known(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [c, from, delta] = arguments else {
            return Err(format!("beginKnown takes 3 arguments: {arguments:?}"));
        };
        let (name, from) = (text(c)?.to_owned(), text(from)?.to_owned());
        let delta = tree_of(delta)?;
        let k = self
            .writer(&from)?
            .known
            .clone()
            .ok_or(format!("{from} has no known head"))?;
        let (files, uploads) = self.files(&delta, &name)?;
        let req = self.request(&name, Some(k.commit), files);
        let seen = self.known_seen(&k)?;
        self.spawn_commit(&name, req, uploads, Some(k.clone()));
        let w = self.writer(&name)?;
        w.base = Some(seen.0.clone());
        w.delta = delta;
        w.seen = Some(seen);
        w.seen_lineage = k.lineage;
        w.attempts = 1;
        self.settle(&name)
    }

    fn lease(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.release(&c, Request::PutLease, None, Delivery::Answered)
    }

    fn check_leases(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.release(&c, Request::ListLeases, None, Delivery::Answered)
    }

    fn read_ref(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.release(&c, Request::ReadRef, None, Delivery::Answered)
    }

    fn recheck(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        if !self.writer(&c)?.rechecking {
            return Err(format!("{c} has no unanswered swap to recheck"));
        }
        self.release(&c, Request::ReadRef, None, Delivery::Answered)
    }

    fn swap(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        let how = Delivery::of(arguments.get(1).ok_or("swap takes a Delivery")?)?;
        self.release(&c, Request::Swap, None, how)
    }

    /// The commit and its page, and the intent listing made alongside them.
    fn upload(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.release_one(&c, Request::ListIntents, None, Delivery::Answered)?;
        self.release(&c, Request::PutCommit, None, Delivery::Answered)
    }

    fn intend(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        let at = match arguments.get(1) {
            Some(RuntimeValue::Int(at)) => *at,
            other => return Err(format!("intend takes a time, got {other:?}")),
        };
        self.writer(&c)?.intent = Some((String::new(), at));
        self.release(&c, Request::PutIntent, None, Delivery::Answered)?;
        self.new_round(&c)
    }

    fn poll_ref(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.release(&c, Request::ReadRef, None, Delivery::Answered)?;
        self.polled(&c)
    }

    fn poll_list(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.release(&c, Request::ListIntents, None, Delivery::Answered)?;
        self.polled(&c)
    }

    /// The writer has decided to poll again (it is not the oldest).
    fn wait(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        let p = &self.writer(&c)?.progress;
        if !(p.holds(Request::ReadRef) || p.holds(Request::ListIntents)) {
            return Err(format!("{c} is not polling again"));
        }
        self.new_round(&c)
    }

    /// The intents read and the batch stored: every intent read and commit upload it makes.
    fn gather(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.drain(&c, &[Request::GetIntent, Request::PutCommit])?;
        let p = &self.writer(&c)?.progress;
        if p.holds(Request::Swap) {
            self.writer(&c)?.combining = Some("BatchSwapping");
            Ok(())
        } else if p.holds(Request::ReadRef) && !p.holds(Request::ListIntents) {
            // The swap never left (the lease check refused it): it reads the ref instead.
            self.writer(&c)?.combining = Some("BatchRechecking");
            Ok(())
        } else {
            self.new_round(&c)
        }
    }

    fn batch_swap(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        let how = Delivery::of(arguments.get(1).ok_or("batchSwap takes a Delivery")?)?;
        self.release(&c, Request::Swap, None, how)?;
        let w = self.writer(&c)?;
        if w.rechecking {
            w.combining = Some("BatchRechecking");
            Ok(())
        } else if w.progress.holds(Request::DeleteIntent) {
            w.combining = Some("Cleaning");
            Ok(())
        } else {
            self.new_round(&c)
        }
    }

    /// The batch's intents deleted, all at once.
    fn cleanup(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.drain(&c, &[Request::DeleteIntent])?;
        self.returned_or_new_round(&c)
    }

    fn batch_recheck(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.release(&c, Request::ReadRef, None, Delivery::Answered)?;
        self.returned_or_new_round(&c)
    }

    fn withdraw(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.release(&c, Request::DeleteIntent, None, Delivery::Answered)
    }

    /// Reword the tip: `beginRebase` with the `reword` plan.
    fn begin_rewrite(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let name = Harness::actor_arg(arguments)?;
        self.rebase(&name, "reword")
    }

    fn begin_rebase(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let name = Harness::actor_arg(arguments)?;
        let plan = match arguments.get(1).map(text).transpose()? {
            Some("dropParent") => "dropParent",
            Some("dropTip") => "dropTip",
            Some("reorder") => "reorder",
            Some("squash") => "squash",
            Some("squashUnder") => "squashUnder",
            other => return Err(format!("unknown plan {other:?}")),
        };
        self.rebase(&name, plan)
    }

    /// Read the ref, then the record its lineage names, and publish the undo of it
    /// (`pail rebase --undo`).
    fn begin_undo(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let name = Harness::actor_arg(arguments)?;
        let repo = self.client(&name);
        let events = self.sender.clone();
        let actor = name.clone();
        let committer = format!("{name} <{name}@example.com>");
        self.rt().spawn(async move {
            let result: anyhow::Result<()> = async {
                let (cur, _) = repo.read_ref_value(BRANCH).await?.context("no branch")?;
                let id = cur.lineage.rewrite.clone().context("never rewritten")?;
                let rec = repo.read_rewrite(BRANCH, &id).await?;
                let req = RewriteRequest::undo(
                    BRANCH,
                    &rec,
                    cur.lineage.clone(),
                    committer,
                    DATE.to_owned(),
                );
                repo.rewrite_ref(req, None).await.map(|_| ())
            }
            .await;
            let _ = events.send(Event::Finished {
                actor,
                outcome: rewrite_outcome(result),
            });
        });
        self.began_rewrite(&name, "undo")
    }

    /// Move the branch to its tip's parent with `force_ref`, which reads the ref itself.
    fn begin_reset(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let name = Harness::actor_arg(arguments)?;
        let (tip, _) = Harness::read_ref(self)?;
        let parent = self
            .rt()
            .block_on(self.observer.commit_obj(&tip))
            .map_err(|e| e.to_string())?
            .parent
            .ok_or("resetting the root")?;
        let repo = self.client(&name);
        let events = self.sender.clone();
        let actor = name.clone();
        let committer = format!("{name} <{name}@example.com>");
        self.rt().spawn(async move {
            let result = repo.force_ref(BRANCH, parent, &committer).await;
            let _ = events.send(Event::Finished {
                actor,
                outcome: rewrite_outcome(result),
            });
        });
        self.began_rewrite(&name, "reset")
    }

    /// The plan's commits, stored in one wave.
    fn rw_store(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.release(&c, Request::PutCommit, None, Delivery::Answered)?;
        self.drain(&c, &[Request::PutCommit])
    }

    /// The ref read, then every commit it carries and its record, until it swaps or fails.
    fn rw_read(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.release(&c, Request::ReadRef, None, Delivery::Answered)?;
        self.drain(&c, &[Request::PutCommit, Request::PutRewrite])
    }

    fn rw_swap(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        let how = Delivery::of(arguments.get(1).ok_or("rwSwap takes a Delivery")?)?;
        self.release(&c, Request::Swap, None, how)
    }

    fn rw_recheck(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        if !self.writer(&c)?.rechecking {
            return Err(format!("{c} has no unanswered rewrite swap to recheck"));
        }
        self.release(&c, Request::ReadRef, None, Delivery::Answered)
    }

    /// Make `c`'s stack whole, past every gate: its commit on the branch's tip, then the
    /// branch `c` naming it.
    fn stack(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [c, delta] = arguments else {
            return Err(format!("stack takes 2 arguments: {arguments:?}"));
        };
        let name = text(c)?.to_owned();
        let delta = tree_of(delta)?;
        let (tip, _) = Harness::read_ref(self)?;
        let (files, uploads) = self.files(&delta, &name)?;
        if !uploads.is_empty() {
            return Err("a stack's files must be small enough to inline".into());
        }
        let req = self.request(&name, Some(tip), files);
        let fields = CommitFields {
            author: req.author,
            author_date: req.author_date,
            committer: req.committer,
            committer_date: req.committer_date,
            message: req.message,
            meta: req.meta,
        };
        let repo = Repo::with_store(self.gate_with(&name, true), None);
        let h = self
            .rt()
            .block_on(async {
                let h = repo
                    .write_commit(Some(tip), &req.delta, &fields)
                    .await?
                    .commit;
                repo.create_branch(&name, h).await?;
                anyhow::Ok(h)
            })
            .map_err(|e| e.to_string())?;
        let mut id = self.name(&tip)?;
        id.push(name.clone());
        self.names.insert(h, id.clone());
        self.hashes.insert(id, h);
        self.writer(&name)?.stacked = true;
        Ok(())
    }

    /// `land_stack` of `c`'s stack onto the branch: it leases first.
    fn begin_land(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let name = Harness::actor_arg(arguments)?;
        let (data, _) = self
            .object(&format!("refs/{name}"))?
            .ok_or(format!("{name} has no stack"))?;
        let v: RefValue = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        let stack = self.name(&v.commit)?;
        let repo = self.client(&name);
        let events = self.sender.clone();
        let actor = name.clone();
        let committer = format!("{name} <{name}@example.com>");
        self.rt().spawn(async move {
            let outcome = match repo.land_stack(&actor, BRANCH, &committer, DATE).await {
                Ok(_) => "Published",
                Err(e) if e.downcast_ref::<ConflictError>().is_some() => "Conflicted",
                Err(_) => "Unknown",
            };
            let _ = events.send(Event::Finished { actor, outcome });
        });
        let w = self.writer(&name)?;
        w.base = Some(vec!["root".to_owned()]);
        w.rechecking = false;
        w.progress = Progress::default();
        let calls = w.ld.as_ref().map_or(0, |ld| ld.calls) + 1;
        w.ld = Some(Lander {
            calls,
            stack,
            ..Lander::default()
        });
        self.settle(&name)
    }

    /// The branch read, then the copy's upload if it makes one, up to its swap.
    fn ld_read(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.release(&c, Request::ReadRef, None, Delivery::Answered)?;
        self.drain(&c, &[Request::PutCommit])
    }

    fn ld_swap(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        let how = Delivery::of(arguments.get(1).ok_or("ldSwap takes a Delivery")?)?;
        self.release(&c, Request::Swap, None, how)
    }

    /// `Repo::successor` of the commit `c` last read, from the lineage it read it at to the
    /// branch's now, read past every gate.
    fn locate(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let name = Harness::actor_arg(arguments)?;
        let w = self.writer(&name)?;
        let (seen, _, _) = w.seen.clone().ok_or(format!("{name} has read nothing"))?;
        let since = w.seen_lineage.clone();
        let commit = *self
            .hashes
            .get(&seen)
            .ok_or_else(|| format!("no commit {seen:?}"))?;
        let (data, _) = self
            .object(&format!("refs/{BRANCH}"))?
            .ok_or("the branch ref is gone")?;
        let now: RefValue = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        let found = self
            .rt()
            .block_on(
                self.observer
                    .successor(BRANCH, commit, &since, &now.lineage),
            )
            .map_err(|e| e.to_string())?;
        // None (nothing of its history survived) has no model counterpart: it names no commit.
        let id = match found {
            Some(h) => self.name(&h)?,
            None => vec!["none".to_owned()],
        };
        self.writer(&name)?.found = Some((id, now.lineage));
        Ok(())
    }

    fn ld_recheck(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        if !self.writer(&c)?.rechecking {
            return Err(format!("{c} has no swap to recheck"));
        }
        self.release(&c, Request::ReadRef, None, Delivery::Answered)
    }

    fn gc_lease(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        if !self.collector.running {
            let repo = self.client(GC);
            let events = self.sender.clone();
            self.rt().spawn(async move {
                let opts = pail::gc::GcOptions {
                    grace_secs: GRACE,
                    dry_run: false,
                    wait: Duration::from_secs(60),
                    now: None,
                };
                // `gc_with`, taking the lease apart so the harness can read its age.
                let outcome = async {
                    let lease = pail::lease::Lease::exclusive(&repo.store, opts.wait).await?;
                    let _ = events.send(Event::GcHeld {
                        probe: lease.probe(),
                    });
                    let report = pail::gc::gc_under(&repo, &opts, &lease).await;
                    let released = lease.release().await;
                    report?;
                    released
                }
                .await;
                let _ = events.send(Event::Finished {
                    actor: GC.to_owned(),
                    outcome: if outcome.is_ok() { "Done" } else { "Failed" },
                });
            });
            self.collector.running = true;
            self.settle(GC)?;
        }
        self.release(GC, Request::PutLease, None, Delivery::Answered)
    }

    fn gc_check_leases(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        self.release(GC, Request::ListLeases, None, Delivery::Answered)
    }

    fn gc_mark(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        self.release(GC, Request::ReadRef, None, Delivery::Answered)
    }

    fn gc_list_commits(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        self.release(GC, Request::ListCommits, None, Delivery::Answered)
    }

    fn gc_delete_commit(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let id = id_of(arguments.first().ok_or("gcDeleteCommit takes a commit")?)?;
        let h = self
            .hashes
            .get(&id)
            .ok_or_else(|| format!("no commit {id:?}"))?;
        let key = format!("commits/{}", h.hex());
        self.release(GC, Request::DeleteCommit, Some(&key), Delivery::Answered)
    }

    fn gc_list_pages(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        self.release(GC, Request::ListPages, None, Delivery::Answered)
    }

    fn gc_delete_page(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let page = arguments.first().ok_or("gcDeletePage takes a page")?;
        let want: PageKey = (
            tree_of(field(page, "tree")?)?,
            match field(page, "locs")? {
                RuntimeValue::Map(m) => m
                    .iter()
                    .map(|(k, v)| Ok((text(k)?.to_owned(), id_of(v)?)))
                    .collect::<Result<_, String>>()?,
                other => return Err(format!("expected a map of locations: {other:?}")),
            },
        );
        let keys: Vec<String> = self.trees.lock().unwrap().keys().cloned().collect();
        let key = keys
            .into_iter()
            .find(|k| self.page(k).is_ok_and(|p| p == want))
            .ok_or_else(|| format!("no page is {want:?}"))?;
        self.release(GC, Request::DeletePage, Some(&key), Delivery::Answered)
    }

    fn gc_list_packs(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        self.release(GC, Request::ListPacks, None, Delivery::Answered)
    }

    fn gc_delete_pack(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let id = id_of(arguments.first().ok_or("gcDeletePack takes a pack")?)?;
        let hash = self
            .pack_names
            .lock()
            .unwrap()
            .iter()
            .find(|(_, n)| **n == id)
            .map(|(h, _)| *h)
            .ok_or_else(|| format!("no pack {id:?}"))?;
        let key = format!("packs/{}.pack", hash.hex());
        self.release(GC, Request::DeletePack, Some(&key), Delivery::Answered)
    }

    fn gc_done(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        self.release(GC, Request::DeleteLease, None, Delivery::Answered)
    }

    fn grace_elapses(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        self.book.lock().unwrap().epoch += 1;
        Ok(())
    }

    /// The clock moves on a tick, and every refresher whose sleep ended makes its request.
    fn tick(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        let before = self.refresh_requests;
        let woke = self.clock.advance();
        while self.refresh_requests - before < woke {
            let event = self.events.recv_timeout(SETTLE).map_err(|_| {
                format!(
                    "{woke} refreshers woke, {} asked",
                    self.refresh_requests - before
                )
            })?;
            self.handle(event)?;
        }
        Ok(())
    }

    /// A refresher's put reaches the store (or does not); answered, its reply then waits.
    fn refresh_land(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let h = Harness::actor_arg(arguments)?;
        let how = Delivery::of(arguments.get(1).ok_or("refreshLand takes a Delivery")?)?;
        self.refresher_waits(&h, Request::RefreshLease)?;
        let asleep = self.clock.sleeping();
        self.release_refresh(&h, Request::RefreshLease, how)?;
        if matches!(how, Delivery::Answered) {
            self.refresher_waits(&h, Request::AckLease)?;
            self.refreshers.lock().unwrap().entry(h).or_default().landed = true;
        } else {
            self.refresher_sleeps(asleep + 1)?;
            let now = self.clock.ticks();
            let mut r = self.refreshers.lock().unwrap();
            let r = r.entry(h).or_default();
            (r.inflight, r.sent, r.done) = (false, 0, now);
        }
        Ok(())
    }

    /// The reply of a refresh arrives, and the refresher sleeps again.
    fn refresh_ack(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let h = Harness::actor_arg(arguments)?;
        self.refresher_waits(&h, Request::AckLease)?;
        let asleep = self.clock.sleeping();
        self.release_refresh(&h, Request::AckLease, Delivery::Answered)?;
        self.refresher_sleeps(asleep + 1)?;
        let now = self.clock.ticks();
        let mut r = self.refreshers.lock().unwrap();
        let r = r.entry(h).or_default();
        (r.inflight, r.sent, r.landed, r.done) = (false, 0, false, now);
        Ok(())
    }
}
