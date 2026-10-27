//! Every pail store backend checked against the Quint model of a store
//! (`verify/store_spec.qnt`, scenarios in `verify/store_scenarios.qnt`).
//!
//! Each model action is one `Store` method call. After each, the objects in the store and
//! the method's reply must equal the model's: the same bodies, the same versions (ETags or
//! generations, numbered per key in the order the store issued them) and the same answer.
//! `PAIL_TEST_STORES` adds real S3/GCS (or emulator) URLs, as `tests/backends.rs` does.

#[path = "store_refinement/generated.rs"]
mod generated;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use bytes::Bytes;
use object_store::memory::InMemory;
use pail::store::{Cas, Store, Version};
use quint_refinements::{NormalizedRuntimeEvidence, RuntimeValue};

/// The prefix every scenario's keys live under (`o/...`).
const PREFIX: &str = "o/";

#[derive(Clone, Debug)]
enum Backend {
    Memory,
    File,
    Url(String),
}

thread_local! {
    /// The backend the scenarios on this thread run against.
    static BACKEND: RefCell<Backend> = const { RefCell::new(Backend::Memory) };
}

fn refine(backend: Backend) {
    let name = format!("{backend:?}");
    BACKEND.with(|b| *b.borrow_mut() = backend);
    let results =
        generated::refine_all::<StoreUnderTest>().unwrap_or_else(|e| panic!("{name}: {e}"));
    for r in &results {
        eprintln!(
            "{name}: {} refined {} obligations",
            r.scenario, r.evaluated_obligations
        );
    }
    assert_eq!(
        results.len(),
        6,
        "every scenario in verify/store_scenarios.qnt ran"
    );
}

#[test]
fn memory_store_refines_the_store_model() {
    refine(Backend::Memory);
}

#[test]
fn file_store_refines_the_store_model() {
    refine(Backend::File);
}

#[test]
fn configured_stores_refine_the_store_model() {
    let Ok(urls) = std::env::var("PAIL_TEST_STORES") else {
        return;
    };
    for url in urls.split_whitespace() {
        refine(Backend::Url(url.to_owned()));
    }
}

// ---------- the refinement map: real store to model state ----------

/// The versions each key has had, oldest first: `None` for a delete, which the model
/// counts as a version (its tombstone) though the store issues none.
#[derive(Default)]
struct Versions {
    seen: HashMap<String, Vec<Option<Version>>>,
}

impl Versions {
    /// The model version of `v`, recording it if it is new.
    fn of(&mut self, key: &str, v: &Version) -> i64 {
        let seen = self.seen.entry(key.to_owned()).or_default();
        let found = seen.iter().position(|s| s.as_ref() == Some(v));
        match found {
            Some(i) => i as i64 + 1,
            None => {
                seen.push(Some(v.clone()));
                seen.len() as i64
            }
        }
    }

    fn known(&self, key: &str, v: &Version) -> Option<i64> {
        let i = self
            .seen
            .get(key)?
            .iter()
            .position(|s| s.as_ref() == Some(v))?;
        Some(i as i64 + 1)
    }

    /// The real version the model calls `version` of `key`.
    fn real(&self, key: &str, version: i64) -> Option<Version> {
        let i = usize::try_from(version).ok()?.checked_sub(1)?;
        self.seen.get(key)?.get(i).cloned().flatten()
    }

    /// The object was deleted: its tombstone takes the next version.
    fn deleted(&mut self, key: &str) {
        self.seen.entry(key.to_owned()).or_default().push(None);
    }
}

/// An object as `store.qnt`'s `Versioned` of a string body.
fn versioned(body: &str, version: i64, live: bool) -> RuntimeValue {
    RuntimeValue::Record(BTreeMap::from([
        ("body".to_owned(), RuntimeValue::Text(body.to_owned())),
        ("version".to_owned(), RuntimeValue::Int(version)),
        ("live".to_owned(), RuntimeValue::Bool(live)),
    ]))
}

fn variant(tag: &str, value: RuntimeValue) -> RuntimeValue {
    RuntimeValue::Variant {
        tag: tag.to_owned(),
        value: Box::new(value),
    }
}

/// A method's reply, before its versions are numbered.
enum Reply {
    None,
    Done,
    Created(bool),
    Swapped(Version),
    Conflict,
    Found(String, Version),
    Missing,
    Listed(BTreeSet<String>),
}

struct StoreUnderTest {
    rt: tokio::runtime::Runtime,
    store: Store,
    /// Keeps a `file://` store's directory alive for the scenario.
    _dir: Option<tempfile::TempDir>,
    versions: RefCell<Versions>,
    /// The last body each key had, for its tombstone after a delete.
    bodies: RefCell<BTreeMap<String, String>>,
    reply: Reply,
    /// The key the last call was about.
    key: String,
}

impl StoreUnderTest {
    fn open(backend: &Backend) -> Result<(Store, Option<tempfile::TempDir>), String> {
        Ok(match backend {
            Backend::Memory => (
                Store::from_object_store("memory://store", Arc::new(InMemory::new())),
                None,
            ),
            Backend::File => {
                let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
                let url = format!("file://{}", dir.path().join("bucket").display());
                (Store::open(&url).map_err(|e| e.to_string())?, Some(dir))
            }
            Backend::Url(url) => {
                let run = format!("{url}/refine-{:016x}", rand::random::<u64>());
                (Store::open(&run).map_err(|e| e.to_string())?, None)
            }
        })
    }

    fn call<T>(
        &self,
        f: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> Result<T, String> {
        self.rt.block_on(f).map_err(|e| format!("{e:#}"))
    }

    /// Every object under the prefix, read back, as the model's `objects`; a key seen
    /// before and now gone is a tombstone.
    fn objects(&self) -> Result<RuntimeValue, String> {
        let listed = self.call(self.store.list(PREFIX))?;
        let mut versions = self.versions.borrow_mut();
        let mut bodies = self.bodies.borrow_mut();
        let mut out = BTreeMap::new();
        for (key, _) in &listed {
            let (data, v) = self
                .call(self.store.get_opt(key))?
                .ok_or(format!("{key} listed but missing"))?;
            let body = String::from_utf8_lossy(&data).into_owned();
            let version = versions.of(key, &v);
            bodies.insert(key.clone(), body.clone());
            out.insert(
                RuntimeValue::Text(key.clone()),
                versioned(&body, version, true),
            );
        }
        for (key, body) in bodies.iter() {
            if listed.iter().any(|(k, _)| k == key) {
                continue;
            }
            let seen = &versions.seen[key];
            out.insert(
                RuntimeValue::Text(key.clone()),
                versioned(body, seen.len() as i64, false),
            );
        }
        Ok(RuntimeValue::Map(out))
    }

    fn reply_value(&self) -> Result<RuntimeValue, String> {
        let versions = self.versions.borrow();
        let number = |v: &Version| {
            versions.known(&self.key, v).ok_or(format!(
                "{} returned a version it never stored: {v:?}",
                self.key
            ))
        };
        Ok(match &self.reply {
            Reply::None => RuntimeValue::Text("NoReply".into()),
            Reply::Done => RuntimeValue::Text("Done".into()),
            Reply::Conflict => RuntimeValue::Text("Conflict".into()),
            Reply::Missing => RuntimeValue::Text("Missing".into()),
            Reply::Created(b) => variant("Created", RuntimeValue::Bool(*b)),
            Reply::Swapped(v) => variant("Swapped", RuntimeValue::Int(number(v)?)),
            Reply::Found(body, v) => variant(
                "Found",
                RuntimeValue::Record(BTreeMap::from([
                    ("body".to_owned(), RuntimeValue::Text(body.clone())),
                    ("version".to_owned(), RuntimeValue::Int(number(v)?)),
                ])),
            ),
            Reply::Listed(keys) => variant(
                "Listed",
                RuntimeValue::Set(keys.iter().cloned().map(RuntimeValue::Text).collect()),
            ),
        })
    }

    fn state(&self) -> Result<RuntimeValue, String> {
        let objects = self.objects()?;
        Ok(RuntimeValue::Record(BTreeMap::from([
            ("objects".to_owned(), objects),
            ("reply".to_owned(), self.reply_value()?),
        ])))
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

fn text(v: Option<&RuntimeValue>) -> Result<String, String> {
    match v {
        Some(RuntimeValue::Text(t)) => Ok(t.clone()),
        other => Err(format!("expected text, got {other:?}")),
    }
}

fn int(v: Option<&RuntimeValue>) -> Result<i64, String> {
    match v {
        Some(RuntimeValue::Int(i)) => Ok(*i),
        other => Err(format!("expected an integer, got {other:?}")),
    }
}

impl generated::Implementation for StoreUnderTest {
    type Evidence = Snapshot;

    /// An empty store on the thread's backend (the model starts with no objects).
    fn from_initial_state(_initial: &RuntimeValue) -> Result<Self, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let backend = BACKEND.with(|b| b.borrow().clone());
        let (store, dir) = StoreUnderTest::open(&backend)?;
        Ok(StoreUnderTest {
            rt,
            store,
            _dir: dir,
            versions: RefCell::new(Versions::default()),
            bodies: RefCell::new(BTreeMap::new()),
            reply: Reply::None,
            key: String::new(),
        })
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot(self.state().unwrap_or_else(|e| panic!("snapshot: {e}")))
    }

    fn get_opt(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        self.key = text(arguments.first())?;
        self.reply = match self.call(self.store.get_opt(&self.key))? {
            Some((data, v)) => Reply::Found(String::from_utf8_lossy(&data).into_owned(), v),
            None => Reply::Missing,
        };
        Ok(())
    }

    fn put_new(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        self.key = text(arguments.first())?;
        let data = Bytes::from(text(arguments.get(1))?);
        self.reply = Reply::Created(self.call(self.store.put_new(&self.key, data))?);
        Ok(())
    }

    fn put_any(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        self.key = text(arguments.first())?;
        let data = Bytes::from(text(arguments.get(1))?);
        self.call(self.store.put(&self.key, data))?;
        self.reply = Reply::Done;
        Ok(())
    }

    fn cas_new(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        self.key = text(arguments.first())?;
        let data = Bytes::from(text(arguments.get(1))?);
        self.reply = match self.call(self.store.cas(&self.key, data, None))? {
            Cas::Ok(v) => Reply::Swapped(v),
            Cas::Conflict => Reply::Conflict,
        };
        Ok(())
    }

    fn cas(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        self.key = text(arguments.first())?;
        let version = int(arguments.get(1))?;
        let data = Bytes::from(text(arguments.get(2))?);
        let expected = self
            .versions
            .borrow()
            .real(&self.key, version)
            .ok_or(format!(
                "the store never gave {} version {version}",
                self.key
            ))?;
        self.reply = match self.call(self.store.cas(&self.key, data, Some(&expected)))? {
            Cas::Ok(v) => Reply::Swapped(v),
            Cas::Conflict => Reply::Conflict,
        };
        Ok(())
    }

    fn remove(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        self.key = text(arguments.first())?;
        let existed = self.call(self.store.get_opt(&self.key))?.is_some();
        self.call(self.store.delete(&self.key))?;
        if existed {
            self.versions.borrow_mut().deleted(&self.key);
        }
        self.reply = Reply::Done;
        Ok(())
    }

    fn list_keys(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        let listed = self.call(self.store.list(PREFIX))?;
        self.reply = Reply::Listed(listed.into_iter().map(|(k, _)| k).collect());
        Ok(())
    }
}
