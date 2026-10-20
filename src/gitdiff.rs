//! Git's diff output (`--name-only`, `--name-status`, `--raw`, `--numstat`, `--stat`,
//! `--shortstat`, `-p`) over pail tree changes, following diff.c and xdiff's emitter.
//! Line diffs come from the xdiff port (Myers with the indent heuristic, Git's default).

use std::collections::HashMap;
use std::io::Write;

use anyhow::Result;
use bytes::Bytes;

use crate::gitlog::Shown;
use crate::gitsrc::{mode_for_kind, object_id};
use crate::hash::Hash;
use crate::manifest::{Change, Entry, Kind};
use crate::rev::Ctx;

#[derive(Clone, Debug)]
pub struct DiffOpts {
    pub name_only: bool,
    pub name_status: bool,
    pub raw: bool,
    pub stat: bool,
    pub stat_width: Option<usize>,
    pub stat_name_width: Option<usize>,
    pub stat_graph_width: Option<usize>,
    pub stat_count: Option<usize>,
    pub numstat: bool,
    pub shortstat: bool,
    pub summary: bool,
    pub patch: bool,
    pub no_patch: bool,
    pub context: usize,
    pub full_index: bool,
    pub renames: bool,
    pub rename_score: u32,
}

impl Default for DiffOpts {
    fn default() -> Self {
        DiffOpts {
            name_only: false,
            name_status: false,
            raw: false,
            stat: false,
            stat_width: None,
            stat_name_width: None,
            stat_graph_width: None,
            stat_count: None,
            numstat: false,
            shortstat: false,
            summary: false,
            patch: false,
            no_patch: false,
            context: 3,
            full_index: false,
            renames: true,
            rename_score: 50,
        }
    }
}

impl DiffOpts {
    /// Consume one diff option; false if `a` isn't one.
    pub fn parse_arg(&mut self, a: &str) -> Result<bool> {
        match a {
            "--name-only" => self.name_only = true,
            "--name-status" => self.name_status = true,
            "--raw" => self.raw = true,
            "--stat" => self.stat = true,
            "--numstat" => self.numstat = true,
            "--shortstat" => self.shortstat = true,
            "--summary" => self.summary = true,
            "-p" | "-u" | "--patch" => self.patch = true,
            "--patch-with-stat" => {
                self.patch = true;
                self.stat = true;
            }
            "--patch-with-raw" => {
                self.patch = true;
                self.raw = true;
            }
            "-s" | "--no-patch" => self.no_patch = true,
            "--full-index" => self.full_index = true,
            "--no-renames" => self.renames = false,
            "-M" | "--find-renames" => self.renames = true,
            _ => {
                if let Some(v) = a.strip_prefix("--stat=") {
                    self.stat = true;
                    let mut it = v.split(',');
                    self.stat_width = it.next().and_then(|s| s.parse().ok());
                    self.stat_name_width = it.next().and_then(|s| s.parse().ok());
                    self.stat_count = it.next().and_then(|s| s.parse().ok());
                } else if let Some(v) = a.strip_prefix("--stat-width=") {
                    self.stat = true;
                    self.stat_width = v.parse().ok();
                } else if let Some(v) = a.strip_prefix("--stat-name-width=") {
                    self.stat = true;
                    self.stat_name_width = v.parse().ok();
                } else if let Some(v) = a.strip_prefix("--stat-graph-width=") {
                    self.stat = true;
                    self.stat_graph_width = v.parse().ok();
                } else if let Some(v) = a.strip_prefix("--stat-count=") {
                    self.stat = true;
                    self.stat_count = v.parse().ok();
                } else if let Some(v) = a.strip_prefix("-U").or(a.strip_prefix("--unified=")) {
                    self.patch = true;
                    self.context = v.parse().map_err(|_| crate::gitlog::Fatal(format!("bad context {v}")))?;
                } else if let Some(v) = a.strip_prefix("-M").or(a.strip_prefix("--find-renames=")) {
                    self.renames = true;
                    self.rename_score = v.trim_end_matches('%').parse().unwrap_or(50);
                } else {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    pub fn any_output(&self) -> bool {
        !self.no_patch && (self.name_only || self.name_status || self.raw || self.stat || self.numstat || self.shortstat || self.summary || self.patch)
    }

    pub fn stat_and_patch(&self) -> bool {
        !self.no_patch && self.patch && self.stat
    }
}

/// One file pair after rename detection.
#[derive(Clone, Debug)]
pub struct FilePair {
    pub old: Option<Entry>,
    pub new: Option<Entry>,
    /// Similarity percentage for a rename.
    pub rename: Option<u32>,
}

impl FilePair {
    fn old_path(&self) -> &str {
        self.old.as_ref().or(self.new.as_ref()).map(|e| e.path.as_str()).unwrap()
    }
    fn new_path(&self) -> &str {
        self.new.as_ref().or(self.old.as_ref()).map(|e| e.path.as_str()).unwrap()
    }
    fn status(&self) -> String {
        match (&self.old, &self.new, self.rename) {
            (_, _, Some(s)) => format!("R{s:03}"),
            (None, Some(_), _) => "A".into(),
            (Some(_), None, _) => "D".into(),
            (Some(a), Some(b), _) if (a.kind == Kind::Symlink) != (b.kind == Kind::Symlink) => "T".into(),
            _ => "M".into(),
        }
    }
}

/// Git's C-style quoting of paths with control characters, quotes, backslashes or non-ASCII
/// bytes (core.quotePath).
pub fn quote_path(p: &str) -> String {
    let needs = p.bytes().any(|b| b < 0x20 || b >= 0x7f || b == b'"' || b == b'\\');
    if !needs {
        return p.to_string();
    }
    let mut out = String::from("\"");
    for b in p.bytes() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0b => out.push_str("\\v"),
            0x0c => out.push_str("\\f"),
            b if b < 0x20 || b >= 0x7f => out.push_str(&format!("\\{b:03o}")),
            b => out.push(b as char),
        }
    }
    out.push('"');
    out
}

/// `prefix/name` quoted as one unit, as diff headers do.
fn quote_prefixed(prefix: &str, p: &str) -> String {
    let q = quote_path(p);
    if q.starts_with('"') { format!("\"{prefix}{}", &q[1..]) } else { format!("{prefix}{p}") }
}

const MAX_SCORE: u64 = 60000;

/// Git's diffcore-delta: count bytes of `src` that appear in `dst`, by 64-byte-or-newline
/// chunks hashed into a multiset.
fn spanhash(buf: &[u8]) -> HashMap<u32, u64> {
    const HASHBASE: u32 = 107927;
    let mut m: HashMap<u32, u64> = HashMap::new();
    let is_text = !buf.iter().take(8000).any(|&b| b == 0);
    let mut i = 0;
    while i < buf.len() {
        let mut accum1: u32 = 0;
        let mut accum2: u32 = 0;
        let mut n = 0u64;
        while i < buf.len() {
            let c = buf[i] as u32;
            let old1 = accum1;
            i += 1;
            // Ignore CR in CRLF sequences for text.
            if is_text && c == u32::from(b'\r') && i < buf.len() && buf[i] == b'\n' {
                continue;
            }
            accum1 = (accum1 << 7) ^ (accum2 >> 25);
            accum2 = (accum2 << 7) ^ (old1 >> 25);
            accum1 = accum1.wrapping_add(c);
            n += 1;
            if n >= 64 || c == u32::from(b'\n') {
                break;
            }
        }
        let h = accum1.wrapping_add(accum2.wrapping_mul(0x61)) % HASHBASE;
        *m.entry(h).or_default() += n;
    }
    m
}

/// Git's `estimate_similarity` score (0..MAX_SCORE) for a rename candidate.
fn similarity(src: &[u8], dst: &[u8], min_score: u64) -> u64 {
    let (ss, ds) = (src.len() as u64, dst.len() as u64);
    let max_size = ss.max(ds);
    let delta_size = ss.abs_diff(ds);
    if max_size * (MAX_SCORE - min_score) < delta_size * MAX_SCORE {
        return 0;
    }
    if ss == 0 || ds == 0 {
        return 0;
    }
    let a = spanhash(src);
    let b = spanhash(dst);
    let mut copied = 0u64;
    for (h, n) in &a {
        if let Some(m) = b.get(h) {
            copied += (*n).min(*m);
        }
    }
    if max_size == 0 { MAX_SCORE } else { copied * MAX_SCORE / max_size }
}

pub struct DiffPrinter<'a> {
    ctx: &'a Ctx,
    o: &'a DiffOpts,
    color: bool,
    abbrev: usize,
    /// Git blob ids by pail content hash.
    blob_ids: std::sync::Mutex<HashMap<Hash, String>>,
    /// Contents fetched ahead for the commits about to print.
    prefetched: std::sync::Mutex<HashMap<Hash, Bytes>>,
}

impl<'a> DiffPrinter<'a> {
    pub async fn new(ctx: &'a Ctx, o: &'a DiffOpts, color: bool, abbrev: usize) -> Result<DiffPrinter<'a>> {
        Ok(DiffPrinter { ctx, o, color, abbrev, blob_ids: Default::default(), prefetched: Default::default() })
    }

    /// Entries whose contents the requested output needs: all of them for stats, patches and
    /// raw ids; only additions and deletions (rename candidates) for name-only listings.
    fn wanted<'c>(&self, cs: &'c [Change]) -> Vec<&'c Entry> {
        if !self.o.any_output() {
            return vec![];
        }
        let full = self.o.stat || self.o.numstat || self.o.shortstat || (self.o.patch && !self.o.no_patch) || self.o.raw;
        if full {
            return cs.iter().flat_map(|c| [&c.old, &c.new]).flatten().collect();
        }
        if !self.o.renames {
            return vec![];
        }
        let adds = cs.iter().any(|c| c.old.is_none());
        let dels = cs.iter().any(|c| c.new.is_none());
        if !(adds && dels) {
            return vec![];
        }
        cs.iter().filter(|c| c.old.is_none() || c.new.is_none()).flat_map(|c| [&c.old, &c.new]).flatten().collect()
    }

    /// Give old versions named only by hash (from change lists) a location: the local cache
    /// serves them by hash; otherwise they are looked up in the parent's manifest.
    async fn locate(&self, parent: Option<Hash>, entries: Vec<Entry>) -> Result<Vec<Entry>> {
        let blobs = self.ctx.repo.blobs.as_ref();
        let mut out = Vec::with_capacity(entries.len());
        let mut root = None;
        for e in entries {
            if !matches!(e.loc, crate::manifest::Loc::Unresolved) || blobs.is_some_and(|b| b.has(&e.hash)) {
                out.push(e);
                continue;
            }
            if root.is_none() {
                root = Some(self.ctx.repo.root_of(parent).await?);
            }
            match self.ctx.repo.get_entry(root.unwrap(), &e.path).await? {
                Some(found) if found.hash == e.hash => out.push(found),
                _ => anyhow::bail!("{}: old version {} not found in the parent commit", e.path, e.hash.short()),
            }
        }
        Ok(out)
    }

    /// Fetch, in one batched read, the contents the next commits will need (replacing what
    /// was fetched for the previous batch).
    pub async fn prefetch(&self, shown: &[Shown]) -> Result<()> {
        let mut want: Vec<Entry> = vec![];
        let mut seen = std::collections::HashSet::new();
        for s in shown {
            let Some(cs) = s.changes.as_deref() else { continue };
            let mine: Vec<Entry> = self.wanted(cs).into_iter().filter(|e| seen.insert(e.hash)).cloned().collect();
            want.extend(self.locate(s.commit.parent, mine).await?);
        }
        let data = self.ctx.repo.read_files(&want).await?;
        *self.prefetched.lock().unwrap() = want.iter().map(|e| e.hash).zip(data).collect();
        Ok(())
    }

    /// Contents of every entry in the changes, by content hash.
    pub async fn load(&self, parent: Option<Hash>, changes: &[Change]) -> Result<HashMap<Hash, Bytes>> {
        let mut out = HashMap::new();
        let mut want: Vec<Entry> = vec![];
        {
            let pre = self.prefetched.lock().unwrap();
            for c in changes {
                for e in [&c.old, &c.new].into_iter().flatten() {
                    if out.contains_key(&e.hash) || want.iter().any(|w: &Entry| w.hash == e.hash) {
                        continue;
                    }
                    match pre.get(&e.hash) {
                        Some(d) => {
                            out.insert(e.hash, d.clone());
                        }
                        None => want.push(e.clone()),
                    }
                }
            }
        }
        let want = self.locate(parent, want).await?;
        let data = self.ctx.repo.read_files(&want).await?;
        out.extend(want.iter().map(|e| e.hash).zip(data));
        Ok(out)
    }

    /// Pair deletions with additions as renames: exact content first, then by similarity.
    pub fn pairs(&self, changes: &[Change], data: &HashMap<Hash, Bytes>) -> Vec<FilePair> {
        let mut pairs: Vec<FilePair> = vec![];
        let mut adds: Vec<&Entry> = vec![];
        let mut dels: Vec<&Entry> = vec![];
        for c in changes {
            match (&c.old, &c.new) {
                (Some(a), Some(b)) if (a.kind == Kind::Symlink) != (b.kind == Kind::Symlink) && self.o.patch => {
                    // A type change shows as a deletion plus an addition.
                    pairs.push(FilePair { old: Some(a.clone()), new: None, rename: None });
                    pairs.push(FilePair { old: None, new: Some(b.clone()), rename: None });
                }
                (Some(a), Some(b)) => pairs.push(FilePair { old: Some(a.clone()), new: Some(b.clone()), rename: None }),
                (None, Some(b)) => adds.push(b),
                (Some(a), None) => dels.push(a),
                (None, None) => {}
            }
        }
        let mut used_del = vec![false; dels.len()];
        let mut matched_add = vec![None; adds.len()];
        if self.o.renames && !adds.is_empty() && !dels.is_empty() {
            // Exact renames: same content. Prefer a source with the same basename.
            for (ai, a) in adds.iter().enumerate() {
                if a.kind == Kind::Symlink {
                    continue;
                }
                let base = a.path.rsplit('/').next();
                let cands: Vec<usize> = (0..dels.len()).filter(|&d| !used_del[d] && dels[d].hash == a.hash && dels[d].kind != Kind::Symlink).collect();
                let pick = cands.iter().copied().find(|&d| dels[d].path.rsplit('/').next() == base).or(cands.first().copied());
                if let Some(d) = pick {
                    used_del[d] = true;
                    matched_add[ai] = Some((d, 100u32));
                }
            }
            // Inexact renames (Git's estimate_similarity), best score first.
            let min_score = self.o.rename_score as u64 * MAX_SCORE / 100;
            let rest_adds: Vec<usize> = (0..adds.len()).filter(|&i| matched_add[i].is_none() && adds[i].kind != Kind::Symlink).collect();
            let rest_dels: Vec<usize> = (0..dels.len()).filter(|&i| !used_del[i] && dels[i].kind != Kind::Symlink).collect();
            if !rest_adds.is_empty() && !rest_dels.is_empty() && rest_adds.len() * rest_dels.len() <= 1000 * 1000 {
                let mut cands: Vec<(u64, usize, usize)> = vec![];
                for &ai in &rest_adds {
                    let Some(dst) = data.get(&adds[ai].hash) else { continue };
                    for &di in &rest_dels {
                        let Some(src) = data.get(&dels[di].hash) else { continue };
                        let mut score = similarity(src, dst, min_score);
                        if score >= min_score {
                            if dels[di].path.rsplit('/').next() == adds[ai].path.rsplit('/').next() {
                                score += 1; // basename tie-break, as Git's name score
                            }
                            cands.push((score, ai, di));
                        }
                    }
                }
                cands.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
                for (score, ai, di) in cands {
                    if matched_add[ai].is_some() || used_del[di] {
                        continue;
                    }
                    used_del[di] = true;
                    let pct = (score.min(MAX_SCORE) * 100 / MAX_SCORE) as u32;
                    matched_add[ai] = Some((di, pct));
                }
            }
        }
        for (ai, a) in adds.iter().enumerate() {
            match matched_add[ai] {
                Some((d, s)) => pairs.push(FilePair { old: Some(dels[d].clone()), new: Some((*a).clone()), rename: Some(s) }),
                None => pairs.push(FilePair { old: None, new: Some((*a).clone()), rename: None }),
            }
        }
        for (di, d) in dels.iter().enumerate() {
            if !used_del[di] {
                pairs.push(FilePair { old: Some((*d).clone()), new: None, rename: None });
            }
        }
        // Git's queue order: by destination path (renames sort by their new name).
        pairs.sort_by(|a, b| a.new_path().cmp(b.new_path()).then(a.new.is_some().cmp(&b.new.is_some())));
        pairs
    }

    fn blob_id(&self, e: &Entry, data: &HashMap<Hash, Bytes>) -> String {
        let mut m = self.blob_ids.lock().unwrap();
        m.entry(e.hash).or_insert_with(|| object_id("blob", data.get(&e.hash).map(|b| &b[..]).unwrap_or(b""))).clone()
    }

    fn abbrev_id(&self, id: &str) -> String {
        if self.o.full_index { id.to_string() } else { id[..self.abbrev.min(id.len())].to_string() }
    }

    pub async fn print(&self, s: &Shown, changes: &[Change], out: &mut dyn Write) -> Result<()> {
        let needed: Vec<Change> = {
            let w: std::collections::HashSet<Hash> = self.wanted(changes).iter().map(|e| e.hash).collect();
            changes
                .iter()
                .map(|c| Change {
                    path: c.path.clone(),
                    old: c.old.clone().filter(|e| w.contains(&e.hash)),
                    new: c.new.clone().filter(|e| w.contains(&e.hash)),
                })
                .collect()
        };
        let data = self.load(s.commit.parent, &needed).await?;
        let pairs = if self.o.renames {
            self.pairs(changes, &data)
        } else {
            let mut o2 = self.o.clone();
            o2.renames = false;
            let p = DiffPrinter { ctx: self.ctx, o: &o2, color: self.color, abbrev: self.abbrev, blob_ids: Default::default(), prefetched: Default::default() };
            p.pairs(changes, &data)
        };
        let mut sep = false;
        if self.o.raw || self.o.name_only || self.o.name_status {
            for p in &pairs {
                if self.o.raw {
                    let mode = |e: &Option<Entry>| e.as_ref().map(|e| mode_for_kind(e.kind)).unwrap_or("000000");
                    let id = |e: &Option<Entry>| match e {
                        Some(e) => self.abbrev_id(&self.blob_id(e, &data)),
                        None => "0".repeat(if self.o.full_index { 40 } else { self.abbrev }),
                    };
                    let st = p.status();
                    let names = if p.rename.is_some() {
                        format!("{}\t{}", quote_path(p.old_path()), quote_path(p.new_path()))
                    } else {
                        quote_path(p.new_path())
                    };
                    writeln!(out, ":{} {} {} {} {st}\t{names}", mode(&p.old), mode(&p.new), id(&p.old), id(&p.new))?;
                } else if self.o.name_status {
                    if p.rename.is_some() {
                        writeln!(out, "{}\t{}\t{}", p.status(), quote_path(p.old_path()), quote_path(p.new_path()))?;
                    } else {
                        writeln!(out, "{}\t{}", p.status(), quote_path(p.new_path()))?;
                    }
                } else {
                    writeln!(out, "{}", quote_path(p.new_path()))?;
                }
            }
            sep = true;
        }
        let needs_counts = self.o.stat || self.o.numstat || self.o.shortstat;
        let counts: Vec<Counts> = if needs_counts { pairs.iter().map(|p| counts(p, &data)).collect() } else { vec![] };
        if self.o.numstat {
            for (p, c) in pairs.iter().zip(&counts) {
                let name = stat_name(p);
                if c.binary {
                    writeln!(out, "-\t-\t{name}")?;
                } else {
                    writeln!(out, "{}\t{}\t{name}", c.added, c.deleted)?;
                }
            }
            sep = true;
        }
        if self.o.stat {
            self.show_stats(&pairs, &counts, out)?;
            sep = true;
        } else if self.o.shortstat {
            let files = pairs.len();
            let adds: u64 = counts.iter().filter(|c| !c.binary).map(|c| c.added).sum();
            let dels: u64 = counts.iter().filter(|c| !c.binary).map(|c| c.deleted).sum();
            writeln!(out, "{}", stat_summary(files, adds, dels))?;
            sep = true;
        }
        if self.o.summary {
            for p in &pairs {
                match (&p.old, &p.new, p.rename) {
                    (Some(_), Some(_), Some(s)) => writeln!(out, " rename {} ({s}%)", rename_name(p.old_path(), p.new_path()))?,
                    (None, Some(n), _) => writeln!(out, " create mode {} {}", mode_for_kind(n.kind), quote_path(&n.path))?,
                    (Some(o), None, _) => writeln!(out, " delete mode {} {}", mode_for_kind(o.kind), quote_path(&o.path))?,
                    (Some(o), Some(n), _) if o.kind != n.kind && o.kind != Kind::Symlink && n.kind != Kind::Symlink => {
                        writeln!(out, " mode change {} => {} {}", mode_for_kind(o.kind), mode_for_kind(n.kind), quote_path(&n.path))?
                    }
                    _ => {}
                }
            }
            sep = true;
        }
        if self.o.patch && !self.o.no_patch {
            if sep {
                out.write_all(b"\n")?;
            }
            for p in &pairs {
                self.patch(p, &data, out)?;
            }
        }
        Ok(())
    }

    fn show_stats(&self, pairs: &[FilePair], counts: &[Counts], out: &mut dyn Write) -> Result<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let width_of = |s: &str| unicode_width::UnicodeWidthStr::width(s);
        let names: Vec<String> = pairs.iter().map(stat_name).collect();
        let count = self.o.stat_count.unwrap_or(pairs.len()).min(pairs.len());
        let mut max_len = 0usize;
        let mut max_change = 0u64;
        let mut bin_width = 0usize;
        let mut number_width = 0usize;
        for i in 0..count {
            let c = &counts[i];
            max_len = max_len.max(width_of(&names[i]));
            if c.binary {
                let w = 14 + decimal_width(c.added) + decimal_width(c.deleted);
                bin_width = bin_width.max(w);
                number_width = 3;
                continue;
            }
            max_change = max_change.max(c.added + c.deleted);
        }
        let mut width = self.o.stat_width.unwrap_or_else(term_columns) as i64;
        number_width = number_width.max(decimal_width(max_change));
        let nw = number_width as i64;
        if width < 16 + 6 + nw {
            width = 16 + 6 + nw;
        }
        let mut graph_width: i64 = if max_change as i64 + 4 > bin_width as i64 { max_change as i64 } else { bin_width as i64 - 4 };
        if let Some(g) = self.o.stat_graph_width {
            if (g as i64) < graph_width {
                graph_width = g as i64;
            }
        }
        let mut name_width: i64 = match self.o.stat_name_width {
            Some(n) if n > 0 && n < max_len => n as i64,
            _ => max_len as i64,
        };
        if name_width + nw + 6 + graph_width > width {
            if graph_width > width * 3 / 8 - nw - 6 {
                graph_width = width * 3 / 8 - nw - 6;
                if graph_width < 6 {
                    graph_width = 6;
                }
            }
            if let Some(g) = self.o.stat_graph_width {
                if graph_width > g as i64 {
                    graph_width = g as i64;
                }
            }
            if name_width > width - nw - 6 - graph_width {
                name_width = width - nw - 6 - graph_width;
            } else {
                graph_width = width - nw - 6 - name_width;
            }
        }
        let (add_c, del_c, reset) = if self.color { ("\x1b[32m", "\x1b[31m", "\x1b[m") } else { ("", "", "") };
        for i in 0..count {
            let c = &counts[i];
            let mut name: &str = &names[i];
            let mut prefix = "";
            let mut len = name_width;
            let mut name_len = width_of(name) as i64;
            if name_width < name_len {
                // Drop leading characters until the name fits, then start at a '/'.
                prefix = "...";
                len = (len - 3).max(0);
                while name_len > len {
                    let mut it = name.chars();
                    let Some(ch) = it.next() else { break };
                    name_len -= unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as i64;
                    name = it.as_str();
                }
                if let Some(slash) = name.find('/') {
                    name = &name[slash..];
                }
            }
            let padding = (len - width_of(name) as i64).max(0) as usize;
            if c.binary {
                write!(out, " {prefix}{name}{:padding$} | {:>nw$}", "", "Bin", nw = number_width)?;
                if c.added == 0 && c.deleted == 0 {
                    out.write_all(b"\n")?;
                    continue;
                }
                writeln!(out, " {del_c}{}{reset} -> {add_c}{}{reset} bytes", c.deleted, c.added)?;
                continue;
            }
            let (mut add, mut del) = (c.added as i64, c.deleted as i64);
            if graph_width <= max_change as i64 {
                let scale = |it: i64| if it == 0 { 0 } else { 1 + it * (graph_width - 1) / max_change as i64 };
                let mut total = scale(add + del);
                if total < 2 && add > 0 && del > 0 {
                    total = 2;
                }
                if add < del {
                    add = scale(add);
                    del = total - add;
                } else {
                    del = scale(del);
                    add = total - del;
                }
            }
            let n = c.added + c.deleted;
            write!(out, " {prefix}{name}{:padding$} | {:>nw$}{}", "", n, if n > 0 { " " } else { "" }, nw = number_width)?;
            if add > 0 {
                write!(out, "{add_c}{}{reset}", "+".repeat(add as usize))?;
            }
            if del > 0 {
                write!(out, "{del_c}{}{reset}", "-".repeat(del as usize))?;
            }
            out.write_all(b"\n")?;
        }
        if count < pairs.len() {
            writeln!(out, " ...")?;
        }
        let adds: u64 = counts.iter().filter(|c| !c.binary).map(|c| c.added).sum();
        let dels: u64 = counts.iter().filter(|c| !c.binary).map(|c| c.deleted).sum();
        writeln!(out, "{}", stat_summary(pairs.len(), adds, dels))?;
        Ok(())
    }

    fn patch(&self, p: &FilePair, data: &HashMap<Hash, Bytes>, out: &mut dyn Write) -> Result<()> {
        let (a, b) = (p.old_path(), p.new_path());
        writeln!(out, "diff --git {} {}", quote_prefixed("a/", a), quote_prefixed("b/", b))?;
        let empty = Bytes::new();
        let old = p.old.as_ref().map(|e| data.get(&e.hash).unwrap_or(&empty)).unwrap_or(&empty);
        let new = p.new.as_ref().map(|e| data.get(&e.hash).unwrap_or(&empty)).unwrap_or(&empty);
        let mut index_mode = None;
        match (&p.old, &p.new) {
            (None, Some(n)) => writeln!(out, "new file mode {}", mode_for_kind(n.kind))?,
            (Some(o), None) => writeln!(out, "deleted file mode {}", mode_for_kind(o.kind))?,
            (Some(o), Some(n)) => {
                if o.kind != n.kind {
                    writeln!(out, "old mode {}\nnew mode {}", mode_for_kind(o.kind), mode_for_kind(n.kind))?;
                } else {
                    index_mode = Some(mode_for_kind(n.kind));
                }
            }
            _ => {}
        }
        if let Some(s) = p.rename {
            writeln!(out, "similarity index {s}%\nrename from {}\nrename to {}", quote_path(a), quote_path(b))?;
        }
        let same = match (&p.old, &p.new) {
            (Some(o), Some(n)) => o.hash == n.hash,
            _ => false,
        };
        if same {
            return Ok(());
        }
        let id = |e: &Option<Entry>| match e {
            Some(e) => self.abbrev_id(&self.blob_id(e, data)),
            None => "0".repeat(if self.o.full_index { 40 } else { self.abbrev }),
        };
        match index_mode {
            Some(m) => writeln!(out, "index {}..{} {m}", id(&p.old), id(&p.new))?,
            None => writeln!(out, "index {}..{}", id(&p.old), id(&p.new))?,
        }
        let binary = is_binary(old) || is_binary(new);
        let la = if p.old.is_some() { quote_prefixed("a/", a) } else { "/dev/null".into() };
        let lb = if p.new.is_some() { quote_prefixed("b/", b) } else { "/dev/null".into() };
        if binary {
            writeln!(out, "Binary files {la} and {lb} differ")?;
            return Ok(());
        }
        if old.is_empty() && new.is_empty() {
            return Ok(());
        }
        writeln!(out, "--- {la}\n+++ {lb}")?;
        emit_hunks(old, new, self.o.context, self.color, out)
    }
}

fn is_binary(d: &[u8]) -> bool {
    d.iter().take(8000).any(|&b| b == 0)
}

fn decimal_width(n: u64) -> usize {
    n.to_string().len()
}

fn term_columns() -> usize {
    if let Some(c) = std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).filter(|&c: &usize| c > 0) {
        return c;
    }
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    80
}

pub fn stat_summary(files: usize, adds: u64, dels: u64) -> String {
    let mut s = format!(" {files} file{} changed", if files == 1 { "" } else { "s" });
    if adds > 0 || dels == 0 {
        s.push_str(&format!(", {adds} insertion{}(+)", if adds == 1 { "" } else { "s" }));
    }
    if dels > 0 || adds == 0 {
        s.push_str(&format!(", {dels} deletion{}(-)", if dels == 1 { "" } else { "s" }));
    }
    s
}

/// `a/{b => c}/d` for renames: Git's pprint_rename.
fn rename_name(a: &str, b: &str) -> String {
    if quote_path(a) != a || quote_path(b) != b {
        return format!("{} => {}", quote_path(a), quote_path(b));
    }
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let at = |i: usize| ab.get(i).copied().unwrap_or(0);
    let bt = |i: usize| bb.get(i).copied().unwrap_or(0);
    let mut pfx = 0usize;
    let mut i = 0;
    while i < ab.len() && i < bb.len() && ab[i] == bb[i] {
        if ab[i] == b'/' {
            pfx = i + 1;
        }
        i += 1;
    }
    // Walk back from the terminating NULs; with a prefix, run one into it to see its '/'.
    let adj = if pfx > 0 { 1 } else { 0 };
    let (mut x, mut y) = (ab.len() as i64, bb.len() as i64);
    let mut sfx = 0usize;
    let floor = pfx as i64 - adj;
    while floor <= x && floor <= y && at(x as usize) == bt(y as usize) {
        if at(x as usize) == b'/' {
            sfx = ab.len() - x as usize;
        }
        x -= 1;
        y -= 1;
        if x < 0 || y < 0 {
            break;
        }
    }
    let a_mid = (ab.len() as i64 - pfx as i64 - sfx as i64).max(0) as usize;
    let b_mid = (bb.len() as i64 - pfx as i64 - sfx as i64).max(0) as usize;
    let mut out = String::new();
    if pfx + sfx > 0 {
        out.push_str(&a[..pfx]);
        out.push('{');
    }
    out.push_str(&a[pfx..pfx + a_mid]);
    out.push_str(" => ");
    out.push_str(&b[pfx..pfx + b_mid]);
    if pfx + sfx > 0 {
        out.push('}');
        out.push_str(&a[ab.len() - sfx..]);
    }
    out
}

fn stat_name(p: &FilePair) -> String {
    if p.rename.is_some() { rename_name(p.old_path(), p.new_path()) } else { quote_path(p.new_path()) }
}

struct Counts {
    added: u64,
    deleted: u64,
    binary: bool,
}

fn counts(p: &FilePair, data: &HashMap<Hash, Bytes>) -> Counts {
    let empty = Bytes::new();
    let old = p.old.as_ref().map(|e| data.get(&e.hash).unwrap_or(&empty)).unwrap_or(&empty);
    let new = p.new.as_ref().map(|e| data.get(&e.hash).unwrap_or(&empty)).unwrap_or(&empty);
    if is_binary(old) || is_binary(new) {
        let same = p.old.as_ref().zip(p.new.as_ref()).is_some_and(|(a, b)| a.hash == b.hash);
        return if same { Counts { added: 0, deleted: 0, binary: true } } else { Counts { added: new.len() as u64, deleted: old.len() as u64, binary: true } };
    }
    if p.old.as_ref().zip(p.new.as_ref()).is_some_and(|(a, b)| a.hash == b.hash) {
        return Counts { added: 0, deleted: 0, binary: false };
    }
    let (_, _, script) = crate::xdiff::diff(old, new);
    Counts {
        added: script.iter().map(|c| c.chg2 as u64).sum(),
        deleted: script.iter().map(|c| c.chg1 as u64).sum(),
        binary: false,
    }
}

/// Git's default function-name rule: a line starting with a letter, `_` or `$`, cut to 80
/// bytes and right-trimmed.
fn func_name(line: &[u8]) -> Option<&[u8]> {
    let first = *line.first()?;
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
        return None;
    }
    let mut l = &line[..line.len().min(80)];
    while let Some((last, rest)) = l.split_last() {
        if last.is_ascii_whitespace() {
            l = rest;
        } else {
            break;
        }
    }
    Some(l)
}

/// Unified hunks exactly as xdl_emit_diff writes them: changes grouped when at most
/// 2*ctx lines apart, trailing context bounded by both files, function names searched
/// backwards from each hunk to the previous one.
fn emit_hunks(old: &[u8], new: &[u8], ctx: usize, color: bool, out: &mut dyn Write) -> Result<()> {
    let (la, lb, script) = crate::xdiff::diff(old, new);
    let (c_frag, c_old, c_new, c_reset) = if color { ("\x1b[36m", "\x1b[31m", "\x1b[32m", "\x1b[m") } else { ("", "", "", "") };
    let emit = |out: &mut dyn Write, sign: &str, color: &str, line: &[u8]| -> Result<()> {
        out.write_all(color.as_bytes())?;
        out.write_all(sign.as_bytes())?;
        out.write_all(line.strip_suffix(b"\n").unwrap_or(line))?;
        if !color.is_empty() {
            out.write_all(c_reset.as_bytes())?;
        }
        out.write_all(b"\n")?;
        if !line.ends_with(b"\n") {
            out.write_all(b"\\ No newline at end of file\n")?;
        }
        Ok(())
    };
    let ctx = ctx as i64;
    let mut func: Vec<u8> = vec![];
    let mut func_prev: i64 = -1;
    let mut i = 0;
    while i < script.len() {
        let mut j = i;
        while j + 1 < script.len() && (script[j + 1].i1 as i64 - (script[j].i1 + script[j].chg1) as i64) <= 2 * ctx {
            j += 1;
        }
        let (xch, xche) = (script[i], script[j]);
        let s1 = (xch.i1 as i64 - ctx).max(0);
        let s2 = (xch.i2 as i64 - ctx).max(0);
        let lctx = ctx.min(la.len() as i64 - (xche.i1 + xche.chg1) as i64).min(lb.len() as i64 - (xche.i2 + xche.chg2) as i64);
        let e1 = (xche.i1 + xche.chg1) as i64 + lctx;
        let e2 = (xche.i2 + xche.chg2) as i64 + lctx;
        let mut l = s1 - 1;
        while l != func_prev && l >= 0 && l < la.len() as i64 {
            if let Some(f) = func_name(la[l as usize]) {
                func = f.to_vec();
                break;
            }
            l -= 1;
        }
        func_prev = s1 - 1;
        let (c1, c2) = (e1 - s1, e2 - s2);
        let mut hdr = format!("@@ -{}", if c1 > 0 { s1 + 1 } else { s1 });
        if c1 != 1 {
            hdr.push_str(&format!(",{c1}"));
        }
        hdr.push_str(&format!(" +{}", if c2 > 0 { s2 + 1 } else { s2 }));
        if c2 != 1 {
            hdr.push_str(&format!(",{c2}"));
        }
        hdr.push_str(" @@");
        out.write_all(c_frag.as_bytes())?;
        out.write_all(hdr.as_bytes())?;
        out.write_all(c_reset.as_bytes())?;
        if !func.is_empty() {
            out.write_all(b" ")?;
            out.write_all(&func)?;
        }
        out.write_all(b"\n")?;
        for k in s2..xch.i2 as i64 {
            emit(out, " ", "", lb[k as usize])?;
        }
        let (mut a1, mut a2) = (xch.i1, xch.i2);
        for c in &script[i..=j] {
            while a1 < c.i1 && a2 < c.i2 {
                emit(out, " ", "", lb[a2])?;
                a1 += 1;
                a2 += 1;
            }
            for k in c.i1..c.i1 + c.chg1 {
                emit(out, "-", c_old, la[k])?;
            }
            for k in c.i2..c.i2 + c.chg2 {
                emit(out, "+", c_new, lb[k])?;
            }
            a1 = c.i1 + c.chg1;
            a2 = c.i2 + c.chg2;
        }
        for k in (xche.i2 + xche.chg2) as i64..e2 {
            emit(out, " ", "", lb[k as usize])?;
        }
        i = j + 1;
    }
    Ok(())
}
