//! `Repo::commit` checked against the Quint model of publishing (`verify/publish.qnt`).
//!
//! A model action is one request a client makes to the bucket. Each client is a real `Repo`
//! over a shared in-memory bucket, behind a gate that holds its ref reads, commit uploads
//! and ref swaps until the scenario releases them, so the clients interleave exactly as the
//! scenario says. A swap can be released with its reply lost (applied, or never received).
//! After every action the bucket, and each client's progress as read off the requests it
//! made, must equal the model's state. `verify/README.md` has the mapping.

#[path = "refinement/generated.rs"]
mod generated;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
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
/// How long a released client may take to reach its next request.
const SETTLE: Duration = Duration::from_secs(20);

#[test]
fn commit_refines_the_publish_model() {
    let results = generated::refine_all::<Harness>().unwrap_or_else(|e| panic!("{e}"));
    for r in &results {
        eprintln!(
            "{} refined {} obligations",
            r.scenario, r.evaluated_obligations
        );
    }
    assert_eq!(
        results.len(),
        5,
        "every scenario in verify/scenarios.qnt ran"
    );
}

// ---------- the gate ----------

/// A request the model sees as one step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    ReadRef,
    PutCommit,
    Swap,
    /// The first write of the combining path: the client gave up swapping directly.
    PutIntent,
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
        client: String,
        request: Request,
        go: oneshot::Sender<Delivery>,
    },
    Answered {
        client: String,
        request: Request,
        key: String,
        etag: Option<String>,
    },
    Finished {
        client: String,
        outcome: &'static str,
    },
}

/// One client's view of the bucket: every request passes through, and the ones the model
/// counts as steps wait for the scenario to release them.
struct Gate {
    inner: Arc<InMemory>,
    client: String,
    events: mpsc::Sender<Event>,
}

impl fmt::Debug for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Gate({})", self.client)
    }
}

impl fmt::Display for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gate for {}", self.client)
    }
}

fn lost_reply() -> object_store::Error {
    object_store::Error::Generic {
        store: "gate",
        source: "request timed out".into(),
    }
}

impl Gate {
    fn write_request(location: &Path) -> Option<Request> {
        let key = location.as_ref();
        if key == format!("refs/{BRANCH}") {
            Some(Request::Swap)
        } else if key.starts_with("commits/") {
            Some(Request::PutCommit)
        } else if key.starts_with("intents/") {
            Some(Request::PutIntent)
        } else {
            None
        }
    }

    async fn hold(&self, request: Request) -> Delivery {
        let (go, released) = oneshot::channel();
        let _ = self.events.send(Event::Held {
            client: self.client.clone(),
            request,
            go,
        });
        // A harness that went away fails the request rather than performing it.
        released.await.unwrap_or(Delivery::Dropped)
    }

    fn answered(&self, request: Request, location: &Path, etag: Option<String>) {
        let _ = self.events.send(Event::Answered {
            client: self.client.clone(),
            request,
            key: location.to_string(),
            etag,
        });
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
        let Some(request) = Gate::write_request(location) else {
            return self.inner.put_opts(location, payload, opts).await;
        };
        let result = match self.hold(request).await {
            Delivery::Answered => self.inner.put_opts(location, payload, opts).await,
            Delivery::Lost => {
                let _ = self.inner.put_opts(location, payload, opts).await;
                Err(lost_reply())
            }
            Delivery::Dropped => Err(lost_reply()),
        };
        self.answered(request, location, None);
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
        let release = self.hold(Request::ReadRef).await;
        let result = match release {
            Delivery::Answered => self.inner.get_opts(location, options).await,
            Delivery::Lost | Delivery::Dropped => Err(lost_reply()),
        };
        let etag = result.as_ref().ok().and_then(|r| r.meta.e_tag.clone());
        self.answered(Request::ReadRef, location, etag);
        result
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, StoreResult<Path>>,
    ) -> BoxStream<'static, StoreResult<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, StoreResult<ObjectMeta>> {
        self.inner.list(prefix)
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

// ---------- the implementation the model is checked against ----------

/// A commit as the model names it: its writer and generation.
type Id = (String, i64);

fn id_value((by, generation): &Id) -> RuntimeValue {
    RuntimeValue::Record(BTreeMap::from([
        ("by".to_owned(), RuntimeValue::Text(by.clone())),
        ("gen".to_owned(), RuntimeValue::Int(*generation)),
    ]))
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
    let RuntimeValue::Int(generation) = field(v, "gen")? else {
        return Err(format!("expected an integer generation: {v:?}"));
    };
    Ok((text(field(v, "by")?)?.to_owned(), *generation))
}

fn tree_of(v: &RuntimeValue) -> Result<BTreeMap<String, String>, String> {
    let RuntimeValue::Map(m) = v else {
        return Err(format!("expected a map of paths: {v:?}"));
    };
    m.iter()
        .map(|(k, v)| Ok((text(k)?.to_owned(), text(v)?.to_owned())))
        .collect()
}

fn tree_value(t: &BTreeMap<String, String>) -> RuntimeValue {
    RuntimeValue::Map(
        t.iter()
            .map(|(k, v)| (RuntimeValue::Text(k.clone()), RuntimeValue::Text(v.clone())))
            .collect(),
    )
}

#[derive(Default)]
struct Client {
    base: Option<Id>,
    delta: BTreeMap<String, String>,
    /// The request it is waiting at, if any.
    held: Option<(Request, oneshot::Sender<Delivery>)>,
    /// Its last swap went unanswered, so its next ref read is a recheck.
    rechecking: bool,
    /// Ref reads that began an attempt.
    attempts: i64,
    /// The ref as its last attempt read it: commit and version.
    seen: Option<(Id, i64)>,
    outcome: Option<&'static str>,
}

/// Real clients over one bucket, and what the scenario has seen of them.
struct Harness {
    rt: Option<tokio::runtime::Runtime>,
    bucket: Arc<InMemory>,
    /// Reads the bucket directly, past every gate.
    observer: Repo,
    events: mpsc::Receiver<Event>,
    sender: mpsc::Sender<Event>,
    clients: BTreeMap<String, Client>,
    names: HashMap<Hash, Id>,
    hashes: HashMap<Id, Hash>,
    versions: Mutex<Versions>,
    /// The commit each version of the ref named, oldest first.
    ref_commits: Mutex<Vec<Hash>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Clients still held (one that went on to combine) are abandoned, not released.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

impl Harness {
    fn rt(&self) -> &tokio::runtime::Runtime {
        self.rt.as_ref().expect("runtime")
    }

    fn files(&self, tree: &BTreeMap<String, String>) -> Result<Delta, String> {
        let inputs: Vec<FileInput> = tree
            .iter()
            .map(|(path, data)| FileInput {
                path: path.clone(),
                kind: Kind::File,
                data: Bytes::from(data.clone().into_bytes()),
            })
            .collect();
        let entries = self
            .rt()
            .block_on(self.observer.write_files(&inputs, &HashMap::new()))
            .map_err(|e| e.to_string())?;
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

    fn client(&mut self, name: &str) -> Result<&mut Client, String> {
        self.clients
            .get_mut(name)
            .ok_or_else(|| format!("no client {name}"))
    }

    /// The ref's commit and version, recording a version not seen before.
    fn read_ref(&self) -> Result<(Hash, i64), String> {
        let key = format!("refs/{BRANCH}");
        let (data, etag) = self.object(&key)?.ok_or("the branch ref is gone")?;
        let value: RefValue = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        let (version, new) = self.versions.lock().unwrap().of(&key, &etag);
        if new {
            self.ref_commits.lock().unwrap().push(value.commit);
        }
        Ok((value.commit, version))
    }

    fn name(&self, h: &Hash) -> Result<Id, String> {
        self.names
            .get(h)
            .cloned()
            .ok_or_else(|| format!("commit {} has no writer the scenario knows", h.short()))
    }

    fn handle(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::Held {
                client,
                request,
                go,
            } => {
                let c = self.client(&client)?;
                if let Some((was, _)) = &c.held {
                    return Err(format!(
                        "{client} made a {request:?} request while one {was:?} was held"
                    ));
                }
                if request == Request::ReadRef && !c.rechecking {
                    c.attempts += 1;
                }
                c.held = Some((request, go));
            }
            Event::Answered {
                client,
                request,
                key,
                etag,
            } => match request {
                Request::ReadRef => {
                    let seen = match etag {
                        Some(etag) => {
                            self.read_ref()?;
                            let version = self
                                .versions
                                .lock()
                                .unwrap()
                                .known(&key, &etag)
                                .ok_or("a client read a ref version never stored")?;
                            let commit = self.ref_commits.lock().unwrap()[version as usize - 1];
                            Some((self.name(&commit)?, version))
                        }
                        None => None,
                    };
                    let c = self.client(&client)?;
                    if c.rechecking {
                        c.rechecking = false;
                    } else {
                        c.seen = seen;
                    }
                }
                Request::PutCommit => {
                    let h = Hash::from_hex(key.trim_start_matches("commits/"))
                        .map_err(|e| e.to_string())?;
                    if !self.names.contains_key(&h) {
                        let generation = self
                            .rt()
                            .block_on(self.observer.commit_obj(&h))
                            .map_err(|e| e.to_string())?
                            .generation as i64;
                        self.names.insert(h, (client.clone(), generation));
                        self.hashes.insert((client, generation), h);
                    }
                }
                Request::Swap | Request::PutIntent => {}
            },
            Event::Finished { client, outcome } => {
                self.client(&client)?.outcome = Some(outcome);
            }
        }
        Ok(())
    }

    /// Process requests until `client` waits at its next one or has returned.
    fn settle(&mut self, client: &str) -> Result<(), String> {
        loop {
            let c = self.client(client)?;
            if c.held.is_some() || c.outcome.is_some() {
                return Ok(());
            }
            let event = self
                .events
                .recv_timeout(SETTLE)
                .map_err(|_| format!("{client} made no request within {SETTLE:?}"))?;
            self.handle(event)?;
        }
    }

    /// Let `client`'s held request through, which the model says is `expected`.
    fn release(&mut self, client: &str, expected: Request, how: Delivery) -> Result<(), String> {
        let c = self.client(client)?;
        let Some((request, go)) = c.held.take() else {
            return Err(format!(
                "the model's next step for {client} is {expected:?}, but it is making no request \
                 (outcome {:?})",
                c.outcome
            ));
        };
        if request != expected {
            return Err(format!(
                "the model's next step for {client} is {expected:?}, but its next request is \
                 {request:?}"
            ));
        }
        if !matches!(how, Delivery::Answered) {
            c.rechecking = true;
        }
        go.send(how)
            .map_err(|_| format!("{client} stopped waiting"))?;
        self.settle(client)
    }

    fn client_arg(arguments: &[RuntimeValue]) -> Result<String, String> {
        Ok(text(arguments.first().ok_or("missing client argument")?)?.to_owned())
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

/// The store layer: each object's version as `store.qnt` numbers them (1 for its first
/// write, one more per write), from the ETags the bucket issued.
#[derive(Default)]
struct Versions {
    etags: HashMap<String, Vec<String>>,
}

impl Versions {
    /// The version `etag` is of `key`, and whether it is one not seen before.
    fn of(&mut self, key: &str, etag: &str) -> (i64, bool) {
        let seen = self.etags.entry(key.to_owned()).or_default();
        match seen.iter().position(|e| e == etag) {
            Some(i) => (i as i64 + 1, false),
            None => {
                seen.push(etag.to_owned());
                (seen.len() as i64, true)
            }
        }
    }

    fn known(&self, key: &str, etag: &str) -> Option<i64> {
        let i = self.etags.get(key)?.iter().position(|e| e == etag)?;
        Some(i as i64 + 1)
    }
}

/// A `Versioned` object of `store.qnt`.
fn versioned(body: RuntimeValue, version: i64) -> RuntimeValue {
    RuntimeValue::Record(BTreeMap::from([
        ("body".to_owned(), body),
        ("version".to_owned(), RuntimeValue::Int(version)),
        ("live".to_owned(), RuntimeValue::Bool(true)),
    ]))
}

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

    /// Store layer: every object under `prefix` with its key (prefix removed), bytes and
    /// version. pail deletes nothing in these scenarios, so an object seen before must
    /// still be there.
    fn space(&self, prefix: &str) -> Result<Vec<(String, Bytes, i64)>, String> {
        let listed = self
            .rt()
            .block_on(self.observer.store.list(prefix))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for (key, _) in &listed {
            let (data, etag) = self.object(key)?.ok_or(format!("{key} vanished"))?;
            let (version, _) = self.versions.lock().unwrap().of(key, &etag);
            out.push((key.trim_start_matches(prefix).to_owned(), data, version));
        }
        let versions = self.versions.lock().unwrap();
        if let Some(gone) = versions
            .etags
            .keys()
            .filter(|k| k.starts_with(prefix))
            .find(|k| !listed.iter().any(|(l, _)| l == *k))
        {
            return Err(format!("{gone} was deleted; the model has no deletes yet"));
        }
        Ok(out)
    }

    /// Pail layer: a commit object as the model's `Commit` (its tree read back in full,
    /// its ancestors named).
    fn commit_value(&self, h: &Hash) -> Result<RuntimeValue, String> {
        let rt = self.rt();
        let c = rt
            .block_on(self.observer.commit_obj(h))
            .map_err(|e| e.to_string())?;
        let entries = rt
            .block_on(self.observer.entries(c.root))
            .map_err(|e| e.to_string())?;
        let data = rt
            .block_on(self.observer.read_files(&entries))
            .map_err(|e| e.to_string())?;
        let tree: BTreeMap<String, String> = entries
            .iter()
            .zip(data)
            .map(|(e, d)| (e.path.clone(), String::from_utf8_lossy(&d).into_owned()))
            .collect();
        let mut ancestors = BTreeSet::new();
        let mut parent = c.parent;
        while let Some(p) = parent {
            ancestors.insert(id_value(&self.name(&p)?));
            parent = rt
                .block_on(self.observer.commit_obj(&p))
                .map_err(|e| e.to_string())?
                .parent;
        }
        Ok(RuntimeValue::Record(BTreeMap::from([
            ("tree".to_owned(), tree_value(&tree)),
            ("ancestors".to_owned(), RuntimeValue::Set(ancestors)),
        ])))
    }

    /// Pail layer: the bucket as `bucket.qnt` lays it out.
    fn bucket_value(&self) -> Result<RuntimeValue, String> {
        let mut commits = BTreeMap::new();
        for (hex, _, version) in self.space("commits/")? {
            let h = Hash::from_hex(&hex).map_err(|e| e.to_string())?;
            commits.insert(
                id_value(&self.name(&h)?),
                versioned(self.commit_value(&h)?, version),
            );
        }
        self.read_ref()?;
        let mut refs = BTreeMap::new();
        for (branch, data, version) in self.space("refs/")? {
            let value: RefValue = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
            let body = RuntimeValue::Record(BTreeMap::from([(
                "commit".to_owned(),
                id_value(&self.name(&value.commit)?),
            )]));
            refs.insert(RuntimeValue::Text(branch), versioned(body, version));
        }
        Ok(RuntimeValue::Record(BTreeMap::from([
            ("commits".to_owned(), RuntimeValue::Map(commits)),
            ("refs".to_owned(), RuntimeValue::Map(refs)),
        ])))
    }

    /// The whole model state: the bucket, each client's progress as read off its
    /// requests, and the ref's history.
    fn state(&self) -> Result<RuntimeValue, String> {
        let bucket = self.bucket_value()?;
        let history = self
            .ref_commits
            .lock()
            .unwrap()
            .iter()
            .map(|h| self.name(h).map(|id| id_value(&id)))
            .collect::<Result<Vec<_>, _>>()?;

        let root: Id = ("root".to_owned(), 0);
        let mut clients = BTreeMap::new();
        for (name, c) in &self.clients {
            let phase = match (&c.outcome, &c.held, c.base.is_some()) {
                (_, _, false) => "Idle",
                (_, Some((Request::PutIntent, _)), _) | (Some(_), _, _) => "Done",
                (None, Some((Request::ReadRef, _)), _) if c.rechecking => "Rechecking",
                (None, Some((Request::ReadRef, _)), _) => "Reading",
                (None, Some((Request::PutCommit, _)), _) => "Uploading",
                (None, Some((Request::Swap, _)), _) => "Swapping",
                (None, None, true) => return Err(format!("{name} is between requests")),
            };
            let outcome = match (&c.outcome, &c.held) {
                (Some(o), _) => *o,
                (None, Some((Request::PutIntent, _))) => "Combining",
                (None, _) => "Pending",
            };
            let base = c.base.clone().unwrap_or_else(|| root.clone());
            let (seen, seen_version) = c.seen.clone().unwrap_or((base.clone(), 0));
            clients.insert(
                RuntimeValue::Text(name.clone()),
                RuntimeValue::Record(BTreeMap::from([
                    ("base".to_owned(), id_value(&base)),
                    ("delta".to_owned(), tree_value(&c.delta)),
                    ("phase".to_owned(), RuntimeValue::Text(phase.to_owned())),
                    (
                        "seen".to_owned(),
                        RuntimeValue::Record(BTreeMap::from([
                            ("commit".to_owned(), id_value(&seen)),
                            ("version".to_owned(), RuntimeValue::Int(seen_version)),
                        ])),
                    ),
                    ("attempt".to_owned(), RuntimeValue::Int(c.attempts.max(1))),
                    ("outcome".to_owned(), RuntimeValue::Text(outcome.to_owned())),
                ])),
            );
        }
        Ok(RuntimeValue::Record(BTreeMap::from([
            ("bucket".to_owned(), bucket),
            ("clients".to_owned(), RuntimeValue::Map(clients)),
            ("history".to_owned(), RuntimeValue::List(history)),
        ])))
    }
}

impl generated::Implementation for Harness {
    type Evidence = Snapshot;

    /// A bucket holding the model's root commit, and an idle client per model client.
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
        let clients = clients
            .keys()
            .map(|k| Ok((text(k)?.to_owned(), Client::default())))
            .collect::<Result<_, String>>()?;
        let mut h = Harness {
            rt: Some(rt),
            bucket,
            observer,
            events,
            sender,
            clients,
            names: HashMap::new(),
            hashes: HashMap::new(),
            versions: Mutex::new(Versions::default()),
            ref_commits: Mutex::new(Vec::new()),
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
        let req = h.request(&root.0, None, delta);
        let made = h
            .rt()
            .block_on(h.observer.commit(req))
            .map_err(|e| e.to_string())?;
        h.names.insert(made.commit, root.clone());
        h.hashes.insert(root, made.commit);
        h.read_ref()?;
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
        let gate = Gate {
            inner: self.bucket.clone(),
            client: name.clone(),
            events: self.sender.clone(),
        };
        let repo = Repo::with_store(
            Store::from_object_store("memory://bucket", Arc::new(gate)),
            None,
        );
        let events = self.sender.clone();
        let client = name.clone();
        self.rt().spawn(async move {
            let outcome = match repo.commit(req).await {
                Ok(_) => "Published",
                Err(e) if e.downcast_ref::<ConflictError>().is_some() => "Conflicted",
                Err(_) => "Unknown",
            };
            let _ = events.send(Event::Finished { client, outcome });
        });
        let c = self.client(&name)?;
        c.base = Some(base);
        c.delta = delta;
        self.settle(&name)
    }

    fn read_ref(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::client_arg(arguments)?;
        self.release(&c, Request::ReadRef, Delivery::Answered)
    }

    fn recheck(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::client_arg(arguments)?;
        if !self.client(&c)?.rechecking {
            return Err(format!("{c} has no unanswered swap to recheck"));
        }
        self.release(&c, Request::ReadRef, Delivery::Answered)
    }

    fn swap(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::client_arg(arguments)?;
        let how = Delivery::of(arguments.get(1).ok_or("swap takes a Delivery")?)?;
        self.release(&c, Request::Swap, how)
    }

    fn upload(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = Harness::client_arg(arguments)?;
        self.release(&c, Request::PutCommit, Delivery::Answered)
    }
}
