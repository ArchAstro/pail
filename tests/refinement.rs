//! `Repo::commit` and `gc` checked against the Quint model of pail (`verify/pail.qnt`).
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
use pail::hash::Hash;
use pail::manifest::Kind;
use pail::objects::RefValue;
use pail::repo::{CommitRequest, ConflictError, Delta, FileInput, Repo};
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
        8,
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
    ListCommits,
    DeleteCommit,
    ListPages,
    DeletePage,
    /// gc releasing its lease: its last request.
    DeleteLease,
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
    },
    Finished {
        actor: String,
        outcome: &'static str,
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
}

/// A writer's page puts wait for its commit upload to be released: the model uploads a
/// commit and its page in one step.
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

fn lost_reply() -> object_store::Error {
    object_store::Error::Generic {
        store: "gate",
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
        } else if key.starts_with("locks/") {
            Some(Request::PutLease)
        } else {
            None
        }
    }

    fn list_request(prefix: &str) -> Option<Request> {
        match prefix {
            "locks" => Some(Request::ListLeases),
            "commits" => Some(Request::ListCommits),
            "pages" => Some(Request::ListPages),
            _ => None,
        }
    }

    fn delete_request(&self, key: &str) -> Option<Request> {
        if key.starts_with("commits/") {
            Some(Request::DeleteCommit)
        } else if key.starts_with("pages/") {
            Some(Request::DeletePage)
        } else if key.starts_with("locks/") && self.actor == GC {
            Some(Request::DeleteLease)
        } else {
            None
        }
    }

    async fn hold(&self, request: Request, key: &str) -> Delivery {
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
        let _ = self.events.send(Event::Answered {
            actor: self.actor.clone(),
            request,
            key: key.to_owned(),
            etag,
        });
    }

    fn wrote(&self, key: &str) {
        let mut b = self.book.lock().unwrap();
        *b.versions.entry(key.to_owned()).or_default() += 1;
        let epoch = b.epoch;
        b.written.insert(key.to_owned(), epoch);
        if key.starts_with("locks/") {
            b.holders.insert(key.to_owned(), self.actor.clone());
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
        if key.starts_with("pages/") {
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
        let result = match self.hold(request, key).await {
            Delivery::Answered => self.put(location, payload, opts).await,
            Delivery::Lost => {
                let _ = self.put(location, payload, opts).await;
                Err(lost_reply())
            }
            Delivery::Dropped => Err(lost_reply()),
        };
        if request == Request::PutCommit {
            self.uploads.lock().unwrap().open = false;
        }
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
                    let one = futures::stream::once(async { Ok(location) }).boxed();
                    let out = gate.inner.delete_stream(one).next().await.unwrap();
                    if out.is_ok() {
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
            Some(Request::ListCommits) | Some(Request::ListPages)
        );
        let gate = self.clone();
        let prefix = prefix.cloned();
        futures::stream::once(async move {
            let key = prefix.as_ref().map(|p| p.to_string()).unwrap_or_default();
            if let Some(request) = request
                && !matches!(gate.hold(request, &key).await, Delivery::Answered)
            {
                return futures::stream::iter(vec![Err(lost_reply())]);
            }
            let items: Vec<_> = gate.inner.list(prefix.as_ref()).collect().await;
            if let Some(request) = request {
                gate.answered(request, &key, None);
            }
            let items = items
                .into_iter()
                .map(|m| m.map(|m| if ages { gate.aged(m) } else { m }))
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

fn id_value(id: &Id) -> RuntimeValue {
    RuntimeValue::List(id.iter().map(|a| RuntimeValue::Text(a.clone())).collect())
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

/// What an actor is doing, as read off its requests.
#[derive(Default)]
struct Progress {
    /// The request it is waiting at, if any.
    held: Option<(Request, String, oneshot::Sender<Delivery>)>,
    outcome: Option<&'static str>,
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
    /// The ref as its last attempt read it: commit and version.
    seen: Option<(Id, i64)>,
}

/// The collector's run, as read off its requests.
#[derive(Default)]
struct Collector {
    running: bool,
    progress: Progress,
    marked: Option<(BTreeSet<Id>, BTreeSet<Tree>)>,
    doomed_commits: BTreeSet<String>,
    doomed_pages: BTreeSet<String>,
    spared: Option<(BTreeSet<Tree>, BTreeSet<Id>)>,
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
    /// Each object's body as last seen, so a deleted one keeps it (`store.qnt` tombstones).
    bodies: Mutex<HashMap<String, RuntimeValue>>,
    /// Pages by key: the tree each holds.
    trees: Mutex<HashMap<String, Tree>>,
    /// The ref's ETags in order: version n is the n-th.
    ref_etags: Mutex<Vec<String>>,
    /// The commit each version of the ref named, oldest first.
    ref_commits: Mutex<Vec<Hash>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Actors still held (a writer that went on to combine) are abandoned, not released.
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
        let gate = Gate {
            inner: self.bucket.clone(),
            actor: actor.to_owned(),
            events: self.sender.clone(),
            book: self.book.clone(),
            uploads: Arc::default(),
        };
        Store::from_object_store("memory://bucket", Arc::new(gate))
    }

    fn files(&self, tree: &Tree) -> Result<Delta, String> {
        let inputs: Vec<FileInput> = tree
            .iter()
            .map(|(path, data)| FileInput {
                path: path.clone(),
                kind: Kind::File,
                data: Bytes::from(data.clone().into_bytes()),
            })
            .collect();
        // Small files are stored inline: nothing to upload, so no lease to take.
        let (entries, uploads) = self.observer.prepare_files(&inputs, &HashMap::new());
        if !uploads.is_empty() {
            return Err("the model's files must be small enough to inline".into());
        }
        Ok(entries
            .into_iter()
            .map(|e| (e.path.clone(), Some(e)))
            .collect())
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
        Ok((value.commit, self.ref_version(&etag, Some(value.commit))))
    }

    fn ref_version(&self, etag: &str, commit: Option<Hash>) -> i64 {
        let mut etags = self.ref_etags.lock().unwrap();
        if let Some(i) = etags.iter().position(|e| e == etag) {
            return i as i64 + 1;
        }
        etags.push(etag.to_owned());
        if let Some(c) = commit {
            self.ref_commits.lock().unwrap().push(c);
        }
        etags.len() as i64
    }

    fn name(&self, h: &Hash) -> Result<Id, String> {
        self.names
            .get(h)
            .cloned()
            .ok_or_else(|| format!("commit {} has no writer the scenario knows", h.short()))
    }

    /// Every commit the branch reached at `head`, and their trees.
    fn reach(&self, head: Hash) -> Result<(BTreeSet<Id>, BTreeSet<Tree>), String> {
        let (mut ids, mut trees) = (BTreeSet::new(), BTreeSet::new());
        let mut cur = Some(head);
        while let Some(h) = cur {
            ids.insert(self.name(&h)?);
            let c = self
                .rt()
                .block_on(self.observer.commit_obj(&h))
                .map_err(|e| e.to_string())?;
            trees.insert(self.tree(&format!("pages/{}", c.root.hex()))?);
            cur = c.parent;
        }
        Ok((ids, trees))
    }

    fn handle(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::Held {
                actor,
                request,
                key,
                go,
            } => {
                if actor != GC {
                    let w = self.writer(&actor)?;
                    if request == Request::ReadRef && !w.rechecking {
                        w.attempts += 1;
                    }
                }
                let p = self.progress(&actor)?;
                if let Some((was, _, _)) = &p.held {
                    return Err(format!(
                        "{actor} made a {request:?} request while one {was:?} was held"
                    ));
                }
                p.held = Some((request, key, go));
            }
            Event::Answered {
                actor,
                request,
                key,
                etag,
            } => self.answered(&actor, request, &key, etag)?,
            Event::Finished { actor, outcome } => {
                if actor == GC {
                    self.collector = Collector::default();
                } else {
                    self.writer(&actor)?.progress.outcome = Some(outcome);
                }
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
                    self.collector.marked = Some(self.reach(commit)?);
                    return Ok(());
                }
                let seen = match seen {
                    Some((c, v)) => Some((self.name(&c)?, v)),
                    None => None,
                };
                let w = self.writer(actor)?;
                if w.rechecking {
                    w.rechecking = false;
                } else {
                    w.seen = seen;
                }
            }
            Request::PutCommit => {
                let h = Hash::from_hex(key.trim_start_matches("commits/"))
                    .map_err(|e| e.to_string())?;
                if !self.names.contains_key(&h) {
                    let parent = self
                        .rt()
                        .block_on(self.observer.commit_obj(&h))
                        .map_err(|e| e.to_string())?
                        .parent
                        .ok_or("a writer's commit has no parent")?;
                    let mut id = self.name(&parent)?;
                    id.push(actor.to_owned());
                    self.names.insert(h, id.clone());
                    self.hashes.insert(id, h);
                }
            }
            Request::ListCommits => {
                let (marked, _) = self.collector.marked.clone().ok_or("gc listed unmarked")?;
                let young = self.young()?;
                let mut doomed = BTreeSet::new();
                for (hex, _) in self.live("commits/")? {
                    let h = Hash::from_hex(&hex).map_err(|e| e.to_string())?;
                    let id = self.name(&h)?;
                    if !marked.contains(&id) && !young.1.contains(&id) {
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
                    let t = self.tree(&key)?;
                    if !marked.contains(&t) && !young.0.contains(&t) && !spared.0.contains(&t) {
                        doomed.insert(key);
                    }
                }
                self.collector.doomed_pages = doomed;
            }
            Request::DeleteCommit => {
                self.collector.doomed_commits.remove(key);
            }
            Request::DeletePage => {
                self.collector.doomed_pages.remove(key);
            }
            Request::Swap
            | Request::PutIntent
            | Request::PutLease
            | Request::ListLeases
            | Request::DeleteLease => {}
        }
        Ok(())
    }

    /// Process requests until `actor` waits at its next one or has returned.
    fn settle(&mut self, actor: &str) -> Result<(), String> {
        loop {
            let done = if actor == GC {
                !self.collector.running
            } else {
                self.writer(actor)?.progress.outcome.is_some()
            };
            if self.progress(actor)?.held.is_some() || done {
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
    /// given, on `key`).
    fn release(
        &mut self,
        actor: &str,
        expected: Request,
        key: Option<&str>,
        how: Delivery,
    ) -> Result<(), String> {
        let p = self.progress(actor)?;
        let Some((request, held_key, go)) = p.held.take() else {
            return Err(format!(
                "the model's next step for {actor} is {expected:?}, but it is making no request \
                 (outcome {:?})",
                p.outcome
            ));
        };
        if request != expected || key.is_some_and(|k| k != held_key) {
            return Err(format!(
                "the model's next step for {actor} is {expected:?} {key:?}, but its next request \
                 is {request:?} {held_key}"
            ));
        }
        if actor != GC && !matches!(how, Delivery::Answered) {
            self.writer(actor)?.rechecking = true;
        }
        go.send(how)
            .map_err(|_| format!("{actor} stopped waiting"))?;
        self.settle(actor)
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
            .map(|(e, d)| (e.path.clone(), String::from_utf8_lossy(&d).into_owned()))
            .collect();
        self.trees
            .lock()
            .unwrap()
            .insert(key.to_owned(), tree.clone());
        Ok(tree)
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
        Ok(record([
            ("tree", tree_value(&tree)),
            ("ancestors", RuntimeValue::Set(ancestors)),
        ]))
    }

    /// Commits and pages written in this grace epoch: `Young` (trees, ids).
    fn young(&self) -> Result<(BTreeSet<Tree>, BTreeSet<Id>), String> {
        let young: Vec<String> = {
            let book = self.book.lock().unwrap();
            book.written
                .iter()
                .filter(|(_, e)| **e == book.epoch)
                .map(|(k, _)| k.clone())
                .collect()
        };
        let (mut pages, mut commits) = (BTreeSet::new(), BTreeSet::new());
        for key in young {
            if key.starts_with("pages/") {
                pages.insert(self.tree(&key)?);
            } else if let Some(hex) = key.strip_prefix("commits/") {
                let h = Hash::from_hex(hex).map_err(|e| e.to_string())?;
                commits.insert(self.name(&h)?);
            }
        }
        Ok((pages, commits))
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
            if live.contains(key) {
                h.1 = Some(kind);
            }
        }
        Ok(RuntimeValue::Map(
            holders
                .into_iter()
                .map(|(holder, (version, live, kind))| {
                    (
                        RuntimeValue::Text(holder),
                        versioned(
                            RuntimeValue::Text(live.unwrap_or(kind).to_owned()),
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
                tree_value(&self.tree(&format!("pages/{hex}"))?),
                RuntimeValue::Bool(true),
            ))
        })?;
        self.read_ref()?;
        let mut refs = BTreeMap::new();
        for (branch, data) in self.live("refs/")? {
            let value: RefValue = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
            let (_, etag) = self
                .object(&format!("refs/{branch}"))?
                .ok_or("the ref vanished")?;
            let body = record([("commit", id_value(&self.name(&value.commit)?))]);
            refs.insert(
                RuntimeValue::Text(branch),
                versioned(body, self.ref_version(&etag, None), true),
            );
        }
        let (young_pages, young_commits) = self.young()?;
        Ok(record([
            ("pages", RuntimeValue::Map(pages)),
            ("commits", RuntimeValue::Map(commits)),
            ("refs", RuntimeValue::Map(refs)),
            ("locks", self.locks_value()?),
            (
                "young",
                record([
                    ("pages", set(young_pages.iter().map(tree_value))),
                    ("commits", set(young_commits.iter().map(id_value))),
                ]),
            ),
        ]))
    }

    fn writer_value(&self, name: &str, w: &Writer) -> Result<RuntimeValue, String> {
        let root: Id = vec!["root".to_owned()];
        let held = w.progress.held.as_ref().map(|(r, _, _)| *r);
        let phase = match (&w.progress.outcome, held, w.base.is_some()) {
            (_, _, false) => "Idle",
            (_, Some(Request::PutIntent), _) | (Some(_), _, _) => "Done",
            (None, Some(Request::PutLease), _) => "Leasing",
            (None, Some(Request::ListLeases), _) => "CheckingLeases",
            (None, Some(Request::ReadRef), _) if w.rechecking => "Rechecking",
            (None, Some(Request::ReadRef), _) => "Reading",
            (None, Some(Request::PutCommit), _) => "Uploading",
            (None, Some(Request::Swap), _) => "Swapping",
            (None, other, true) => return Err(format!("{name} is between requests ({other:?})")),
        };
        let outcome = match (&w.progress.outcome, held) {
            (Some(o), _) => *o,
            (None, Some(Request::PutIntent)) => "Combining",
            (None, _) => "Pending",
        };
        let base = w.base.clone().unwrap_or_else(|| root.clone());
        let (seen, seen_version) = w.seen.clone().unwrap_or((base.clone(), 0));
        Ok(record([
            ("base", id_value(&base)),
            ("delta", tree_value(&w.delta)),
            ("phase", RuntimeValue::Text(phase.to_owned())),
            (
                "seen",
                record([
                    ("commit", id_value(&seen)),
                    ("version", RuntimeValue::Int(seen_version)),
                ]),
            ),
            ("attempt", RuntimeValue::Int(w.attempts.max(1))),
            ("outcome", RuntimeValue::Text(outcome.to_owned())),
        ]))
    }

    fn collector_value(&self) -> Result<RuntimeValue, String> {
        let c = &self.collector;
        let held = c.progress.held.as_ref().map(|(r, _, _)| *r);
        let phase = match (c.running, held) {
            (false, _) | (true, Some(Request::PutLease)) => "GcIdle",
            (true, Some(Request::ListLeases)) => "GcDraining",
            (true, Some(Request::ReadRef)) => "GcLocked",
            (true, Some(Request::ListCommits)) => "GcMarked",
            (true, Some(Request::DeleteCommit | Request::ListPages)) => "GcSweepingCommits",
            (true, Some(Request::DeletePage | Request::DeleteLease)) => "GcSweepingPages",
            (true, other) => return Err(format!("gc is between requests ({other:?})")),
        };
        let (live_commits, live_pages) = c.marked.clone().unwrap_or_default();
        let (spared_pages, spared_commits) = c.spared.clone().unwrap_or_default();
        let doomed_commits = c
            .doomed_commits
            .iter()
            .map(|k| {
                let h =
                    Hash::from_hex(k.trim_start_matches("commits/")).map_err(|e| e.to_string())?;
                Ok(id_value(&self.name(&h)?))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let doomed_pages = c
            .doomed_pages
            .iter()
            .map(|k| self.tree(k).map(|t| tree_value(&t)))
            .collect::<Result<Vec<_>, String>>()?;
        Ok(record([
            ("phase", RuntimeValue::Text(phase.to_owned())),
            ("liveCommits", set(live_commits.iter().map(id_value))),
            ("livePages", set(live_pages.iter().map(tree_value))),
            ("doomedCommits", set(doomed_commits)),
            ("doomedPages", set(doomed_pages)),
            (
                "spared",
                record([
                    ("pages", set(spared_pages.iter().map(tree_value))),
                    ("commits", set(spared_commits.iter().map(id_value))),
                ]),
            ),
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
        Ok(record([
            ("bucket", bucket),
            ("clients", RuntimeValue::Map(clients)),
            ("collector", self.collector_value()?),
            ("history", RuntimeValue::List(history)),
        ]))
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
            book: Arc::default(),
            observer,
            events,
            sender,
            writers,
            collector: Collector::default(),
            names: HashMap::new(),
            hashes: HashMap::new(),
            bodies: Mutex::default(),
            trees: Mutex::default(),
            ref_etags: Mutex::default(),
            ref_commits: Mutex::default(),
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
        let delta = h.files(&tree_of(field(field(root_commit, "body")?, "tree")?)?)?;
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
        let req = self.request(&name, Some(base_hash), self.files(&delta)?);
        let repo = Repo::with_store(self.gate(&name), None);
        let events = self.sender.clone();
        let actor = name.clone();
        self.rt().spawn(async move {
            let outcome = match repo.commit(req).await {
                Ok(_) => "Published",
                Err(e) if e.downcast_ref::<ConflictError>().is_some() => "Conflicted",
                Err(_) => "Unknown",
            };
            let _ = events.send(Event::Finished { actor, outcome });
        });
        let w = self.writer(&name)?;
        w.base = Some(base);
        w.delta = delta;
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

    fn upload(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::actor_arg(arguments)?;
        self.release(&c, Request::PutCommit, None, Delivery::Answered)
    }

    fn gc_lease(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        if !self.collector.running {
            let repo = Repo::with_store(self.gate(GC), None);
            let events = self.sender.clone();
            self.rt().spawn(async move {
                let opts = pail::gc::GcOptions {
                    grace_secs: GRACE,
                    dry_run: false,
                    wait: Duration::from_secs(60),
                    now: None,
                };
                let outcome = match pail::gc::gc_with(&repo, &opts).await {
                    Ok(_) => "Done",
                    Err(_) => "Failed",
                };
                let _ = events.send(Event::Finished {
                    actor: GC.to_owned(),
                    outcome,
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
        let t = tree_of(arguments.first().ok_or("gcDeletePage takes a tree")?)?;
        let key = {
            let trees = self.trees.lock().unwrap();
            trees
                .iter()
                .find(|(_, v)| **v == t)
                .map(|(k, _)| k.clone())
                .ok_or_else(|| format!("no page holds {t:?}"))?
        };
        self.release(GC, Request::DeletePage, Some(&key), Delivery::Answered)
    }

    fn gc_done(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        self.release(GC, Request::DeleteLease, None, Delivery::Answered)
    }

    fn grace_elapses(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        self.book.lock().unwrap().epoch += 1;
        Ok(())
    }
}
