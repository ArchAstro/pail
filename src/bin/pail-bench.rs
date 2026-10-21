//! pail-bench: benchmarks on a real repository's snapshot, against pail stores and against
//! Git pushing to a bare remote.
//!
//! The base is a Git commit (default: the tip of a mirror's main) imported once into the
//! store. Every scenario then works on its own branch off that base:
//!
//! 1. small      sequential commits of 1-3 files (library), plus a working-copy variant
//! 2. wide       one commit editing 2,000 files; adding 5,000; deleting a 3,000-file dir;
//!               renaming a 2,000-file dir
//! 3. writers    1/4/16 concurrent writers on one branch, disjoint paths
//! 4. hot        8 writers where 30% of commits touch a shared hot file
//! 5. stacks     40 branches with 5-commit stacks landed while main moves; stacked
//!               branches; 8 concurrent landers
//! 6. bigfile    256 MB file, 1 MB changed in the middle
//! 7. reads      log, log -- path, diff between branch tips, switch, worktree add
//! 8. wc         the CLI in a working copy: 50 x (edit 1-3 files, `pail add -A`, `pail commit`)
//!               against `git add -A`, `git commit`, `git push`, each step timed
//!
//! Git runs the same edits in working copies and pushes to a bare remote (concurrent writers
//! retry with pull --rebase), so both sides pay for publishing to a shared repository.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::{Value, json};

use pail::gitsrc::{Git, kind_for_mode};
use pail::hash::Hash;
use pail::manifest::Entry;
use pail::repo::{CommitRequest, ConflictError, Delta, FileInput, Repo};
use pail::store::Store;

#[derive(Parser)]
#[command(about = "Benchmark pail (and Git) on a real repository's snapshot")]
struct Args {
    /// Git repository holding the base commit (a bare mirror works).
    #[arg(long)]
    git_dir: PathBuf,
    /// Base commit (default: refs/remotes/origin/main, else HEAD).
    #[arg(long)]
    base: Option<String>,
    /// pail store to benchmark (file://, s3://, gs://); a fresh prefix is best.
    #[arg(long)]
    store: String,
    /// Scratch directory for working copies and the Git remote.
    #[arg(long)]
    work: PathBuf,
    /// Scenarios to run (default: all).
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// Skip the Git baseline.
    #[arg(long)]
    no_git: bool,
    /// Where to write results (JSON).
    #[arg(long)]
    out: Option<PathBuf>,
}

/// One measured result.
#[derive(Default, Clone)]
struct Row {
    scenario: String,
    system: String,
    what: String,
    secs: f64,
    ops: u64,
    p50_ms: Option<f64>,
    p95_ms: Option<f64>,
    puts: u64,
    gets: u64,
    up: u64,
    cas: u64,
    note: String,
}

impl Row {
    fn json(&self) -> Value {
        json!({"scenario": self.scenario, "system": self.system, "what": self.what, "secs": self.secs, "ops": self.ops,
               "p50_ms": self.p50_ms, "p95_ms": self.p95_ms, "puts": self.puts, "gets": self.gets, "bytes_up": self.up,
               "cas": self.cas, "note": self.note})
    }
}

fn pct(v: &mut [f64], p: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(v[((v.len() as f64 - 1.0) * p).round() as usize])
}

fn mb(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MB", n as f64 / 1048576.0)
    } else {
        format!("{:.1} KB", n as f64 / 1024.0)
    }
}

/// Store counters before/after a measured block.
struct Meter {
    t: Instant,
    s: (u64, u64, u64, u64),
    cas: u64,
}

fn meter(repo: &Repo) -> Meter {
    Meter {
        t: Instant::now(),
        s: repo.store.stats.snapshot(),
        cas: repo.store.stats.cas.load(Relaxed),
    }
}

fn finish(
    m: Meter,
    repo: &Repo,
    scenario: &str,
    what: &str,
    ops: u64,
    mut lat: Vec<f64>,
    note: String,
) -> Row {
    let s = repo.store.stats.snapshot();
    Row {
        scenario: scenario.into(),
        system: "pail".into(),
        what: what.into(),
        secs: m.t.elapsed().as_secs_f64(),
        ops,
        p50_ms: pct(&mut lat, 0.5),
        p95_ms: pct(&mut lat, 0.95),
        gets: s.0 - m.s.0,
        puts: s.1 - m.s.1,
        up: s.3 - m.s.3,
        cas: repo.store.stats.cas.load(Relaxed) - m.cas,
        note,
    }
}

fn text(rng: &mut StdRng, len: usize) -> Bytes {
    const WORDS: &[&str] = &[
        "let", "fn", "pail", "commit", "page", "pack", "chunk", "branch", "store", "range", "=",
        "{", "}", "(", ")", ";", "\n", "    ",
    ];
    let mut s = String::with_capacity(len + 16);
    while s.len() < len {
        s.push_str(WORDS[rng.random_range(0..WORDS.len())]);
        s.push(' ');
    }
    Bytes::from(s)
}

fn req(branch: &str, base: Option<Hash>, delta: Delta, msg: &str) -> CommitRequest {
    CommitRequest {
        branch: branch.into(),
        base,
        delta,
        author: "Bench <bench@pail>".into(),
        author_date: "1700000000 +0000".into(),
        committer: "Bench <bench@pail>".into(),
        committer_date: "1700000000 +0000".into(),
        message: msg.into(),
        meta: BTreeMap::new(),
    }
}

struct Bench {
    repo: Arc<Repo>,
    base: Hash,
    files: Vec<Entry>,
    rows: Vec<Row>,
    git: Option<GitSide>,
}

impl Bench {
    /// Write new contents for `paths` and commit them to `branch` on top of `base`.
    async fn edit(
        &self,
        branch: &str,
        base: Option<Hash>,
        paths: &[String],
        rng: &mut StdRng,
        msg: &str,
    ) -> Result<pail::repo::CommitOutcome> {
        let inputs: Vec<FileInput> = paths
            .iter()
            .map(|p| FileInput {
                path: p.clone(),
                kind: pail::manifest::Kind::File,
                data: {
                    let n = rng.random_range(200..6000);
                    text(rng, n)
                },
            })
            .collect();
        let entries = self.repo.write_files(&inputs, &HashMap::new()).await?;
        let delta: Delta = entries
            .into_iter()
            .map(|e| (e.path.clone(), Some(e)))
            .collect();
        self.repo.commit(req(branch, base, delta, msg)).await
    }

    fn pick(&self, rng: &mut StdRng, n: usize) -> Vec<String> {
        (0..n)
            .map(|_| {
                self.files[rng.random_range(0..self.files.len())]
                    .path
                    .clone()
            })
            .collect()
    }

    fn branch(&self, name: &str) -> String {
        format!("bench/{name}")
    }

    async fn fresh_branch(&self, name: &str) -> Result<String> {
        let b = self.branch(name);
        if self.repo.read_ref(&b).await?.is_some() {
            self.repo.force_ref(&b, self.base).await?;
        } else {
            self.repo.create_branch(&b, self.base).await?;
        }
        Ok(b)
    }

    fn push(&mut self, r: Row) {
        println!(
            "  {:9} {:5} {:44} {:8.2}s  {:>6} ops{}  {:>6} PUTs  {:>10} up{}",
            r.scenario,
            r.system,
            r.what,
            r.secs,
            r.ops,
            r.p50_ms
                .map(|p| format!("  p50 {p:6.0} ms  p95 {:6.0} ms", r.p95_ms.unwrap_or(0.0)))
                .unwrap_or_default(),
            r.puts,
            mb(r.up),
            if r.note.is_empty() {
                String::new()
            } else {
                format!("  {}", r.note)
            }
        );
        self.rows.push(r);
    }

    // ------------------------------------------------------------------ scenarios

    async fn small(&mut self) -> Result<()> {
        let b = self.fresh_branch("small").await?;
        let mut rng = StdRng::seed_from_u64(1);
        let m = meter(&self.repo);
        let mut lat = vec![];
        let mut head = Some(self.base);
        for i in 0..200 {
            let n = rng.random_range(1..=3);
            let paths = self.pick(&mut rng, n);
            let t = Instant::now();
            head = Some(
                self.edit(&b, head, &paths, &mut rng, &format!("small {i}"))
                    .await?
                    .commit,
            );
            lat.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let r = finish(
            m,
            &self.repo,
            "small",
            "200 commits of 1-3 files, one writer",
            200,
            lat,
            String::new(),
        );
        self.push(r);
        if let Some(g) = &self.git {
            let r = g.small(&self.files)?;
            self.push(r);
        }
        Ok(())
    }

    async fn wide(&mut self) -> Result<()> {
        let mut rng = StdRng::seed_from_u64(2);
        // Edit 2,000 files across the tree.
        let b = self.fresh_branch("wide").await?;
        let paths: Vec<String> = {
            let mut v = self.pick(&mut rng, 2400);
            v.sort();
            v.dedup();
            v.truncate(2000);
            v
        };
        let m = meter(&self.repo);
        let out = self
            .edit(&b, Some(self.base), &paths, &mut rng, "edit 2000")
            .await?;
        let r = finish(
            m,
            &self.repo,
            "wide",
            "edit 2,000 files across the tree",
            1,
            vec![],
            format!("{} pages", out.pages_written),
        );
        self.push(r);
        // Add 5,000 new files in 50 new directories.
        let new: Vec<String> = (0..5000)
            .map(|i| format!("bench_new/d{:02}/f{i:05}.txt", i % 50))
            .collect();
        let m = meter(&self.repo);
        let out = self
            .edit(&b, Some(out.commit), &new, &mut rng, "add 5000")
            .await?;
        let r = finish(
            m,
            &self.repo,
            "wide",
            "add 5,000 files",
            1,
            vec![],
            format!("{} pages", out.pages_written),
        );
        self.push(r);
        // Delete the largest top-level directory under 3,000+ files.
        let (dir, n) = largest_dir(&self.files, 3000);
        let delta: Delta = self
            .files
            .iter()
            .filter(|e| e.path.starts_with(&format!("{dir}/")))
            .map(|e| (e.path.clone(), None))
            .collect();
        let m = meter(&self.repo);
        let out2 = self
            .repo
            .commit(req(&b, Some(out.commit), delta, "delete dir"))
            .await?;
        let r = finish(
            m,
            &self.repo,
            "wide",
            &format!("delete {dir}/ ({n} files)"),
            1,
            vec![],
            format!("{} pages", out2.pages_written),
        );
        self.push(r);
        // Rename a ~2,000-file directory: content is reused, only the manifest changes.
        let (rdir, rn) = largest_dir(&self.files, 2000);
        let mut delta: Delta = BTreeMap::new();
        for e in self
            .files
            .iter()
            .filter(|e| e.path.starts_with(&format!("{rdir}/")))
        {
            delta.insert(e.path.clone(), None);
            let mut moved = e.clone();
            moved.path = format!("{rdir}_renamed/{}", &e.path[rdir.len() + 1..]);
            delta.insert(moved.path.clone(), Some(moved));
        }
        let b2 = self.fresh_branch("rename").await?;
        let m = meter(&self.repo);
        let out3 = self
            .repo
            .commit(req(&b2, Some(self.base), delta, "rename dir"))
            .await?;
        let r = finish(
            m,
            &self.repo,
            "wide",
            &format!("rename {rdir}/ ({rn} files)"),
            1,
            vec![],
            format!("{} pages", out3.pages_written),
        );
        self.push(r);
        if let Some(g) = &self.git {
            for r in g.wide(&paths, &new, &dir, &rdir)? {
                self.push(r);
            }
        }
        Ok(())
    }

    async fn writers(&mut self) -> Result<()> {
        for w in [1usize, 4, 16] {
            let b = self.fresh_branch(&format!("writers{w}")).await?;
            let per = 20;
            let m = meter(&self.repo);
            let mut tasks = vec![];
            for i in 0..w {
                let repo = self.repo.clone();
                let b = b.clone();
                let base = self.base;
                tasks.push(tokio::spawn(async move {
                    let mut rng = StdRng::seed_from_u64(100 + i as u64);
                    let mut lat = vec![];
                    let mut attempts = 0;
                    let mut head = Some(base);
                    for j in 0..per {
                        let path = format!("bench_writers/w{i:02}/f{j:03}.txt");
                        let data = text(&mut rng, 2000);
                        let t = Instant::now();
                        let es = repo
                            .write_files(
                                &[FileInput {
                                    path: path.clone(),
                                    kind: pail::manifest::Kind::File,
                                    data,
                                }],
                                &HashMap::new(),
                            )
                            .await?;
                        let delta: Delta =
                            es.into_iter().map(|e| (e.path.clone(), Some(e))).collect();
                        let out = repo.commit(req(&b, head, delta, "w")).await?;
                        head = Some(out.commit);
                        attempts += out.attempts;
                        lat.push(t.elapsed().as_secs_f64() * 1000.0);
                    }
                    Ok::<_, anyhow::Error>((lat, attempts))
                }));
            }
            let mut lat = vec![];
            let mut attempts = 0;
            for t in tasks {
                let (l, a) = t.await??;
                lat.extend(l);
                attempts += a;
            }
            let n = (w * per) as u64;
            let secs = m.t.elapsed().as_secs_f64();
            let r = finish(
                m,
                &self.repo,
                "writers",
                &format!("{w} writer(s) x {per} commits, disjoint paths"),
                n,
                lat,
                format!(
                    "{:.1} commits/s, {:.2} attempts/commit",
                    n as f64 / secs,
                    attempts as f64 / n as f64
                ),
            );
            self.push(r);
            if let Some(g) = &self.git {
                let r = g.writers(w, per)?;
                self.push(r);
            }
        }
        Ok(())
    }

    async fn hot(&mut self) -> Result<()> {
        let b = self.fresh_branch("hot").await?;
        let (w, per) = (8usize, 20usize);
        let m = meter(&self.repo);
        let mut tasks = vec![];
        for i in 0..w {
            let repo = self.repo.clone();
            let b = b.clone();
            let base = self.base;
            tasks.push(tokio::spawn(async move {
                let mut rng = StdRng::seed_from_u64(200 + i as u64);
                let (mut ok, mut conflicts) = (0u64, 0u64);
                let mut head = Some(base);
                for j in 0..per {
                    let path = if rng.random_bool(0.3) {
                        "bench_hot/shared.txt".to_string()
                    } else {
                        format!("bench_hot/w{i}/f{j}.txt")
                    };
                    let data = text(&mut rng, 1000);
                    let es = repo
                        .write_files(
                            &[FileInput {
                                path,
                                kind: pail::manifest::Kind::File,
                                data,
                            }],
                            &HashMap::new(),
                        )
                        .await?;
                    let delta: Delta = es.into_iter().map(|e| (e.path.clone(), Some(e))).collect();
                    match repo.commit(req(&b, head, delta, "h")).await {
                        Ok(out) => {
                            ok += 1;
                            head = Some(out.commit);
                        }
                        Err(e) if e.downcast_ref::<ConflictError>().is_some() => {
                            // As a user would: sync to the new head and move on.
                            conflicts += 1;
                            head = repo.read_ref(&b).await?.map(|(h, _)| h);
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok::<_, anyhow::Error>((ok, conflicts))
            }));
        }
        let (mut ok, mut conflicts) = (0, 0);
        for t in tasks {
            let (a, c) = t.await??;
            ok += a;
            conflicts += c;
        }
        let r = finish(
            m,
            &self.repo,
            "hot",
            "8 writers x 20, 30% touch one shared file",
            ok + conflicts,
            vec![],
            format!("{ok} landed, {conflicts} conflicts (stale edits of the shared file)"),
        );
        self.push(r);
        Ok(())
    }

    async fn stacks(&mut self) -> Result<()> {
        let main = self.fresh_branch("stacks-main").await?;
        let mut rng = StdRng::seed_from_u64(3);
        // 40 branches, each a 5-commit stack on the base.
        let mut names = vec![];
        let build = meter(&self.repo);
        for i in 0..40 {
            let b = self.fresh_branch(&format!("stack{i:02}")).await?;
            let mut head = Some(self.base);
            for j in 0..5 {
                let paths = vec![
                    format!("bench_stacks/s{i:02}/c{j}.txt"),
                    format!("bench_stacks/s{i:02}/shared.txt"),
                ];
                head = Some(
                    self.edit(&b, head, &paths, &mut rng, &format!("s{i} c{j}"))
                        .await?
                        .commit,
                );
            }
            names.push(b);
        }
        let r = finish(
            build,
            &self.repo,
            "stacks",
            "build 40 branches x 5-commit stacks",
            200,
            vec![],
            String::new(),
        );
        self.push(r);
        // Land them one by one while main also gets unrelated commits.
        let m = meter(&self.repo);
        let mut lat = vec![];
        let (mut ff, mut replay) = (0, 0);
        let mut head = Some(self.base);
        for (i, b) in names.iter().enumerate() {
            if i % 4 == 0 {
                let paths = vec![format!("bench_stacks/main/{i}.txt")];
                self.edit(&main, head, &paths, &mut rng, "main moves")
                    .await?;
            }
            let t = Instant::now();
            let out = self
                .repo
                .land_stack(b, &main, "Lander <l@x>", "1700000000 +0000")
                .await?;
            head = Some(out.tip);
            lat.push(t.elapsed().as_secs_f64() * 1000.0);
            if out.fast_forward {
                ff += 1
            } else {
                replay += 1
            }
        }
        let r = finish(
            m,
            &self.repo,
            "stacks",
            "land 40 stacks while main moves",
            40,
            lat,
            format!("{ff} fast-forward, {replay} replayed"),
        );
        self.push(r);
        // Stacked branches: b2 on b1 on b0; landing b2 lands all three.
        let mut prev = self.base;
        let mut chain = vec![];
        for k in 0..3 {
            let b = self.fresh_branch(&format!("chain{k}")).await?;
            self.repo.force_ref(&b, prev).await?;
            let mut h = Some(prev);
            for j in 0..4 {
                let paths = vec![format!("bench_chain/b{k}/c{j}.txt")];
                h = Some(
                    self.edit(&b, h, &paths, &mut rng, &format!("chain{k} c{j}"))
                        .await?
                        .commit,
                );
            }
            prev = h.unwrap();
            chain.push(b);
        }
        let m = meter(&self.repo);
        let out = self
            .repo
            .land_stack(&chain[2], &main, "Lander <l@x>", "1700000000 +0000")
            .await?;
        let r = finish(
            m,
            &self.repo,
            "stacks",
            "land a 3-branch chain (12 commits) at once",
            1,
            vec![],
            format!("{} commits landed", out.landed.len()),
        );
        self.push(r);
        // 8 landers racing for main.
        let mut racers = vec![];
        for i in 0..8 {
            let b = self.fresh_branch(&format!("race{i}")).await?;
            let mut h = Some(self.base);
            for j in 0..3 {
                let paths = vec![format!("bench_race/r{i}/c{j}.txt")];
                h = Some(self.edit(&b, h, &paths, &mut rng, "race").await?.commit);
            }
            racers.push(b);
        }
        let m = meter(&self.repo);
        let mut tasks = vec![];
        for b in racers {
            let repo = self.repo.clone();
            let main = main.clone();
            tasks.push(tokio::spawn(async move {
                let t = Instant::now();
                let out = repo
                    .land_stack(&b, &main, "Lander <l@x>", "1700000000 +0000")
                    .await?;
                Ok::<_, anyhow::Error>((t.elapsed().as_secs_f64() * 1000.0, out.attempts))
            }));
        }
        let mut lat = vec![];
        let mut attempts = 0;
        for t in tasks {
            let (l, a) = t.await??;
            lat.push(l);
            attempts += a;
        }
        let r = finish(
            m,
            &self.repo,
            "stacks",
            "8 landers racing (3-commit stacks)",
            8,
            lat,
            format!("{:.2} attempts/landing", attempts as f64 / 8.0),
        );
        self.push(r);
        Ok(())
    }

    async fn bigfile(&mut self) -> Result<()> {
        let b = self.fresh_branch("bigfile").await?;
        // Fresh bytes every run: chunks left in the store by an earlier run would make the
        // first upload look free.
        let mut rng = StdRng::seed_from_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos() as u64,
        );
        let mut data = vec![0u8; 256 << 20];
        rng.fill(&mut data[..]);
        let m = meter(&self.repo);
        let e = self
            .repo
            .write_files(
                &[FileInput {
                    path: "bench_big/blob.bin".into(),
                    kind: pail::manifest::Kind::File,
                    data: Bytes::from(data.clone()),
                }],
                &HashMap::new(),
            )
            .await?;
        let out = self
            .repo
            .commit(req(
                &b,
                Some(self.base),
                e.into_iter().map(|e| (e.path.clone(), Some(e))).collect(),
                "big",
            ))
            .await?;
        let r = finish(
            m,
            &self.repo,
            "bigfile",
            "add a 256 MB file",
            1,
            vec![],
            String::new(),
        );
        self.push(r);
        let mid = 128 << 20;
        rng.fill(&mut data[mid..mid + (1 << 20)]);
        let m = meter(&self.repo);
        let e = self
            .repo
            .write_files(
                &[FileInput {
                    path: "bench_big/blob.bin".into(),
                    kind: pail::manifest::Kind::File,
                    data: Bytes::from(data),
                }],
                &HashMap::new(),
            )
            .await?;
        self.repo
            .commit(req(
                &b,
                Some(out.commit),
                e.into_iter().map(|e| (e.path.clone(), Some(e))).collect(),
                "big edit",
            ))
            .await?;
        let r = finish(
            m,
            &self.repo,
            "bigfile",
            "change 1 MB in its middle",
            1,
            vec![],
            "only the touched chunks upload".into(),
        );
        self.push(r);
        Ok(())
    }

    async fn wc(&mut self, store: &str, work: &Path) -> Result<()> {
        let bin = std::env::current_exe()?.with_file_name("pail");
        let b = self.fresh_branch("wc").await?;
        let dir = work.join("pail-wc");
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        run(Command::new(&bin)
            .args(["clone", store])
            .arg(&dir)
            .args(["-b", &b]))?;
        let pail = |args: &[&str]| -> Result<f64> {
            let t = Instant::now();
            run(Command::new(&bin)
                .current_dir(&dir)
                .args(args)
                .env("PAIL_AUTHOR", "Bench <bench@pail>")
                .env("PAIL_CONFIG_GLOBAL", "/dev/null"))?;
            Ok(t.elapsed().as_secs_f64() * 1000.0)
        };
        let mut rng = StdRng::seed_from_u64(8);
        let (mut add, mut commit, mut lat) = (vec![], vec![], vec![]);
        let m = meter(&self.repo);
        for i in 0..50 {
            let n = rng.random_range(1..=3);
            for p in self.pick(&mut rng, n) {
                let len = rng.random_range(200..6000);
                write_text(&dir, &p, &mut rng, len)?;
            }
            let a = pail(&["add", "-A"])?;
            let c = pail(&["commit", "-m", &format!("wc {i}")])?;
            add.push(a);
            commit.push(c);
            lat.push(a + c);
        }
        let note = format!(
            "add p50 {:.0} ms, commit+publish p50 {:.0} ms",
            pct(&mut add, 0.5).unwrap(),
            pct(&mut commit, 0.5).unwrap()
        );
        // The CLI talks to the store from another process, so the meter only covers time.
        let mut r = finish(
            m,
            &self.repo,
            "wc",
            "50 x edit, add -A, commit (CLI, working copy)",
            50,
            lat,
            note,
        );
        (r.puts, r.gets, r.up, r.cas) = (0, 0, 0, 0);
        self.push(r);
        let _ = std::fs::remove_dir_all(&dir);
        if let Some(g) = &self.git {
            let r = g.wc(&self.files)?;
            self.push(r);
        }
        Ok(())
    }

    async fn reads(&mut self, store: &str, work: &Path) -> Result<()> {
        let bin = std::env::current_exe()?.with_file_name("pail");
        let repo_dir = work.join("reads.pail");
        if !repo_dir.exists() {
            run(Command::new(&bin)
                .args(["clone", "--bare", store])
                .arg(&repo_dir))?;
        }
        let pail = |args: &[&str]| -> Result<f64> {
            let mut best = f64::MAX;
            for _ in 0..3 {
                let t = Instant::now();
                run(Command::new(&bin).arg("--repo").arg(&repo_dir).args(args))?;
                best = best.min(t.elapsed().as_secs_f64());
            }
            Ok(best)
        };
        let cases: Vec<(&str, Vec<String>)> = vec![
            (
                "log of a 240-commit branch",
                vec!["log".into(), "--oneline".into(), self.branch("stacks-main")],
            ),
            (
                "log -- path on it",
                vec![
                    "log".into(),
                    "--oneline".into(),
                    self.branch("stacks-main"),
                    "--".into(),
                    "bench_stacks/s07".into(),
                ],
            ),
            (
                "diff between branch tips (5,000+ files)",
                vec![
                    "log".into(),
                    "-1".into(),
                    "--stat".into(),
                    self.branch("wide"),
                ],
            ),
            (
                "show a 2,000-file commit (--stat)",
                vec![
                    "show".into(),
                    "--stat".into(),
                    format!("{}~2", self.branch("wide")),
                ],
            ),
        ];
        for (what, args) in cases {
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            let secs = pail(&a)?;
            self.push(Row {
                scenario: "reads".into(),
                system: "pail".into(),
                what: what.into(),
                secs,
                ops: 1,
                ..Default::default()
            });
        }
        // Switching a working copy between two branches that differ by 2,000+ files.
        let wt = work.join("reads-wt");
        if wt.exists() {
            std::fs::remove_dir_all(&wt)?;
        }
        let t = Instant::now();
        run(Command::new(&bin)
            .args(["clone", store])
            .arg(&wt)
            .args(["-b", &self.branch("wide")]))?;
        self.push(Row {
            scenario: "reads".into(),
            system: "pail".into(),
            what: "clone a working copy (33k files)".into(),
            secs: t.elapsed().as_secs_f64(),
            ops: 1,
            ..Default::default()
        });
        let t = Instant::now();
        run(Command::new(&bin)
            .current_dir(&wt)
            .args(["switch", &self.branch("rename")]))?;
        self.push(Row {
            scenario: "reads".into(),
            system: "pail".into(),
            what: "switch to a branch 9,000+ files away".into(),
            secs: t.elapsed().as_secs_f64(),
            ops: 1,
            ..Default::default()
        });
        let t = Instant::now();
        run(Command::new(&bin)
            .current_dir(&wt)
            .args(["worktree", "add", "--no-seed"])
            .arg(work.join("reads-wt2")))?;
        self.push(Row {
            scenario: "reads".into(),
            system: "pail".into(),
            what: "worktree add (clones from the local store)".into(),
            secs: t.elapsed().as_secs_f64(),
            ops: 1,
            ..Default::default()
        });
        let _ = std::fs::remove_dir_all(work.join("reads-wt2"));
        let _ = std::fs::remove_dir_all(&wt);
        Ok(())
    }
}

fn run(c: &mut Command) -> Result<Vec<u8>> {
    let out = c.output().with_context(|| format!("running {c:?}"))?;
    if !out.status.success() {
        bail!("{c:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out.stdout)
}

/// The top-level-or-second-level directory whose file count is closest above `want`.
fn largest_dir(files: &[Entry], want: usize) -> (String, usize) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for e in files {
        let parts: Vec<&str> = e.path.split('/').collect();
        for depth in 1..parts.len().min(4) {
            *counts.entry(parts[..depth].join("/")).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().filter(|(_, n)| *n >= want).collect();
    v.sort_by_key(|(d, n)| (*n, d.clone()));
    v.into_iter().next().unwrap_or(("services".into(), 0))
}

// ---------------------------------------------------------------------- Git baseline

struct GitSide {
    work: PathBuf,
    remote: PathBuf,
    base: String,
}

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    run(Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Bench")
        .env("GIT_AUTHOR_EMAIL", "b@x")
        .env("GIT_COMMITTER_NAME", "Bench")
        .env("GIT_COMMITTER_EMAIL", "b@x"))
}

fn write_text(root: &Path, path: &str, rng: &mut StdRng, len: usize) -> Result<()> {
    let p = root.join(path);
    std::fs::create_dir_all(p.parent().unwrap())?;
    // Picked paths can be symlinks (even to directories); replace them with a file.
    if p.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        std::fs::remove_file(&p)?;
    }
    std::fs::write(p, text(rng, len))?;
    Ok(())
}

impl GitSide {
    /// A bare remote sharing the mirror's objects, with `main` at the base commit.
    fn setup(mirror: &Path, base: &str, work: &Path) -> Result<GitSide> {
        let remote = work.join("git-remote.git");
        if !remote.exists() {
            run(Command::new("git")
                .args(["init", "-q", "--bare"])
                .arg(&remote))?;
            let objects = std::fs::canonicalize(mirror)?.join("objects");
            std::fs::write(
                remote.join("objects/info/alternates"),
                format!("{}\n", objects.display()),
            )?;
            // The mirror may itself borrow objects; chain its alternates.
            if let Ok(more) = std::fs::read_to_string(objects.join("info/alternates")) {
                let mut all = std::fs::read_to_string(remote.join("objects/info/alternates"))?;
                all.push_str(&more);
                std::fs::write(remote.join("objects/info/alternates"), all)?;
            }
        }
        git(&remote, &["update-ref", "refs/heads/main", base])?;
        Ok(GitSide {
            work: work.to_path_buf(),
            remote,
            base: base.to_string(),
        })
    }

    /// A working copy on branch `b` (created at the base on the remote).
    fn checkout(&self, name: &str, b: &str) -> Result<PathBuf> {
        git(
            &self.remote,
            &["update-ref", &format!("refs/heads/{b}"), &self.base],
        )?;
        let dir = self.work.join(format!("git-{name}"));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        run(Command::new("git")
            .args(["clone", "-q", "--shared", "-b", b])
            .arg(&self.remote)
            .arg(&dir))?;
        Ok(dir)
    }

    fn commit_push(dir: &Path, b: &str, msg: &str) -> Result<u32> {
        git(dir, &["add", "-A"])?;
        git(dir, &["commit", "-q", "-m", msg])?;
        let mut tries = 0;
        loop {
            tries += 1;
            if git(dir, &["push", "-q", "origin", &format!("HEAD:{b}")]).is_ok() {
                return Ok(tries);
            }
            git(dir, &["pull", "-q", "--rebase", "origin", b])?;
        }
    }

    fn wc(&self, files: &[Entry]) -> Result<Row> {
        let dir = self.checkout("wc", "bench-wc")?;
        let mut rng = StdRng::seed_from_u64(8);
        let (mut add, mut commit, mut push, mut lat) = (vec![], vec![], vec![], vec![]);
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        for i in 0..50 {
            let n = rng.random_range(1..=3);
            for _ in 0..n {
                let p = &files[rng.random_range(0..files.len())].path;
                let len = rng.random_range(200..6000);
                write_text(&dir, p, &mut rng, len)?;
            }
            let t = Instant::now();
            git(&dir, &["add", "-A"])?;
            let a = ms(t);
            let t = Instant::now();
            git(&dir, &["commit", "-q", "-m", &format!("wc {i}")])?;
            let c = ms(t);
            let t = Instant::now();
            git(&dir, &["push", "-q", "origin", "HEAD:bench-wc"])?;
            let p = ms(t);
            add.push(a);
            commit.push(c);
            push.push(p);
            lat.push(a + c + p);
        }
        let _ = std::fs::remove_dir_all(&dir);
        Ok(Row {
            scenario: "wc".into(),
            system: "git".into(),
            what: "50 x edit, add -A, commit, push".into(),
            secs: start.elapsed().as_secs_f64(),
            ops: 50,
            p50_ms: pct(&mut lat.clone(), 0.5),
            p95_ms: pct(&mut lat, 0.95),
            note: format!(
                "add p50 {:.0} ms, commit p50 {:.0} ms, push p50 {:.0} ms",
                pct(&mut add, 0.5).unwrap(),
                pct(&mut commit, 0.5).unwrap(),
                pct(&mut push, 0.5).unwrap()
            ),
            ..Default::default()
        })
    }

    fn small(&self, files: &[Entry]) -> Result<Row> {
        let dir = self.checkout("small", "bench-small")?;
        let mut rng = StdRng::seed_from_u64(1);
        let t = Instant::now();
        let mut lat = vec![];
        for i in 0..200 {
            let n = rng.random_range(1..=3);
            let paths: Vec<String> = (0..n)
                .map(|_| files[rng.random_range(0..files.len())].path.clone())
                .collect();
            let s = Instant::now();
            for p in &paths {
                let len = rng.random_range(200..6000);
                write_text(&dir, p, &mut rng, len)?;
            }
            Self::commit_push(&dir, "bench-small", &format!("small {i}"))?;
            lat.push(s.elapsed().as_secs_f64() * 1000.0);
        }
        let _ = std::fs::remove_dir_all(&dir);
        Ok(Row {
            scenario: "small".into(),
            system: "git".into(),
            what: "200 commits of 1-3 files, commit + push each".into(),
            secs: t.elapsed().as_secs_f64(),
            ops: 200,
            p50_ms: pct(&mut lat.clone(), 0.5),
            p95_ms: pct(&mut lat, 0.95),
            ..Default::default()
        })
    }

    fn wide(
        &self,
        paths: &[String],
        new: &[String],
        del_dir: &str,
        ren_dir: &str,
    ) -> Result<Vec<Row>> {
        let mut rows = vec![];
        let dir = self.checkout("wide", "bench-wide")?;
        let mut rng = StdRng::seed_from_u64(2);
        let mut timed =
            |what: String, f: &mut dyn FnMut(&mut StdRng) -> Result<()>| -> Result<()> {
                let t = Instant::now();
                f(&mut rng)?;
                rows.push(Row {
                    scenario: "wide".into(),
                    system: "git".into(),
                    what,
                    secs: t.elapsed().as_secs_f64(),
                    ops: 1,
                    ..Default::default()
                });
                Ok(())
            };
        timed("edit 2,000 files across the tree".into(), &mut |rng| {
            for p in paths {
                let len = rng.random_range(200..6000);
                write_text(&dir, p, rng, len)?;
            }
            Self::commit_push(&dir, "bench-wide", "edit 2000").map(|_| ())
        })?;
        timed("add 5,000 files".into(), &mut |rng| {
            for p in new {
                let len = rng.random_range(200..6000);
                write_text(&dir, p, rng, len)?;
            }
            Self::commit_push(&dir, "bench-wide", "add 5000").map(|_| ())
        })?;
        timed(format!("delete {del_dir}/"), &mut |_| {
            std::fs::remove_dir_all(dir.join(del_dir))?;
            Self::commit_push(&dir, "bench-wide", "delete").map(|_| ())
        })?;
        let rdir = self.checkout("rename", "bench-rename")?;
        timed(format!("rename {ren_dir}/"), &mut |_| {
            git(&rdir, &["mv", ren_dir, &format!("{ren_dir}_renamed")])?;
            Self::commit_push(&rdir, "bench-rename", "rename").map(|_| ())
        })?;
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&rdir);
        Ok(rows)
    }

    fn writers(&self, w: usize, per: usize) -> Result<Row> {
        let b = format!("bench-writers{w}");
        let dirs: Vec<PathBuf> = (0..w)
            .map(|i| self.checkout(&format!("w{w}-{i}"), &b))
            .collect::<Result<_>>()?;
        let cleanup = dirs.clone();
        let t = Instant::now();
        let handles: Vec<_> = dirs
            .into_iter()
            .enumerate()
            .map(|(i, dir)| {
                let b = b.clone();
                std::thread::spawn(move || -> Result<(Vec<f64>, u32)> {
                    let mut rng = StdRng::seed_from_u64(100 + i as u64);
                    let mut lat = vec![];
                    let mut tries = 0;
                    for j in 0..per {
                        let s = Instant::now();
                        write_text(
                            &dir,
                            &format!("bench_writers/w{i:02}/f{j:03}.txt"),
                            &mut rng,
                            2000,
                        )?;
                        tries += GitSide::commit_push(&dir, &b, "w")?;
                        lat.push(s.elapsed().as_secs_f64() * 1000.0);
                    }
                    Ok((lat, tries))
                })
            })
            .collect();
        let mut lat = vec![];
        let mut tries = 0;
        for h in handles {
            let (l, t) = h.join().expect("writer thread")?;
            lat.extend(l);
            tries += t;
        }
        let n = (w * per) as u64;
        let secs = t.elapsed().as_secs_f64();
        for d in cleanup {
            let _ = std::fs::remove_dir_all(d);
        }
        Ok(Row {
            scenario: "writers".into(),
            system: "git".into(),
            what: format!("{w} writer(s) x {per} commits, push with pull --rebase retries"),
            secs,
            ops: n,
            p50_ms: pct(&mut lat.clone(), 0.5),
            p95_ms: pct(&mut lat, 0.95),
            note: format!(
                "{:.1} commits/s, {:.2} pushes/commit",
                n as f64 / secs,
                tries as f64 / n as f64
            ),
            ..Default::default()
        })
    }
}

// ---------------------------------------------------------------------- setup

/// Import the base commit's files into the store as branch `bench/base` (once).
async fn import_base(repo: &Repo, g: &Git, base: &str) -> Result<Hash> {
    if let Some((h, _)) = repo.read_ref("bench/base").await? {
        return Ok(h);
    }
    println!("importing the base snapshot {}…", &base[..10]);
    let t = Instant::now();
    let items = g.ls_tree(base)?;
    let mut delta: Delta = BTreeMap::new();
    for chunk in items.chunks(4000) {
        let inputs: Vec<FileInput> = chunk
            .iter()
            .map(|it| {
                Ok(FileInput {
                    path: it.path.clone(),
                    kind: kind_for_mode(&it.mode)?,
                    data: Bytes::from(g.cat(&it.oid)?),
                })
            })
            .collect::<Result<_>>()?;
        for e in repo.write_files(&inputs, &HashMap::new()).await? {
            delta.insert(e.path.clone(), Some(e));
        }
    }
    let out = repo
        .commit(req("bench/base", None, delta, "bench base"))
        .await?;
    println!(
        "  imported {} files in {:.1}s",
        out.files,
        t.elapsed().as_secs_f64()
    );
    Ok(out.commit)
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = Args::parse();
    std::fs::create_dir_all(&a.work)?;
    let g = Git::open(&a.git_dir)?;
    let base_git = match &a.base {
        Some(b) => g.rev_parse(b)?,
        None => g
            .rev_parse("refs/remotes/origin/main")
            .or_else(|_| g.rev_parse("HEAD"))?,
    };
    let store = pail::rev::store_url(&a.store)?;
    let repo = Arc::new(Repo::with_store(Store::open(&store)?, None));
    let base = import_base(&repo, &g, &base_git).await?;
    let files = repo.entries(repo.commit_obj(&base).await?.root).await?;
    let git_side = if a.no_git {
        None
    } else {
        Some(GitSide::setup(&a.git_dir, &base_git, &a.work)?)
    };
    let mut b = Bench {
        repo,
        base,
        files,
        rows: vec![],
        git: git_side,
    };
    let want = |s: &str| a.only.is_empty() || a.only.iter().any(|o| o == s);
    println!(
        "store {store}, base {} ({} files)",
        &base_git[..10],
        b.files.len()
    );
    if want("small") {
        b.small().await?;
    }
    if want("wide") {
        b.wide().await?;
    }
    if want("writers") {
        b.writers().await?;
    }
    if want("hot") {
        b.hot().await?;
    }
    if want("stacks") {
        b.stacks().await?;
    }
    if want("bigfile") {
        b.bigfile().await?;
    }
    if want("reads") {
        b.reads(&store, &a.work).await?;
    }
    if want("wc") {
        b.wc(&store, &a.work).await?;
    }
    if let Some(out) = &a.out {
        let rows: Vec<Value> = b.rows.iter().map(Row::json).collect();
        std::fs::write(
            out,
            serde_json::to_vec_pretty(
                &json!({"store": store, "base": base_git, "files": b.files.len(), "rows": rows}),
            )?,
        )?;
        println!("results in {}", out.display());
    }
    Ok(())
}
