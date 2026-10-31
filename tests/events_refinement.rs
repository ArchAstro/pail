//! The real `pail events` watcher (`pail::events::Watcher`), checked against the Quint model
//! of the consumer (`verify/events/events.qnt`, scenarios in `verify/events/events_scenarios.qnt`).
//!
//! The model's keys are `1` and `2`; here a key is a ref `b<k>` (cursor in a file) or an
//! object `k<k>` of the sibling area `<repo>.actions/` (cursor in the bucket, in that area, so
//! that the watcher must also leave its own cursor out of its events). The scenario's world
//! actions (`write`, `remove`, `drop`, `redeliver`) change the real store and the harness's
//! stand-in for the subscription; its consumer actions drive the real watcher's own steps:
//!
//! - `note(n)`, `scan(k)`: `Watcher::observe` re-reads the key (the payload of `n` is never
//!   looked at), and the harness holds the change as pending, or acknowledges `n`;
//! - `start(k)`: `Watcher::scan` re-lists and diffs, as `watch` does at start;
//! - `mark`, `emit`, `save`: `Watcher::mark`, `emit`, `save`, the steps `handle` runs in turn;
//! - `crash`, `restart`: the watcher is dropped, and opened again over the same cursor.
//!
//! After each action the snapshot is the model's state, built from the store, the real cursor
//! (read back from its file or object), the watcher's beliefs and the lines it wrote.

#[path = "events_refinement/generated.rs"]
mod generated;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use pail::events::{Belief, Change, Cursor, CursorData, Event, Spec, Watcher};
use pail::hash::Hash;
use pail::repo::Repo;
use pail::store::Store;
use quint_refinements::{NormalizedRuntimeEvidence, RuntimeValue};

#[test]
fn refs_refine_the_events_model() {
    let results = generated::refine_all::<Under<false>>().unwrap_or_else(|e| panic!("{e}"));
    for r in &results {
        eprintln!(
            "refs: {} refined {} obligations",
            r.scenario, r.evaluated_obligations
        );
    }
    assert_eq!(
        results.len(),
        8,
        "every scenario in verify/events/events_scenarios.qnt ran"
    );
}

#[test]
fn objects_refine_the_events_model() {
    let results = generated::refine_all::<Under<true>>().unwrap_or_else(|e| panic!("{e}"));
    for r in &results {
        eprintln!(
            "objects: {} refined {} obligations",
            r.scenario, r.evaluated_obligations
        );
    }
    assert_eq!(results.len(), 8);
}

// ---------- values ----------

fn int(i: i64) -> RuntimeValue {
    RuntimeValue::Int(i)
}

fn text(s: &str) -> RuntimeValue {
    RuntimeValue::Text(s.to_owned())
}

fn record<const N: usize>(fields: [(&str, RuntimeValue); N]) -> RuntimeValue {
    RuntimeValue::Record(fields.map(|(k, v)| (k.to_owned(), v)).into_iter().collect())
}

fn as_int(v: &RuntimeValue) -> Result<i64, String> {
    match v {
        RuntimeValue::Int(i) => Ok(*i),
        other => Err(format!("expected an integer, got {other:?}")),
    }
}

fn field<'a>(v: &'a RuntimeValue, name: &str) -> Result<&'a RuntimeValue, String> {
    match v {
        RuntimeValue::Record(r) => r.get(name).ok_or(format!("no field {name} in {v:?}")),
        other => Err(format!("expected a record, got {other:?}")),
    }
}

/// The model's `Obs`: `Gone`, `Live(v)`, `Unsure`.
fn obs(v: Option<i64>) -> RuntimeValue {
    match v {
        Some(v) => RuntimeValue::Variant {
            tag: "Live".into(),
            value: Box::new(int(v)),
        },
        None => text("Gone"),
    }
}

fn arg(arguments: &[RuntimeValue], i: usize) -> Result<&RuntimeValue, String> {
    arguments.get(i).ok_or(format!("missing argument {i}"))
}

fn block<T>(f: impl std::future::Future<Output = T>) -> T {
    thread_local! {
        static RT: tokio::runtime::Runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
    }
    RT.with(|rt| rt.block_on(f))
}

// ---------- the world and the watcher ----------

/// What the watcher is in the middle of, as the model's `Pend`.
struct Pending {
    key: i64,
    /// The state read, as a model version (None: gone).
    obs: Option<i64>,
    g: i64,
    note: Option<RuntimeValue>,
    change: Change,
    marked: bool,
    emitted: bool,
    saved: bool,
}

struct Harness {
    objects: bool,
    dir: tempfile::TempDir,
    repo: &'static Repo,
    area: Store,
    spec: Spec,
    watcher: Option<Watcher<'static>>,
    /// The model's `clock`, and each key's last write: (counter, live).
    clock: i64,
    slots: BTreeMap<i64, (i64, bool)>,
    /// A version token the store gave, to the model's version of the write.
    versions: BTreeMap<String, i64>,
    net: BTreeSet<RuntimeValue>,
    sent: BTreeSet<RuntimeValue>,
    start_scan: BTreeSet<i64>,
    pend: Option<Pending>,
    /// The lines written (the watcher's sink), as the model's `Ev`.
    out: Vec<(i64, Option<i64>)>,
}

struct Under<const OBJECTS: bool>(Harness);

const KEYS: [i64; 2] = [1, 2];

impl Harness {
    fn new(objects: bool) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://{}", dir.path().join("repo").display());
        let repo: &'static Repo =
            Box::leak(Box::new(Repo::with_store(Store::open(&url).unwrap(), None)));
        let area = Store::open(&format!("{url}.actions")).unwrap();
        let spec = if objects {
            Spec {
                globs: vec![],
                refs: false,
                prefix: Some("actions/".into()),
            }
        } else {
            Spec {
                globs: vec![],
                refs: true,
                prefix: None,
            }
        };
        let mut h = Harness {
            objects,
            dir,
            repo,
            area,
            spec,
            watcher: None,
            clock: 0,
            slots: KEYS.iter().map(|k| (*k, (0, false))).collect(),
            versions: BTreeMap::new(),
            net: BTreeSet::new(),
            sent: BTreeSet::new(),
            start_scan: BTreeSet::new(),
            pend: None,
            out: vec![],
        };
        h.open();
        h
    }

    fn cursor(&self) -> Cursor {
        if self.objects {
            Cursor::Bucket {
                store: self.area.clone(),
                key: "cursor.json".into(),
                version: None,
            }
        } else {
            Cursor::File(self.cursor_path())
        }
    }

    fn cursor_path(&self) -> PathBuf {
        self.dir.path().join("cursor.json")
    }

    /// Start a watcher over the cursor (whatever it holds).
    fn open(&mut self) {
        let w = block(Watcher::open(self.repo, &self.spec, Some(self.cursor()))).unwrap();
        self.watcher = Some(w);
    }

    /// The watcher's key for the model's.
    fn name(&self, k: i64) -> String {
        if self.objects {
            format!("obj:k{k}")
        } else {
            format!("ref:b{k}")
        }
    }

    fn key_of(&self, name: &str) -> i64 {
        name.trim_start_matches("obj:k")
            .trim_start_matches("ref:b")
            .parse()
            .unwrap()
    }

    fn version_of(&self, token: &str) -> i64 {
        *self
            .versions
            .get(token)
            .unwrap_or_else(|| panic!("a version the harness never wrote: {token}"))
    }

    fn model_ver(&self, token: Option<&str>) -> Option<i64> {
        token.map(|t| self.version_of(t))
    }

    fn note(&self, k: i64, v: Option<i64>, g: i64) -> RuntimeValue {
        record([("key", int(k)), ("obs", obs(v)), ("g", int(g))])
    }

    // ----- the world -----

    fn write(&mut self, k: i64) -> Result<(), String> {
        self.clock += 1;
        let v = self.clock;
        let token = if self.objects {
            block(self.area.put(&format!("k{k}"), format!("{k}/{v}").into()))
                .map_err(|e| e.to_string())?;
            block(self.area.object_version(&format!("k{k}"))).map_err(|e| e.to_string())?
        } else {
            let commit = Hash::of(format!("{k}/{v}").as_bytes());
            block(self.repo.store.put(
                &format!("refs/b{k}"),
                format!(r#"{{"commit":"{}"}}"#, commit.hex()).into(),
            ))
            .map_err(|e| e.to_string())?;
            Some(commit.hex())
        }
        .ok_or("the write is not there")?;
        self.versions.insert(token, v);
        self.slots.insert(k, (v, true));
        let n = self.note(k, Some(v), v);
        self.net.insert(n.clone());
        self.sent.insert(n);
        Ok(())
    }

    fn remove(&mut self, k: i64) -> Result<(), String> {
        self.clock += 1;
        let v = self.clock;
        let store = if self.objects {
            &self.area
        } else {
            &self.repo.store
        };
        let key = if self.objects {
            format!("k{k}")
        } else {
            format!("refs/b{k}")
        };
        block(store.delete(&key)).map_err(|e| e.to_string())?;
        self.slots.insert(k, (v, false));
        let n = self.note(k, None, v);
        self.net.insert(n.clone());
        self.sent.insert(n);
        Ok(())
    }

    // ----- the consumer -----

    fn watcher(&mut self) -> Result<&mut Watcher<'static>, String> {
        self.watcher.as_mut().ok_or("the consumer is down".into())
    }

    /// A read of key `k` that found `change` (or nothing), answering `note`.
    fn read(&mut self, k: i64, change: Option<Change>, note: Option<RuntimeValue>) {
        match change {
            Some(change) => {
                let obs = self.model_ver(change.new.as_deref());
                let g = self.slots[&k].0;
                self.pend = Some(Pending {
                    key: k,
                    obs,
                    g,
                    note,
                    change,
                    marked: false,
                    emitted: false,
                    saved: false,
                });
            }
            None => {
                if let Some(n) = note {
                    self.net.remove(&n);
                }
                self.start_scan.remove(&k);
            }
        }
    }

    fn observe(&mut self, k: i64, note: Option<RuntimeValue>) -> Result<(), String> {
        let name = self.name(k);
        let change = {
            let w = self.watcher.as_ref().ok_or("the consumer is down")?;
            block(w.observe(&name)).map_err(|e| e.to_string())?
        };
        self.read(k, change, note);
        Ok(())
    }

    /// The start-up diff: `Watcher::scan`, the change it finds for `k`.
    fn start(&mut self, k: i64) -> Result<(), String> {
        let name = self.name(k);
        let change = {
            let w = self.watcher.as_ref().ok_or("the consumer is down")?;
            block(w.scan())
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|c| c.key == name)
        };
        self.read(k, change, None);
        Ok(())
    }

    fn pending(&mut self) -> Result<&mut Pending, String> {
        self.pend.as_mut().ok_or("nothing is pending".into())
    }

    fn mark(&mut self) -> Result<(), String> {
        let change = self.pending()?.change.clone();
        let w = self.watcher.as_mut().ok_or("the consumer is down")?;
        block(w.mark(std::slice::from_ref(&change))).map_err(|e| e.to_string())?;
        self.pending()?.marked = true;
        Ok(())
    }

    fn emit(&mut self) -> Result<(), String> {
        let change = self.pending()?.change.clone();
        let mut lines = vec![];
        self.watcher()?
            .emit(&change, "test", &mut |e: Event| {
                lines.push(e);
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        for e in lines {
            let (name, token) = match e {
                Event::Ref(r) => (format!("ref:{}", r.branch), r.commit.map(|c| c.hex())),
                Event::Object(o) => (format!("obj:{}", o.key), o.version),
            };
            let k = self.key_of(&name);
            let v = self.model_ver(token.as_deref());
            self.out.push((k, v));
        }
        self.pending()?.emitted = true;
        Ok(())
    }

    fn save(&mut self) -> Result<(), String> {
        let change = self.pending()?.change.clone();
        let w = self.watcher.as_mut().ok_or("the consumer is down")?;
        block(w.save(std::slice::from_ref(&change))).map_err(|e| e.to_string())?;
        self.pending()?.saved = true;
        Ok(())
    }

    fn finish(&mut self) -> Result<(), String> {
        let p = self.pend.take().ok_or("nothing is pending")?;
        if let Some(n) = p.note {
            self.net.remove(&n);
        }
        self.start_scan.remove(&p.key);
        Ok(())
    }

    // ----- the snapshot -----

    /// The real cursor, read back.
    fn cursor_data(&self) -> CursorData {
        let mut c = self.cursor();
        block(c.load()).unwrap()
    }

    fn state(&self) -> RuntimeValue {
        let store: BTreeMap<RuntimeValue, RuntimeValue> = KEYS
            .iter()
            .map(|k| {
                let (ver, live) = self.slots[k];
                (
                    int(*k),
                    record([("ver", int(ver)), ("live", RuntimeValue::Bool(live))]),
                )
            })
            .collect();
        let cursor = self.cursor_data();
        let cur: BTreeMap<RuntimeValue, RuntimeValue> = KEYS
            .iter()
            .map(|k| {
                let v = cursor.heads.get(&self.name(*k)).map(|t| self.version_of(t));
                (int(*k), obs(v))
            })
            .collect();
        let dirty: BTreeSet<RuntimeValue> =
            cursor.dirty.iter().map(|n| int(self.key_of(n))).collect();
        let seen: BTreeMap<RuntimeValue, RuntimeValue> = KEYS
            .iter()
            .map(|k| {
                let v = match self
                    .watcher
                    .as_ref()
                    .and_then(|w| w.seen.get(&self.name(*k)))
                {
                    Some(Belief::Is(t)) => obs(Some(self.version_of(t))),
                    Some(Belief::Unsure) => text("Unsure"),
                    None => obs(None),
                };
                (int(*k), v)
            })
            .collect();
        let pend = match &self.pend {
            None => text("NoPend"),
            Some(p) => RuntimeValue::Variant {
                tag: "Pend".into(),
                value: Box::new(record([
                    ("key", int(p.key)),
                    ("obs", obs(p.obs)),
                    ("g", int(p.g)),
                    ("note", RuntimeValue::Set(p.note.iter().cloned().collect())),
                    ("marked", RuntimeValue::Bool(p.marked)),
                    ("emitted", RuntimeValue::Bool(p.emitted)),
                    ("saved", RuntimeValue::Bool(p.saved)),
                ])),
            },
        };
        record([
            ("store", RuntimeValue::Map(store)),
            ("clock", int(self.clock)),
            ("net", RuntimeValue::Set(self.net.clone())),
            ("sent", RuntimeValue::Set(self.sent.clone())),
            ("up", RuntimeValue::Bool(self.watcher.is_some())),
            ("seen", RuntimeValue::Map(seen)),
            (
                "startScan",
                RuntimeValue::Set(self.start_scan.iter().map(|k| int(*k)).collect()),
            ),
            ("pend", pend),
            ("cursor", RuntimeValue::Map(cur)),
            ("dirty", RuntimeValue::Set(dirty)),
            (
                "out",
                RuntimeValue::List(
                    self.out
                        .iter()
                        .map(|(k, v)| record([("key", int(*k)), ("obs", obs(*v))]))
                        .collect(),
                ),
            ),
        ])
    }
}

// ---------- evidence ----------

#[derive(Clone)]
struct Snapshot(RuntimeValue);

impl NormalizedRuntimeEvidence for Snapshot {
    fn resolve_name(&self, name: &str) -> Result<RuntimeValue, String> {
        match name {
            "state" => Ok(self.0.clone()),
            other => Err(format!("unknown model name {other}")),
        }
    }

    /// The scenarios' helpers (`events_scenarios.qnt`): a notification and a line.
    fn resolve_call(
        &self,
        operator: &str,
        arguments: &[RuntimeValue],
    ) -> Option<Result<RuntimeValue, String>> {
        let ints = || -> Result<Vec<i64>, String> { arguments.iter().map(as_int).collect() };
        Some(ints().and_then(|a| match (operator, a.as_slice()) {
            ("wrote", [k, v]) => Ok(record([
                ("key", int(*k)),
                ("obs", obs(Some(*v))),
                ("g", int(*v)),
            ])),
            ("removed", [k, v]) => Ok(record([
                ("key", int(*k)),
                ("obs", obs(None)),
                ("g", int(*v)),
            ])),
            ("line", [k, v]) => Ok(record([("key", int(*k)), ("obs", obs(Some(*v)))])),
            ("goneLine", [k]) => Ok(record([("key", int(*k)), ("obs", obs(None))])),
            _ => Err(format!("unknown model operator {operator}")),
        }))
    }
}

impl<const OBJECTS: bool> generated::Implementation for Under<OBJECTS> {
    type Evidence = Snapshot;

    fn from_initial_state(_initial: &RuntimeValue) -> Result<Self, String> {
        Ok(Under(Harness::new(OBJECTS)))
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot(self.0.state())
    }

    fn write(&mut self, a: &[RuntimeValue]) -> Result<(), String> {
        self.0.write(as_int(arg(a, 0)?)?)
    }

    fn remove(&mut self, a: &[RuntimeValue]) -> Result<(), String> {
        self.0.remove(as_int(arg(a, 0)?)?)
    }

    fn drop(&mut self, a: &[RuntimeValue]) -> Result<(), String> {
        let n = arg(a, 0)?;
        self.0.net.remove(n);
        Ok(())
    }

    fn redeliver(&mut self, a: &[RuntimeValue]) -> Result<(), String> {
        self.0.net.insert(arg(a, 0)?.clone());
        Ok(())
    }

    /// The notification names a key; its payload is not read.
    fn note(&mut self, a: &[RuntimeValue]) -> Result<(), String> {
        let n = arg(a, 0)?.clone();
        let k = as_int(field(&n, "key")?)?;
        self.0.observe(k, Some(n))
    }

    fn scan(&mut self, a: &[RuntimeValue]) -> Result<(), String> {
        self.0.observe(as_int(arg(a, 0)?)?, None)
    }

    fn start(&mut self, a: &[RuntimeValue]) -> Result<(), String> {
        self.0.start(as_int(arg(a, 0)?)?)
    }

    fn mark(&mut self, _a: &[RuntimeValue]) -> Result<(), String> {
        self.0.mark()
    }

    fn emit(&mut self, _a: &[RuntimeValue]) -> Result<(), String> {
        self.0.emit()
    }

    fn save(&mut self, _a: &[RuntimeValue]) -> Result<(), String> {
        self.0.save()
    }

    fn finish(&mut self, _a: &[RuntimeValue]) -> Result<(), String> {
        self.0.finish()
    }

    fn crash(&mut self, _a: &[RuntimeValue]) -> Result<(), String> {
        self.0.watcher = None;
        self.0.pend = None;
        self.0.start_scan.clear();
        Ok(())
    }

    fn restart(&mut self, _a: &[RuntimeValue]) -> Result<(), String> {
        self.0.open();
        self.0.start_scan = KEYS.iter().copied().collect();
        Ok(())
    }
}
