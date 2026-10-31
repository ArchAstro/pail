//! Events: what extensions (PR bots, CI, the Actions orchestrator, mirrors) react to.
//!
//! A pail repository has no server to send webhooks, and needs none: every publish is a
//! write to `refs/<branch>`, and object stores already announce writes. `pail events` turns
//! those announcements into one JSON line per change, of two kinds:
//!
//! ```text
//! {"branch":"pr/12","commit":"9f…","old":"41…","source":"gcs","at":1792553000123}
//! {"key":"queue/ubuntu/4182.json","version":"1792…","old":null,"op":"put","source":"gcs","at":1792553000456}
//! ```
//!
//! The first is a ref (`commit` null: deleted). The second is an object under a prefix of
//! the repository's sibling area `<repo>.<area>/` (`--prefix actions/queue/` watches
//! `<repo>.actions/queue/`; `key` is relative to the area, `version` is the object's
//! generation, `op` is `put` or `delete`).
//!
//! # The design: a notification is a hint
//!
//! Truth is re-reading the key. Everything else follows from that, and is modelled in
//! `verify/events/events.qnt` (the code is refined against it, `tests/events_refinement.rs`):
//!
//! - A **hint** (a Pub/Sub message naming an object) says "look at this key". The consumer
//!   re-reads the key and emits it only if it differs from the last state it emitted. A
//!   duplicated or reordered hint costs a read; its payload is never trusted.
//! - A dropped hint costs latency, never correctness: the **safety poll** re-lists
//!   everything every `--safety-poll` seconds (the poll source is the safety poll alone).
//! - Delivery is **at-least-once**. With `--cursor`, the consumer keeps what it has emitted
//!   in a durable cursor, and on start diffs the store against it (changes and deletions
//!   while it was down are emitted, then it streams). Per key, in this order: the cursor
//!   records the intent to emit (`dirty`), the line is written, the cursor takes the new
//!   state and clears the mark. A crash re-emits, never skips. The mark matters: without
//!   it a key that returned to the state the cursor holds (a ref reset, an object deleted
//!   again) would be taken as already emitted, though its readers saw the line in between.
//! - Consumers should still be idempotent (key work by commit, or by key and version).
//!
//! Without `--cursor` a restart takes the present state as already emitted (changes while
//! the consumer was down are not reported); `--initial` emits every key once at start.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::hash::Hash;
use crate::repo::Repo;
use crate::store::{Cas, Store, Version};

#[derive(Debug, Clone, Serialize)]
pub struct RefEvent {
    pub branch: String,
    /// The branch's commit now; None when it was deleted.
    pub commit: Option<Hash>,
    /// The commit this watcher last emitted for the branch (None: new to it).
    pub old: Option<Hash>,
    pub source: &'static str,
    /// Unix milliseconds when the event was emitted.
    pub at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ObjectEvent {
    /// The object's key, relative to the area (`<repo>.<area>/`).
    pub key: String,
    /// The object's version now (its generation); None when it was deleted.
    pub version: Option<String>,
    /// The version this watcher last emitted for the key (None: new to it).
    pub old: Option<String>,
    pub op: &'static str,
    pub source: &'static str,
    pub at: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Event {
    Ref(RefEvent),
    Object(ObjectEvent),
}

/// A nudge: this key may have changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hint {
    Ref(String),
    Object(String),
}

/// Where hints come from. `pull` waits for the next batch and must be safe to drop while
/// waiting (the watcher also wakes for the safety poll); `ack` confirms the last batch
/// once its keys are emitted and the cursor is saved. An unacknowledged batch comes again.
#[async_trait]
pub trait HintSource: Send {
    fn name(&self) -> &'static str;
    async fn pull(&mut self) -> Result<Vec<Hint>>;
    async fn ack(&mut self) -> Result<()>;
}

pub enum Source {
    /// Re-list every interval: for `file://`, emulators, or stores without notifications.
    Poll { interval: Duration },
    /// The bucket's own Pub/Sub notifications, plus the safety poll.
    GcsPubSub {
        subscription: String,
        token: Arc<crate::creds::Source>,
    },
    /// Any source of hints (tests inject them), plus the safety poll.
    Hints(Box<dyn HintSource>),
}

/// `*` matches any run of characters (including `/`); otherwise literal.
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let mut rest = s;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
    }
    true
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------- the sibling area ----------

/// Split `--prefix`/`--cursor` text `area/rest` into the area name and the rest: `actions/`
/// is the whole of `<repo>.actions/`, `actions/queue/` its `queue/`.
pub fn split_area(p: &str) -> Result<(&str, &str)> {
    let (area, rest) = p.split_once('/').unwrap_or((p, ""));
    if area.is_empty() || area.starts_with('.') || area.contains("..") {
        bail!(
            "`{p}` should start with the sibling area's name, e.g. `actions/` for <repo>.actions/"
        );
    }
    Ok((area, rest))
}

/// The URL of the repository's sibling area: `<store url>.<area>`.
pub fn area_url(store_url: &str, area: &str) -> Result<String> {
    let base = store_url.trim_end_matches('/');
    let ok = match base.split_once("://") {
        Some(("file", rest)) => !rest.is_empty(),
        Some(("gs" | "s3", rest)) => rest.contains('/'),
        _ => false,
    };
    if !ok {
        bail!(
            "a sibling area needs a file://, gs:// or s3:// store with a path, which names the repository (got {store_url})"
        );
    }
    Ok(format!("{base}.{area}"))
}

// ---------- the cursor ----------

/// What a watcher has emitted, durably: key → version, and the keys being emitted (a crash
/// leaves the consumer unsure of those). Keys are `ref:<branch>` or `obj:<key>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CursorData {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub heads: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub dirty: BTreeSet<String>,
}

/// Where the cursor lives, and the version of it last read or written (a bucket cursor is
/// compare-and-swapped: a second consumer on the same cursor makes the first fail loudly
/// instead of overwriting).
pub enum Cursor {
    File(PathBuf),
    Bucket {
        store: Store,
        key: String,
        version: Option<Version>,
    },
}

impl Cursor {
    pub async fn load(&mut self) -> Result<CursorData> {
        match self {
            Cursor::File(path) => match std::fs::read(&path) {
                Ok(b) => serde_json::from_slice(&b)
                    .with_context(|| format!("corrupt cursor {}", path.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CursorData::default()),
                Err(e) => Err(e).with_context(|| format!("reading cursor {}", path.display())),
            },
            Cursor::Bucket {
                store,
                key,
                version,
            } => match store.get_opt(key).await? {
                Some((b, v)) => {
                    *version = Some(v);
                    serde_json::from_slice(&b).with_context(|| format!("corrupt cursor {key}"))
                }
                None => {
                    *version = None;
                    Ok(CursorData::default())
                }
            },
        }
    }

    /// Write the cursor atomically: a reader (or a crash) sees the old cursor or the new,
    /// never half of one.
    pub async fn save(&mut self, data: &CursorData) -> Result<()> {
        let mut data = data.clone();
        data.version = 1;
        let bytes = serde_json::to_vec(&data)?;
        match self {
            Cursor::File(path) => {
                use std::io::Write as _;
                let dir = path
                    .parent()
                    .filter(|d| !d.as_os_str().is_empty())
                    .map(|d| d.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                std::fs::create_dir_all(&dir)?;
                let tmp = path.with_extension(format!("tmp{}", std::process::id()));
                let mut f = std::fs::File::create(&tmp)?;
                f.write_all(&bytes)?;
                f.sync_all()?;
                std::fs::rename(&tmp, &path)?;
                // the rename itself must survive a crash
                if let Ok(d) = std::fs::File::open(&dir) {
                    let _ = d.sync_all();
                }
                Ok(())
            }
            Cursor::Bucket {
                store,
                key,
                version,
            } => match store.cas(key, bytes.into(), version.as_ref()).await? {
                Cas::Ok(v) => {
                    *version = Some(v);
                    Ok(())
                }
                Cas::Conflict => bail!(
                    "the cursor {key} changed under us: another consumer is using it (one consumer per cursor)"
                ),
            },
        }
    }
}

// ---------- the watcher ----------

/// What a watcher believes it has emitted for a key. A key with no entry was emitted gone
/// (or never existed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Belief {
    Is(String),
    /// A crash may have come between the intent and the line: emit what is there now.
    Unsure,
}

/// A key's state changed from what the watcher believes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// `ref:<branch>` or `obj:<key>`.
    pub key: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

struct Area {
    store: Store,
    /// Only keys under this prefix of the area.
    filter: String,
    /// The consumer's own cursor, when it lives in the area: not an event.
    skip: Option<String>,
}

/// What to watch.
pub struct Spec {
    /// Refs matching any glob (all when empty), if `refs`.
    pub globs: Vec<String>,
    pub refs: bool,
    /// Objects under this `area/rest` of the repo's sibling area.
    pub prefix: Option<String>,
}

pub struct Watcher<'a> {
    repo: &'a Repo,
    globs: Vec<String>,
    refs: bool,
    area: Option<Area>,
    pub seen: BTreeMap<String, Belief>,
    saved: CursorData,
    cursor: Option<Cursor>,
}

impl<'a> Watcher<'a> {
    /// A watcher for `spec`, resuming from `cursor` when it has one: it believes it has
    /// emitted what the cursor holds (and nothing of the keys marked dirty).
    pub async fn open(
        repo: &'a Repo,
        spec: &Spec,
        mut cursor: Option<Cursor>,
    ) -> Result<Watcher<'a>> {
        let area = match &spec.prefix {
            Some(p) => {
                let (name, rest) = split_area(p)?;
                if !rest.is_empty() && !rest.ends_with('/') {
                    bail!(
                        "--prefix {p}: a prefix inside an area ends with `/` (a directory of objects)"
                    );
                }
                let url = area_url(&repo.store.url, name)?;
                let store = Store::open_in(&url, repo.local.as_deref())?;
                let skip = match &cursor {
                    Some(Cursor::Bucket { store: s, key, .. }) if s.url == url => Some(key.clone()),
                    _ => None,
                };
                Some(Area {
                    store,
                    filter: rest.to_string(),
                    skip,
                })
            }
            None => None,
        };
        let mut w = Watcher {
            repo,
            globs: spec.globs.clone(),
            refs: spec.refs,
            area,
            seen: BTreeMap::new(),
            saved: CursorData::default(),
            cursor: None,
        };
        if let Some(c) = cursor.as_mut() {
            let data = c.load().await?;
            // heads outside what this run watches are dropped: a cursor belongs to one spec
            w.saved.heads = data
                .heads
                .into_iter()
                .filter(|(k, _)| w.wanted(k))
                .collect();
            w.saved.dirty = data.dirty.into_iter().filter(|k| w.wanted(k)).collect();
            for (k, v) in &w.saved.heads {
                w.seen.insert(k.clone(), Belief::Is(v.clone()));
            }
            for k in &w.saved.dirty {
                w.seen.insert(k.clone(), Belief::Unsure);
            }
        }
        w.cursor = cursor;
        Ok(w)
    }

    pub fn has_cursor(&self) -> bool {
        self.cursor.is_some()
    }

    /// Whether this run watches the key (`ref:<branch>` or `obj:<key>`).
    pub fn wanted(&self, key: &str) -> bool {
        if let Some(b) = key.strip_prefix("ref:") {
            self.refs && (self.globs.is_empty() || self.globs.iter().any(|g| glob_match(g, b)))
        } else if let Some(k) = key.strip_prefix("obj:") {
            self.area
                .as_ref()
                .is_some_and(|a| k.starts_with(&a.filter) && a.skip.as_deref() != Some(k))
        } else {
            false
        }
    }

    /// The key's state in the store now, read afresh: a commit or a version, or None.
    pub async fn read(&self, key: &str) -> Result<Option<String>> {
        if let Some(b) = key.strip_prefix("ref:") {
            Ok(self.repo.read_ref(b).await?.map(|(h, _)| h.hex()))
        } else if let Some(k) = key.strip_prefix("obj:") {
            match &self.area {
                Some(a) => a.store.object_version(k).await,
                None => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    /// Every watched key in the store now.
    pub async fn current(&self) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        if self.refs {
            for (b, h) in self.repo.list_refs().await? {
                let key = format!("ref:{b}");
                if self.wanted(&key) {
                    out.insert(key, h.hex());
                }
            }
        }
        if let Some(a) = &self.area {
            for o in a.store.list_meta(&a.filter).await? {
                let key = format!("obj:{}", o.key);
                if self.wanted(&key) {
                    out.insert(key, o.version);
                }
            }
        }
        Ok(out)
    }

    fn change(&self, key: &str, now: Option<String>) -> Option<Change> {
        let belief = self.seen.get(key);
        let old = match belief {
            Some(Belief::Is(v)) => Some(v.clone()),
            _ => None,
        };
        let same = match (belief, &now) {
            (Some(Belief::Unsure), _) => false,
            (Some(Belief::Is(v)), Some(n)) => v == n,
            (None, None) => true,
            _ => false,
        };
        (!same).then(|| Change {
            key: key.to_string(),
            old,
            new: now,
        })
    }

    /// Re-read one key (a hint named it): the change from what was emitted, if any.
    pub async fn observe(&self, key: &str) -> Result<Option<Change>> {
        if !self.wanted(key) {
            return Ok(None);
        }
        let now = self.read(key).await?;
        Ok(self.change(key, now))
    }

    /// Re-list everything (the safety poll, the start-up diff): every key whose state
    /// differs from what was emitted, deletions included.
    pub async fn scan(&self) -> Result<Vec<Change>> {
        let current = self.current().await?;
        let mut out = vec![];
        for (k, v) in &current {
            out.extend(self.change(k, Some(v.clone())));
        }
        for k in self.seen.keys() {
            if !current.contains_key(k) {
                out.extend(self.change(k, None));
            }
        }
        Ok(out)
    }

    /// Take the present state as emitted, silently: a restart without a cursor.
    pub async fn seed(&mut self) -> Result<()> {
        for (k, v) in self.current().await? {
            self.seen.insert(k, Belief::Is(v));
        }
        Ok(())
    }

    /// Step 1 of emitting `changes`: the cursor records the intent. A crash from here on
    /// leaves the consumer unsure of these keys.
    pub async fn mark(&mut self, changes: &[Change]) -> Result<()> {
        if let Some(c) = self.cursor.as_mut() {
            self.saved
                .dirty
                .extend(changes.iter().map(|c| c.key.clone()));
            c.save(&self.saved).await?;
        }
        Ok(())
    }

    /// Step 2: write the line (`sink`), then remember it.
    pub fn emit(
        &mut self,
        c: &Change,
        source: &'static str,
        sink: &mut impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        let at = now_ms();
        let event = if let Some(b) = c.key.strip_prefix("ref:") {
            Event::Ref(RefEvent {
                branch: b.to_string(),
                commit: c.new.as_deref().map(Hash::from_hex).transpose()?,
                old: c.old.as_deref().map(Hash::from_hex).transpose()?,
                source,
                at,
            })
        } else {
            Event::Object(ObjectEvent {
                key: c.key.trim_start_matches("obj:").to_string(),
                version: c.new.clone(),
                old: c.old.clone(),
                op: if c.new.is_some() { "put" } else { "delete" },
                source,
                at,
            })
        };
        sink(event)?;
        match &c.new {
            Some(v) => self.seen.insert(c.key.clone(), Belief::Is(v.clone())),
            None => self.seen.remove(&c.key),
        };
        Ok(())
    }

    /// Step 3: the cursor takes the emitted states and clears the marks.
    pub async fn save(&mut self, changes: &[Change]) -> Result<()> {
        for c in changes {
            match &c.new {
                Some(v) => self.saved.heads.insert(c.key.clone(), v.clone()),
                None => self.saved.heads.remove(&c.key),
            };
            self.saved.dirty.remove(&c.key);
        }
        if let Some(cur) = self.cursor.as_mut() {
            cur.save(&self.saved).await?;
        }
        Ok(())
    }

    /// Emit `changes`: mark, then the lines, then save. (Grouped: each key's own order,
    /// mark before line before save, is the model's; a batch is one write for several.)
    pub async fn handle(
        &mut self,
        changes: &[Change],
        source: &'static str,
        sink: &mut impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        self.mark(changes).await?;
        for c in changes {
            self.emit(c, source, sink)?;
        }
        self.save(changes).await
    }

    /// Emit every watched key once, as it is (`--initial`).
    async fn initial(&mut self, sink: &mut impl FnMut(Event) -> Result<()>) -> Result<()> {
        let changes: Vec<Change> = self
            .current()
            .await?
            .into_iter()
            .map(|(key, v)| Change {
                key,
                old: None,
                new: Some(v),
            })
            .collect();
        self.handle(&changes, "initial", sink).await
    }
}

/// Run until `emit` errs. Ref filters, the prefix, the cursor and `initial` are the
/// command's flags; `safety_poll` is how often to re-list when a source delivers hints
/// (zero: never, and a lost hint is then lost until the next restart with a cursor).
pub async fn watch(
    repo: &Repo,
    source: Source,
    spec: &Spec,
    cursor: Option<Cursor>,
    initial: bool,
    safety_poll: Duration,
    mut emit: impl FnMut(Event) -> Result<()>,
) -> Result<()> {
    let mut w = Watcher::open(repo, spec, cursor).await?;
    // The start-up diff (or the legacy seed): what changed while no one was watching.
    if initial {
        w.initial(&mut emit).await?;
    } else if w.has_cursor() {
        let changes = w.scan().await?;
        w.handle(&changes, "resume", &mut emit).await?;
    } else {
        w.seed().await?;
    }
    let (mut hints, period): (Option<Box<dyn HintSource>>, Option<Duration>) = match source {
        Source::Poll { interval } => (None, Some(interval)),
        Source::GcsPubSub {
            subscription,
            token,
        } => {
            let hs = PubSub::new(&repo.store.url, spec, subscription, token)?;
            (
                Some(Box::new(hs)),
                Some(safety_poll).filter(|d| !d.is_zero()),
            )
        }
        Source::Hints(h) => (Some(h), Some(safety_poll).filter(|d| !d.is_zero())),
    };
    let mut next_scan = period.map(|p| tokio::time::Instant::now() + p);
    loop {
        enum Wake {
            Hints(Result<Vec<Hint>>),
            Scan,
        }
        let wake = {
            let timer = async {
                match next_scan {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending().await,
                }
            };
            match hints.as_mut() {
                Some(h) => tokio::select! {
                    r = h.pull() => Wake::Hints(r),
                    _ = timer => Wake::Scan,
                },
                None => {
                    timer.await;
                    Wake::Scan
                }
            }
        };
        match wake {
            Wake::Scan => {
                let changes = w.scan().await?;
                w.handle(&changes, "poll", &mut emit).await?;
                next_scan = period.map(|p| tokio::time::Instant::now() + p);
            }
            Wake::Hints(r) => {
                let batch = r?;
                // The hint says something changed; the store says what.
                let mut seen_keys = BTreeSet::new();
                let mut changes = vec![];
                for h in batch {
                    let key = match h {
                        Hint::Ref(b) => format!("ref:{b}"),
                        Hint::Object(k) => format!("obj:{k}"),
                    };
                    if seen_keys.insert(key.clone()) {
                        changes.extend(w.observe(&key).await?);
                    }
                }
                let name = hints.as_ref().map(|h| h.name()).unwrap_or("hint");
                w.handle(&changes, name, &mut emit).await?;
                if let Some(h) = hints.as_mut() {
                    h.ack().await?;
                }
            }
        }
    }
}

// ---------- GCS Pub/Sub ----------

/// The object-name prefix of a gs:// store's keys ("" or "repo/").
fn gcs_prefix(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("gs://")
        .context("gcs events need a gs:// store")?;
    let prefix = rest
        .split_once('/')
        .map(|(_, p)| p.trim_matches('/'))
        .unwrap_or("");
    Ok(if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    })
}

/// Read the objects a Pub/Sub pull response names: `(ack ids, hints)`. `refs` is the
/// object-name prefix of the repository's refs (`repo/refs/`), `area` that of its sibling
/// area (`repo.actions/`). Messages for anything else are acknowledged and ignored.
pub fn parse_pull(
    body: &serde_json::Value,
    refs: &str,
    area: Option<&str>,
) -> (Vec<String>, Vec<Hint>) {
    let mut acks = vec![];
    let mut hints = vec![];
    for m in body["receivedMessages"].as_array().into_iter().flatten() {
        acks.push(m["ackId"].as_str().unwrap_or_default().to_string());
        let object = m["message"]["attributes"]["objectId"]
            .as_str()
            .unwrap_or_default();
        if let Some(branch) = object.strip_prefix(refs) {
            hints.push(Hint::Ref(branch.to_string()));
        } else if let Some(key) = area.and_then(|a| object.strip_prefix(a)) {
            hints.push(Hint::Object(key.to_string()));
        }
    }
    (acks, hints)
}

struct PubSub {
    client: reqwest::Client,
    base: String,
    token: Arc<crate::creds::Source>,
    refs: String,
    area: Option<String>,
    unacked: Vec<String>,
}

impl PubSub {
    fn new(
        store_url: &str,
        spec: &Spec,
        subscription: String,
        token: Arc<crate::creds::Source>,
    ) -> Result<PubSub> {
        let prefix = gcs_prefix(store_url)?;
        // `repo.actions/`: the whole area (what `--prefix` narrows is judged by the watcher)
        let area = match &spec.prefix {
            Some(p) => Some(gcs_prefix(&area_url(store_url, split_area(p)?.0)?)?),
            None => None,
        };
        Ok(PubSub {
            client: reqwest::Client::new(),
            base: format!(
                "https://pubsub.googleapis.com/v1/{}",
                subscription.trim_start_matches('/')
            ),
            token,
            refs: format!("{prefix}refs/"),
            area,
            unacked: vec![],
        })
    }

    async fn bearer(&self) -> Result<String> {
        let t = self.token.clone();
        tokio::task::spawn_blocking(move || t.get())
            .await??
            .token
            .context("the credential has no token for Pub/Sub")
    }
}

#[async_trait]
impl HintSource for PubSub {
    fn name(&self) -> &'static str {
        "gcs"
    }

    async fn pull(&mut self) -> Result<Vec<Hint>> {
        loop {
            let bearer = self.bearer().await?;
            let resp = self
                .client
                .post(format!("{}:pull", self.base))
                .bearer_auth(&bearer)
                .header("content-type", "application/json")
                .body(serde_json::json!({"maxMessages": 100}).to_string())
                .send()
                .await
                .context("pulling the subscription")?;
            let status = resp.status();
            let body: serde_json::Value =
                serde_json::from_slice(&resp.bytes().await?).unwrap_or_default();
            if !status.is_success() {
                bail!("Pub/Sub pull failed ({status}): {body}");
            }
            let (acks, hints) = parse_pull(&body, &self.refs, self.area.as_deref());
            self.unacked = acks;
            if !self.unacked.is_empty() {
                return Ok(hints);
            }
        }
    }

    async fn ack(&mut self) -> Result<()> {
        if self.unacked.is_empty() {
            return Ok(());
        }
        let bearer = self.bearer().await?;
        let resp = self
            .client
            .post(format!("{}:acknowledge", self.base))
            .bearer_auth(&bearer)
            .header("content-type", "application/json")
            .body(serde_json::json!({"ackIds": self.unacked}).to_string())
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("Pub/Sub acknowledge failed ({})", resp.status());
        }
        self.unacked.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("pr/*", "pr/12"));
        assert!(glob_match("*", "main"));
        assert!(glob_match("main", "main"));
        assert!(!glob_match("pr/*", "main"));
        assert!(glob_match("release/*/stable", "release/2026/stable"));
        assert!(!glob_match("_meta*", "main"));
        assert_eq!(gcs_prefix("gs://b/team/app").unwrap(), "team/app/");
        assert_eq!(gcs_prefix("gs://b").unwrap(), "");
    }

    #[test]
    fn areas() {
        assert_eq!(split_area("actions/").unwrap(), ("actions", ""));
        assert_eq!(split_area("actions/queue/").unwrap(), ("actions", "queue/"));
        assert_eq!(split_area("actions").unwrap(), ("actions", ""));
        assert!(split_area("/queue").is_err());
        assert_eq!(
            area_url("gs://b/team/app", "actions").unwrap(),
            "gs://b/team/app.actions"
        );
        assert_eq!(
            area_url("file:///x/store/", "actions").unwrap(),
            "file:///x/store.actions"
        );
        assert!(area_url("gs://b", "actions").is_err());
        assert!(area_url("https://host/repo", "actions").is_err());
    }

    #[test]
    fn pull_responses_name_refs_and_area_objects() {
        let body = serde_json::json!({"receivedMessages": [
            {"ackId": "1", "message": {"attributes": {"objectId": "team/app/refs/pr/12"}}},
            {"ackId": "2", "message": {"attributes": {"objectId": "team/app.actions/queue/a.json"}}},
            {"ackId": "3", "message": {"attributes": {"objectId": "team/app/commits/ab"}}},
            {"ackId": "4", "message": {"attributes": {}}},
        ]});
        let (acks, hints) = parse_pull(&body, "team/app/refs/", Some("team/app.actions/"));
        assert_eq!(acks, ["1", "2", "3", "4"]);
        assert_eq!(
            hints,
            [
                Hint::Ref("pr/12".into()),
                Hint::Object("queue/a.json".into())
            ]
        );
        let (_, hints) = parse_pull(&body, "team/app/refs/", None);
        assert_eq!(hints, [Hint::Ref("pr/12".into())]);
    }
}
