//! `extensions/pail-pr`, the real plugin, checked against the Quint model of the pull request
//! protocol (`verify/pr.qnt`, scenarios in `verify/pr_scenarios.qnt`).
//!
//! Each client of the model is a process of the plugin. Its `PAIL_BIN` is `tests/pr_refinement/
//! shim.py`, which passes reads to the real `pail` and holds every `put` and `land` until the
//! harness releases it, so a scenario's interleavings (A reads, B writes, A writes) happen at
//! the plugin's real read/write boundary, with no hook in the plugin. The clock is fixed by
//! `run_pr.py`, a launcher that sets `time.time` and runs the plugin unchanged.
//!
//! - `begin(c, cmd)` starts `c`'s process and waits until it is held at its first write, or
//!   has exited;
//! - `release(c)` lets the held write through and waits likewise;
//! - `push`, `ci(k)` are the world: a commit on the PR's branch in a working copy, a passing
//!   check; `view` runs `pail pr view --json`.
//!
//! After each action the snapshot is built from the store: the `_meta` records, the two
//! branches' tips, and each client's held write (what it would put, and the blob it read) or
//! its exit status.

#[path = "pr_refinement/generated.rs"]
mod generated;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use quint_refinements::{NormalizedRuntimeEvidence, RuntimeValue};
use serde_json::Value;

/// The model's `HEAD`: no `--commit`.
const HEAD: i64 = 99;

#[test]
fn pr_plugin_refines_the_pr_model() {
    let results = generated::refine_all::<PrUnderTest>().unwrap_or_else(|e| panic!("{e}"));
    for r in &results {
        eprintln!(
            "{} refined {} obligations",
            r.scenario, r.evaluated_obligations
        );
    }
    assert_eq!(
        results.len(),
        17,
        "every scenario in verify/pr_scenarios.qnt ran"
    );
}

// ---------- values ----------

fn tagged(tag: &str, value: RuntimeValue) -> RuntimeValue {
    RuntimeValue::Variant {
        tag: tag.to_owned(),
        value: Box::new(value),
    }
}

/// A unit constructor, which the model's values carry as its name.
fn unit(tag: &str) -> RuntimeValue {
    RuntimeValue::Text(tag.to_owned())
}

fn record<const N: usize>(fields: [(&str, RuntimeValue); N]) -> RuntimeValue {
    RuntimeValue::Record(fields.map(|(k, v)| (k.to_owned(), v)).into_iter().collect())
}

fn int(i: i64) -> RuntimeValue {
    RuntimeValue::Int(i)
}

fn text(s: &str) -> RuntimeValue {
    RuntimeValue::Text(s.to_owned())
}

fn as_int(v: &RuntimeValue) -> Result<i64, String> {
    match v {
        RuntimeValue::Int(i) => Ok(*i),
        other => Err(format!("expected an integer, got {other:?}")),
    }
}

fn as_text(v: &RuntimeValue) -> Result<&str, String> {
    match v {
        RuntimeValue::Text(t) => Ok(t),
        other => Err(format!("expected text, got {other:?}")),
    }
}

fn field<'a>(v: &'a RuntimeValue, name: &str) -> Result<&'a RuntimeValue, String> {
    match v {
        RuntimeValue::Record(r) => r.get(name).ok_or(format!("no field {name} in {v:?}")),
        other => Err(format!("expected a record, got {other:?}")),
    }
}

fn variant(v: &RuntimeValue) -> Result<(&str, Option<&RuntimeValue>), String> {
    match v {
        RuntimeValue::Variant { tag, value } => Ok((tag, Some(value))),
        RuntimeValue::Text(tag) => Ok((tag, None)),
        other => Err(format!("expected a variant, got {other:?}")),
    }
}

/// A comment's file name: `<13-digit ms>-<who>`.
fn comment_file(id: &RuntimeValue) -> Result<String, String> {
    Ok(format!(
        "{:013}-{}",
        as_int(field(id, "ms")?)?,
        as_text(field(id, "who")?)?
    ))
}

fn comment_id(file: &str) -> RuntimeValue {
    let (ms, who) = file.split_once('-').unwrap_or((file, ""));
    record([("ms", int(ms.parse().unwrap_or(-1))), ("who", text(who))])
}

// ---------- processes ----------

const WAIT: Duration = Duration::from_secs(60);

/// The ways a client's process may be, as the model has them.
struct Proc {
    child: Child,
    /// The model's `Cmd`, as begun.
    cmd: RuntimeValue,
    /// Exited: whether with status 0.
    exited: Option<bool>,
}

struct PrUnderTest {
    dir: tempfile::TempDir,
    pail: PathBuf,
    /// Commit `k` of the model is `hashes[k]` (0: the base's).
    hashes: Vec<String>,
    procs: BTreeMap<String, Proc>,
    /// `view`'s last answer: outdated comment files, and "ready".
    viewed: BTreeSet<String>,
    view_ready: bool,
    /// Every version of every PR record seen, by blob id: a held write names the blob it read.
    seen: RefCell<HashMap<String, RuntimeValue>>,
}

fn who_of(c: &str) -> &str {
    if c == "alice2" { "alice" } else { c }
}

impl PrUnderTest {
    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn store(&self) -> String {
        format!("file://{}", self.path("store").display())
    }

    /// The real pail, as `who` in `cwd`.
    fn cmd(&self, cwd: &Path) -> Command {
        let mut c = Command::new(&self.pail);
        c.current_dir(cwd)
            .env("PAIL_AUTHOR", "tester <t@example.com>")
            .env("PAIL_CONFIG_GLOBAL", self.path("config"))
            .env_remove("PAIL_CLONE")
            .env_remove("PAIL_DIR")
            .env("PAIL_STORE", self.store());
        c
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> String {
        let out = self.cmd(cwd).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "pail {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn rev(&self, rev: &str) -> Option<String> {
        let out = self
            .cmd(self.dir.path())
            .args(["rev-parse", rev])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// What commit `k` holds (`fileAt` in `verify/pr.qnt`).
    fn write_commit(&self, k: i64) {
        let w = self.path("work");
        let a = format!("{k}\n10\n11\n");
        let b = if k <= 1 {
            "20\n21\n22\n".to_string()
        } else {
            format!("20\n21\n22\n{k}\n")
        };
        std::fs::write(w.join("a"), a).unwrap();
        std::fs::write(w.join("b"), b).unwrap();
    }

    fn commit(&mut self, k: i64) {
        let w = self.path("work");
        self.write_commit(k);
        self.run(&w, &["add", "-A"]);
        self.run(&w, &["commit", "-m", &format!("commit {k}")]);
    }

    fn hash(&self, k: i64) -> Result<&str, String> {
        self.hashes
            .get(usize::try_from(k).map_err(|_| format!("commit {k}"))?)
            .map(|s| s.as_str())
            .ok_or(format!("no commit {k}"))
    }

    fn num(&self, hash: &str) -> i64 {
        self.hashes
            .iter()
            .position(|h| h == hash)
            .map(|i| i as i64)
            .unwrap_or(-1)
    }

    // ---------- the plugin ----------

    fn launch(&self, c: &str, args: &[String], ms: i64) -> Child {
        let ctl = self.path("ctl");
        for ext in ["blocked.json", "go", "blocked.tmp"] {
            let _ = std::fs::remove_file(ctl.join(format!("{c}.{ext}")));
        }
        // PR_PLUGIN: another copy of the plugin (a mutant, to see that the scenarios catch it).
        let plugin = std::env::var_os("PR_PLUGIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions/pail-pr"));
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/pr_refinement");
        let mut cmd = Command::new("python3");
        cmd.arg(here.join("run_pr.py"))
            .arg(plugin)
            .args(args)
            .current_dir(self.dir.path())
            .env("PAIL_BIN", self.path("pail-shim"))
            .env("PR_REAL_PAIL", &self.pail)
            .env("PR_CTL", &ctl)
            .env("PR_CLIENT", c)
            .env("PR_NOW_MS", ms.to_string())
            .env("PAIL_STORE", self.store())
            .env("PAIL_AUTHOR", format!("{0} <{0}@example.com>", who_of(c)))
            .env("PAIL_CONFIG_GLOBAL", self.path("config"))
            .env_remove("PAIL_DIR")
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(self.path(&format!("{c}.out"))).unwrap())
            .stderr(std::fs::File::create(self.path(&format!("{c}.err"))).unwrap());
        cmd.spawn().unwrap()
    }

    /// Wait until `c` is held at a write or has exited.
    fn settle(&mut self, c: &str) -> Result<(), String> {
        let blocked = self.path("ctl").join(format!("{c}.blocked.json"));
        let started = Instant::now();
        loop {
            let p = self.procs.get_mut(c).unwrap();
            if blocked.exists() {
                return Ok(());
            }
            if let Some(status) = p.child.try_wait().map_err(|e| e.to_string())? {
                // It may have been held and exited in between (not possible), or written last.
                if blocked.exists() {
                    return Ok(());
                }
                p.exited = Some(status.success());
                if std::env::var_os("PR_DEBUG").is_some() {
                    let err =
                        std::fs::read_to_string(self.path(&format!("{c}.err"))).unwrap_or_default();
                    eprintln!("{c} exited {status}: {}", err.trim());
                }
                return Ok(());
            }
            if started.elapsed() > WAIT {
                let err =
                    std::fs::read_to_string(self.path(&format!("{c}.err"))).unwrap_or_default();
                return Err(format!("{c} neither held at a write nor exited: {err}"));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The plugin's arguments for a model `Cmd`.
    fn args(&self, cmd: &RuntimeValue) -> Result<(Vec<String>, i64), String> {
        let s = |x: &str| x.to_string();
        let (tag, v) = variant(cmd)?;
        let v = v.cloned().unwrap_or(int(0));
        let mut ms = 1;
        let args: Vec<String> = match tag {
            "Create" => vec![
                s("create"),
                s("--head"),
                s("pr/x"),
                s("--base"),
                s("main"),
                s("--title"),
                format!("t{}", as_int(&v)?),
            ],
            "EditTitle" => vec![s("edit"), s("1"), s("--title"), format!("t{}", as_int(&v)?)],
            "EditBody" => vec![s("edit"), s("1"), s("--body"), format!("b{}", as_int(&v)?)],
            "EditDraft" => vec![
                s("edit"),
                s("1"),
                s(if v == RuntimeValue::Bool(true) {
                    "--draft"
                } else {
                    "--ready"
                }),
            ],
            "Ready" => {
                let mut a = vec![s("ready"), s("1")];
                if v == RuntimeValue::Bool(true) {
                    a.push(s("--undo"));
                }
                a
            }
            "Close" => vec![s("close"), s("1")],
            "Reopen" => vec![s("reopen"), s("1")],
            "Merge" => vec![s("merge"), s("1")],
            "CmReview" => {
                let mut a = vec![
                    s("review"),
                    s("1"),
                    s(if field(&v, "approve")? == &RuntimeValue::Bool(true) {
                        "--approve"
                    } else {
                        "--request-changes"
                    }),
                ];
                let commit = as_int(field(&v, "commit")?)?;
                if commit != HEAD {
                    a.extend([s("--commit"), self.hash(commit)?.to_string()]);
                }
                a
            }
            "CmComment" => {
                ms = as_int(field(&v, "ms")?)?;
                let mut a = vec![
                    s("comment"),
                    s("1"),
                    s("-m"),
                    format!("b{}", as_int(field(&v, "body")?)?),
                ];
                let (kind, sp) = variant(field(&v, "spec")?)?;
                match kind {
                    "Top" => {}
                    "Line" => {
                        let sp = sp.ok_or("Line without a payload")?;
                        a.extend([
                            s("--path"),
                            as_text(field(sp, "path")?)?.to_string(),
                            s("--line"),
                            as_int(field(sp, "line")?)?.to_string(),
                            s("--side"),
                            as_text(field(sp, "side")?)?.to_lowercase(),
                        ]);
                        let commit = as_int(field(sp, "commit")?)?;
                        if commit != HEAD {
                            a.extend([s("--commit"), self.hash(commit)?.to_string()]);
                        }
                    }
                    "Reply" => {
                        a.extend([
                            s("--reply-to"),
                            comment_file(sp.ok_or("Reply without a payload")?)?,
                        ]);
                    }
                    other => return Err(format!("unknown spec {other}")),
                }
                a
            }
            other => return Err(format!("cannot begin {other}")),
        };
        Ok((args, ms))
    }

    // ---------- the store, as the model's state ----------

    /// `_meta`'s files: path -> (blob id, JSON).
    fn meta(&self) -> BTreeMap<String, (String, Value)> {
        let r = self
            .cmd(self.dir.path())
            .args(["ls", "-r", "_meta:"])
            .output()
            .unwrap();
        if !r.status.success() {
            return BTreeMap::new();
        }
        let paths: Vec<String> = String::from_utf8_lossy(&r.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().last().map(String::from))
            .filter(|p| p.ends_with(".json"))
            .collect();
        let mut child = self
            .cmd(self.dir.path())
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        let input: String = paths.iter().map(|p| format!("_meta:{p}\n")).collect();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap().stdout;
        let mut pos = 0;
        let mut files = BTreeMap::new();
        for p in &paths {
            let nl = pos + out[pos..].iter().position(|b| *b == b'\n').unwrap();
            let header = String::from_utf8_lossy(&out[pos..nl]).to_string();
            let mut it = header.split_whitespace();
            let (id, _kind, size) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
            let size: usize = size.parse().unwrap();
            let body = &out[nl + 1..nl + 1 + size];
            files.insert(
                p.clone(),
                (
                    id.to_string(),
                    serde_json::from_slice(body).unwrap_or(Value::Null),
                ),
            );
            pos = nl + 1 + size + 1;
        }
        files
    }

    fn pr_value(&self, j: &Value) -> RuntimeValue {
        let num = |s: &str, prefix: &str| {
            s.strip_prefix(prefix)
                .and_then(|n| n.parse().ok())
                .unwrap_or(0)
        };
        let state = match j["state"].as_str().unwrap_or("") {
            "open" => unit("Open"),
            "closed" => unit("Closed"),
            "merging" => tagged(
                "Merging",
                record([
                    ("by", text(j["merging"]["by"].as_str().unwrap_or(""))),
                    (
                        "commit",
                        int(self.num(j["merging"]["commit"].as_str().unwrap_or(""))),
                    ),
                ]),
            ),
            "merged" => tagged(
                "Merged",
                record([(
                    "commit",
                    int(self.num(j["merged_commit"].as_str().unwrap_or(""))),
                )]),
            ),
            other => panic!("unknown PR state {other}"),
        };
        record([
            ("author", text(j["author"].as_str().unwrap_or(""))),
            ("title", int(num(j["title"].as_str().unwrap_or(""), "t"))),
            ("body", int(num(j["body"].as_str().unwrap_or(""), "b"))),
            ("state", state),
            (
                "draft",
                RuntimeValue::Bool(j["draft"].as_bool().unwrap_or(false)),
            ),
        ])
    }

    fn comment_value(&self, j: &Value) -> RuntimeValue {
        let anchor = match j["path"].as_str() {
            Some(path) => tagged(
                "Anchored",
                record([
                    ("path", text(path)),
                    ("line", int(j["line"].as_i64().unwrap_or(0))),
                    ("side", unit(if j["side"] == "old" { "Old" } else { "New" })),
                    ("commit", int(self.num(j["commit"].as_str().unwrap_or("")))),
                ]),
            ),
            None => unit("NoAnchor"),
        };
        let reply = match j["in_reply_to"].as_str() {
            Some(id) => tagged("ReplyTo", comment_id(id)),
            None => unit("NoReply"),
        };
        let body = j["body"]
            .as_str()
            .unwrap_or("")
            .strip_prefix('b')
            .and_then(|n| n.parse().ok());
        record([
            ("body", int(body.unwrap_or(0))),
            ("anchor", anchor),
            ("reply", reply),
        ])
    }

    fn review_value(&self, j: &Value) -> RuntimeValue {
        record([
            ("approve", RuntimeValue::Bool(j["verdict"] == "approve")),
            ("commit", int(self.num(j["commit"].as_str().unwrap_or("")))),
        ])
    }

    /// What a client is held at, from the call its `put` or `land` was about to make.
    fn held(&self, c: &str, blocked: &Value) -> Result<RuntimeValue, String> {
        let argv: Vec<&str> = blocked["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        let flag = |name: &str| argv.iter().position(|a| *a == name).map(|i| argv[i + 1]);
        if argv[0] == "land" {
            // A land that names no commit lands whatever the branch is by then.
            let Some(tip) = flag("--expect-tip") else {
                return Ok(tagged("WLandAny", unit("Any")));
            };
            return Ok(tagged("WLand", record([("commit", int(self.num(tip)))])));
        }
        let (path, body) = blocked["files"].as_object().unwrap().iter().next().unwrap();
        let j: Value = serde_json::from_str(body.as_str().unwrap()).unwrap();
        let if_absent = argv.contains(&"--if-absent");
        let parts: Vec<&str> = path.split('/').collect();
        match parts.as_slice() {
            ["prs", n] if if_absent => Ok(tagged(
                "WCreate",
                record([
                    ("n", int(n.trim_end_matches(".json").parse().unwrap())),
                    ("rec", self.pr_value(&j)),
                ]),
            )),
            ["prs", _] => {
                // A write of the record that is not a compare-and-swap is not the model's.
                let Some(expect) = flag("--expect").and_then(|e| e.split_once('=')) else {
                    return Ok(tagged("WBlind", record([("after", self.pr_value(&j))])));
                };
                let before = self
                    .seen
                    .borrow()
                    .get(expect.1)
                    .ok_or(format!(
                        "{c} read a PR record the harness never saw: {}",
                        expect.1
                    ))?
                    .clone();
                let after = self.pr_value(&j);
                let cmd = &self.procs[c].cmd;
                let cmd = if variant(cmd)?.0 == "Merge" {
                    // A merge writes three records: tell them apart by what they say.
                    match (
                        variant(field(&after, "state")?)?.0,
                        variant(field(&before, "state")?)?.0,
                    ) {
                        ("Merging", _) => unit("Merge"),
                        ("Open", "Merging") => unit("Release"),
                        ("Merged", _) => tagged(
                            "Finish",
                            field(variant(field(&after, "state")?)?.1.unwrap(), "commit")?.clone(),
                        ),
                        other => return Err(format!("a merge wrote {other:?}")),
                    }
                } else {
                    cmd.clone()
                };
                Ok(tagged(
                    "WSave",
                    record([("expect", before), ("after", after), ("cmd", cmd)]),
                ))
            }
            ["prs", _, "reviews", _] => Ok(tagged("WReview", self.review_value(&j))),
            ["prs", _, "comments", file] if if_absent => Ok(tagged(
                "WComment",
                record([
                    ("id", comment_id(file.trim_end_matches(".json"))),
                    ("c", self.comment_value(&j)),
                ]),
            )),
            _ => Ok(tagged("WOther", text(&argv.join(" ")))),
        }
    }

    fn state(&self) -> RuntimeValue {
        let meta = self.meta();
        let mut prs = BTreeMap::new();
        let mut reviews = BTreeMap::new();
        let mut comments = BTreeMap::new();
        let mut checks = BTreeSet::new();
        for (p, (id, j)) in &meta {
            let parts: Vec<&str> = p.split('/').collect();
            match parts.as_slice() {
                ["prs", n] => {
                    let v = self.pr_value(j);
                    self.seen.borrow_mut().insert(id.clone(), v.clone());
                    prs.insert(int(n.trim_end_matches(".json").parse().unwrap()), v);
                }
                ["prs", "1", "reviews", who] => {
                    reviews.insert(text(who.trim_end_matches(".json")), self.review_value(j));
                }
                ["prs", "1", "comments", file] => {
                    comments.insert(
                        comment_id(file.trim_end_matches(".json")),
                        self.comment_value(j),
                    );
                }
                ["checks", hash, "test.json"] if j["state"] == "success" => {
                    checks.insert(int(self.num(hash)));
                }
                _ => {}
            }
        }
        let tip = self.num(&self.rev("pr/x").unwrap());
        let landed = self.num(&self.rev("main").unwrap());
        let mut clients = BTreeMap::new();
        for c in ["alice", "alice2", "bob", "carol"] {
            let blocked = self.path("ctl").join(format!("{c}.blocked.json"));
            let (phase, res) = match self.procs.get(c) {
                None => (unit("Idle"), unit("NoRes")),
                Some(p) if blocked.exists() => {
                    let j: Value =
                        serde_json::from_str(&std::fs::read_to_string(&blocked).unwrap()).unwrap();
                    let w = self.held(c, &j).unwrap_or_else(|e| panic!("{e}"));
                    let _ = p;
                    (tagged("Wait", w), unit("NoRes"))
                }
                Some(p) => (
                    unit("Idle"),
                    unit(match p.exited {
                        Some(true) => "Acked",
                        Some(false) => "Refused",
                        None => "NoRes",
                    }),
                ),
            };
            clients.insert(text(c), record([("phase", phase), ("res", res)]));
        }
        record([
            ("prs", RuntimeValue::Map(prs)),
            ("reviews", RuntimeValue::Map(reviews)),
            ("comments", RuntimeValue::Map(comments)),
            ("checks", RuntimeValue::Set(checks)),
            ("tip", int(tip)),
            ("landed", int(landed)),
            ("clients", RuntimeValue::Map(clients)),
            (
                "viewed",
                RuntimeValue::Set(self.viewed.iter().map(|f| comment_id(f)).collect()),
            ),
            ("viewReady", RuntimeValue::Bool(self.view_ready)),
        ])
    }
}

impl Drop for PrUnderTest {
    fn drop(&mut self) {
        for p in self.procs.values_mut() {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
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

    fn resolve_call(
        &self,
        _operator: &str,
        _arguments: &[RuntimeValue],
    ) -> Option<Result<RuntimeValue, String>> {
        None
    }
}

impl generated::Implementation for PrUnderTest {
    type Evidence = Snapshot;

    /// A store with the base (commit 0: `a`, `b` and the policy: one approval, check `test`)
    /// and the PR's branch `pr/x` with its first commit.
    fn from_initial_state(_initial: &RuntimeValue) -> Result<Self, String> {
        let pail = PathBuf::from(env!("CARGO_BIN_EXE_pail"));
        let mut t = PrUnderTest {
            dir: tempfile::tempdir().unwrap(),
            pail,
            hashes: vec![],
            procs: BTreeMap::new(),
            viewed: BTreeSet::new(),
            view_ready: false,
            seen: RefCell::new(HashMap::new()),
        };
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/pr_refinement");
        std::fs::create_dir_all(t.path("ctl")).unwrap();
        let shim = t.path("pail-shim");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\nexec python3 {} \"$@\"\n",
                here.join("shim.py").display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let d = t.dir.path().to_path_buf();
        let store = t.store();
        t.run(&d, &["init", &store]);
        t.run(&d, &["clone", &store, "work"]);
        let w = t.path("work");
        t.write_commit(0);
        std::fs::write(
            w.join("pail.pr.json"),
            r#"{"approvals":1,"required_checks":["test"]}"#,
        )
        .unwrap();
        t.run(&w, &["add", "-A"]);
        t.run(&w, &["commit", "-m", "base"]);
        let base = t.rev("main").ok_or("no main")?;
        t.run(&w, &["switch", "-c", "pr/x"]);
        t.commit(1);
        let first = t.rev("pr/x").ok_or("no pr/x")?;
        t.hashes = vec![base, first];
        Ok(t)
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot(self.state())
    }

    fn begin(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [c, cmd] = arguments else {
            return Err(format!(
                "begin takes a client and a command, got {arguments:?}"
            ));
        };
        let c = as_text(c)?.to_string();
        let (args, ms) = self.args(cmd)?;
        let child = self.launch(&c, &args, ms);
        self.procs.insert(
            c.clone(),
            Proc {
                child,
                cmd: cmd.clone(),
                exited: None,
            },
        );
        self.settle(&c)
    }

    fn release(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let c = as_text(arguments.first().ok_or("release takes a client")?)?.to_string();
        let ctl = self.path("ctl");
        let blocked = ctl.join(format!("{c}.blocked.json"));
        if !blocked.exists() {
            return Err(format!("{c} is not held at a write"));
        }
        std::fs::remove_file(&blocked).unwrap();
        std::fs::write(ctl.join(format!("{c}.go")), "").unwrap();
        self.settle(&c)
    }

    fn push(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        let k = self.hashes.len() as i64;
        self.commit(k);
        let h = self.rev("pr/x").ok_or("no pr/x")?;
        self.hashes.push(h);
        Ok(())
    }

    fn ci(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let k = as_int(arguments.first().ok_or("ci takes a commit")?)?;
        let hash = self.hash(k)?.to_string();
        let src = self.path("check.json");
        std::fs::write(&src, r#"{"state":"success"}"#).unwrap();
        let d = self.dir.path().to_path_buf();
        self.run(
            &d,
            &[
                "put",
                "_meta",
                "-m",
                "check",
                "--file",
                &format!("checks/{hash}/test.json={}", src.display()),
            ],
        );
        Ok(())
    }

    fn view(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        self.procs.remove("view");
        let mut child = self.launch("view", &["view".into(), "1".into(), "--json".into()], 1);
        let status = child.wait().unwrap();
        let out = std::fs::read_to_string(self.path("view.out")).unwrap();
        if !status.success() {
            return Err(format!(
                "view failed: {}",
                std::fs::read_to_string(self.path("view.err")).unwrap_or_default()
            ));
        }
        let j: Value =
            serde_json::from_str(&out).map_err(|e| format!("view --json: {e}: {out}"))?;
        self.viewed = j["comments"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["outdated"] == true)
            .map(|c| c["file"].as_str().unwrap().to_string())
            .collect();
        self.view_ready = j["ready"] == true;
        Ok(())
    }
}
