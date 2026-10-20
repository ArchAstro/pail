//! `pail log`: Git's `log` interface over pail history.
//!
//! Options, ordering, pretty formats, placeholders, dates and the layout of diff output follow
//! Git (revision.c, log-tree.c, pretty.c). History is pail's: the first-parent chain, so output
//! matches `git log --first-parent`. Merged commits keep their `Merge:` line and Git parents
//! when imported by pail-mirror.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;

use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};

use crate::gitdate::{DateMode, GitDate, format_date, parse_approx, parse_date_mode, split_ident};
use crate::gitdiff::{DiffOpts, DiffPrinter};
use crate::hash::Hash;
use crate::manifest::Change;
use crate::objects::Commit;
use crate::rev::Ctx;

#[derive(Clone, Debug, PartialEq)]
pub enum Format {
    Oneline,
    Short,
    Medium,
    Full,
    Fuller,
    Reference,
    Raw,
    /// `format:` (separator semantics) or `tformat:` (terminator semantics).
    User { fmt: String, terminator: bool },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decorate {
    No,
    Short,
    Full,
}

#[derive(Default, Clone, Copy, PartialEq, Debug)]
enum Pattern {
    #[default]
    Basic,
    Extended,
    Fixed,
}

pub struct LogOpts {
    pub revs: Vec<String>,
    pub paths: Vec<String>,
    pub max_count: Option<usize>,
    pub skip: usize,
    pub since: Option<i64>,
    pub since_filter: Option<i64>,
    pub until: Option<i64>,
    authors: Vec<String>,
    committers: Vec<String>,
    greps: Vec<String>,
    all_match: bool,
    invert_grep: bool,
    ignore_case: bool,
    pattern: Pattern,
    min_parents: usize,
    max_parents: Option<usize>,
    pub reverse: bool,
    pub all: bool,
    pub format: Format,
    pub abbrev_commit: Option<bool>,
    pub abbrev: Option<usize>,
    pub date: (DateMode, bool),
    pub decorate: Option<Decorate>,
    pub parents: bool,
    pub color: Option<bool>,
    pub diff: DiffOpts,
    pub full_diff: bool,
}

impl Default for LogOpts {
    fn default() -> Self {
        LogOpts {
            revs: vec![],
            paths: vec![],
            max_count: None,
            skip: 0,
            since: None,
            since_filter: None,
            until: None,
            authors: vec![],
            committers: vec![],
            greps: vec![],
            all_match: false,
            invert_grep: false,
            ignore_case: false,
            pattern: Pattern::Basic,
            min_parents: 0,
            max_parents: None,
            reverse: false,
            all: false,
            format: Format::Medium,
            abbrev_commit: None,
            abbrev: None,
            date: (DateMode::Normal, false),
            decorate: None,
            parents: false,
            color: None,
            diff: DiffOpts::default(),
            full_diff: false,
        }
    }
}

/// A Git usage error: printed as `fatal: …`, exit status 128.
#[derive(Debug)]
pub struct Fatal(pub String);

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Fatal {}

fn fatal(s: impl Into<String>) -> anyhow::Error {
    Fatal(s.into()).into()
}

fn parse_format(s: &str) -> Result<Format> {
    Ok(match s {
        "oneline" => Format::Oneline,
        "short" => Format::Short,
        "medium" | "" => Format::Medium,
        "full" => Format::Full,
        "fuller" => Format::Fuller,
        "reference" => Format::Reference,
        "raw" => Format::Raw,
        _ => {
            if let Some(f) = s.strip_prefix("format:") {
                Format::User { fmt: f.to_string(), terminator: false }
            } else if let Some(f) = s.strip_prefix("tformat:") {
                Format::User { fmt: f.to_string(), terminator: true }
            } else if s.contains('%') {
                Format::User { fmt: s.to_string(), terminator: true }
            } else {
                return Err(fatal(format!("invalid --pretty format: {s}")));
            }
        }
    })
}

impl LogOpts {
    /// Parse Git `log` arguments. Non-option words are revisions until `--`; the caller
    /// moves words that don't resolve into `paths`, as Git does.
    pub fn parse(args: &[String]) -> Result<LogOpts> {
        let mut o = LogOpts::default();
        let mut i = 0;
        // `--opt=value` or `--opt value`.
        let take = |i: &mut usize, a: &str, name: &str| -> Result<Option<String>> {
            if let Some(v) = a.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
                return Ok(Some(v.to_string()));
            }
            if a == name {
                *i += 1;
                return args.get(*i).cloned().map(Some).ok_or_else(|| fatal(format!("{name} requires a value")));
            }
            Ok(None)
        };
        let num = |v: &str| -> Result<usize> { v.parse().map_err(|_| fatal(format!("'{v}': not an integer"))) };
        while i < args.len() {
            let a = args[i].as_str();
            if a == "--" {
                o.paths.extend(args[i + 1..].iter().cloned());
                break;
            }
            if !a.starts_with('-') || a == "-" {
                o.revs.push(a.to_string());
                i += 1;
                continue;
            }
            if let Some(n) = a.strip_prefix('-').filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) {
                o.max_count = Some(num(n)?);
            } else if let Some(n) = a.strip_prefix("-n").filter(|n| !n.is_empty()) {
                o.max_count = Some(num(n)?);
            } else if a == "-n" {
                i += 1;
                o.max_count = Some(num(args.get(i).context("-n requires a value")?)?);
            } else if let Some(v) = take(&mut i, a, "--max-count")? {
                o.max_count = Some(num(&v)?);
            } else if let Some(v) = take(&mut i, a, "--skip")? {
                o.skip = num(&v)?;
            } else if let Some(v) = take(&mut i, a, "--since-as-filter")? {
                o.since_filter = Some(parse_approx(&v)?);
            } else if let Some(v) = take(&mut i, a, "--since")?.or(take(&mut i, a, "--after")?) {
                o.since = Some(parse_approx(&v)?);
            } else if let Some(v) = take(&mut i, a, "--until")?.or(take(&mut i, a, "--before")?) {
                o.until = Some(parse_approx(&v)?);
            } else if let Some(v) = take(&mut i, a, "--author")? {
                o.authors.push(v);
            } else if let Some(v) = take(&mut i, a, "--committer")? {
                o.committers.push(v);
            } else if let Some(v) = take(&mut i, a, "--grep")? {
                o.greps.push(v);
            } else if let Some(v) = take(&mut i, a, "--min-parents")? {
                o.min_parents = num(&v)?;
            } else if let Some(v) = take(&mut i, a, "--max-parents")? {
                o.max_parents = Some(num(&v)?);
            } else if let Some(v) = a.strip_prefix("--pretty=").or(a.strip_prefix("--format=")) {
                o.format = parse_format(v)?;
            } else if let Some(v) = a.strip_prefix("--date=") {
                o.date = parse_date_mode(v).map_err(|e| fatal(e.to_string()))?;
            } else if let Some(v) = a.strip_prefix("--abbrev=") {
                o.abbrev = Some(num(v)?.clamp(4, 64));
            } else if let Some(v) = a.strip_prefix("--decorate=") {
                o.decorate = Some(match v {
                    "short" | "auto" => Decorate::Short,
                    "full" => Decorate::Full,
                    "no" => Decorate::No,
                    _ => return Err(fatal(format!("invalid --decorate option: {v}"))),
                });
            } else if let Some(v) = a.strip_prefix("--color=") {
                o.color = match v {
                    "always" => Some(true),
                    "never" => Some(false),
                    "auto" => None,
                    _ => return Err(fatal(format!("invalid --color value: {v}"))),
                };
            } else if !o.diff.parse_arg(a)? {
                match a {
                    "--all-match" => o.all_match = true,
                    "--invert-grep" => o.invert_grep = true,
                    "-i" | "--regexp-ignore-case" => o.ignore_case = true,
                    "-E" | "--extended-regexp" | "-P" | "--perl-regexp" => o.pattern = Pattern::Extended,
                    "-F" | "--fixed-strings" => o.pattern = Pattern::Fixed,
                    "-G" | "--basic-regexp" => o.pattern = Pattern::Basic,
                    "--merges" => o.min_parents = 2,
                    "--no-merges" => o.max_parents = Some(1),
                    "--no-min-parents" => o.min_parents = 0,
                    "--no-max-parents" => o.max_parents = None,
                    "--first-parent" | "--no-walk=unsorted" | "--date-order" | "--topo-order" => {}
                    "--reverse" => o.reverse = true,
                    "--all" | "--branches" => o.all = true,
                    "--oneline" => {
                        o.format = Format::Oneline;
                        o.abbrev_commit = Some(true);
                    }
                    "--pretty" => o.format = Format::Medium,
                    "--abbrev-commit" => o.abbrev_commit = Some(true),
                    "--no-abbrev-commit" => o.abbrev_commit = Some(false),
                    "--relative-date" => o.date = (DateMode::Relative, false),
                    "--decorate" => o.decorate = Some(Decorate::Short),
                    "--no-decorate" => o.decorate = Some(Decorate::No),
                    "--parents" => o.parents = true,
                    "--color" => o.color = Some(true),
                    "--no-color" => o.color = Some(false),
                    "--full-diff" => o.full_diff = true,
                    _ => return Err(fatal(format!("unrecognized argument: {a}"))),
                }
            }
            i += 1;
        }
        Ok(o)
    }
}

/// Convert a POSIX basic regex to Rust syntax: `\+ \? \| \( \) \{ \}` are operators and the
/// bare characters are literals; `\<`/`\>` are word boundaries.
fn bre_to_rust(p: &str) -> String {
    let mut out = String::new();
    let mut it = p.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => match it.next() {
                Some(n @ ('+' | '?' | '|' | '(' | ')' | '{' | '}')) => out.push(n),
                Some('<') | Some('>') => out.push_str("\\b"),
                Some(n) => {
                    out.push('\\');
                    out.push(n);
                }
                None => out.push_str("\\\\"),
            },
            '+' | '?' | '|' | '(' | ')' | '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

struct Matchers {
    authors: Vec<Regex>,
    committers: Vec<Regex>,
    greps: Vec<Regex>,
}

impl Matchers {
    fn new(o: &LogOpts) -> Result<Matchers> {
        let build = |p: &String| -> Result<Regex> {
            let src = match o.pattern {
                Pattern::Basic => bre_to_rust(p),
                Pattern::Extended => p.clone(),
                Pattern::Fixed => regex::escape(p),
            };
            RegexBuilder::new(&src)
                .case_insensitive(o.ignore_case)
                .multi_line(true)
                .build()
                .map_err(|e| fatal(format!("invalid regex {p:?}: {e}")))
        };
        Ok(Matchers {
            authors: o.authors.iter().map(build).collect::<Result<_>>()?,
            committers: o.committers.iter().map(build).collect::<Result<_>>()?,
            greps: o.greps.iter().map(build).collect::<Result<_>>()?,
        })
    }

    fn accepts(&self, o: &LogOpts, c: &Commit) -> bool {
        // Header patterns must all hold; message patterns are ORed unless --all-match.
        if !self.authors.is_empty() && !self.authors.iter().any(|r| r.is_match(&c.author)) {
            return false;
        }
        if !self.committers.is_empty() && !self.committers.iter().any(|r| r.is_match(&c.committer)) {
            return false;
        }
        if self.greps.is_empty() {
            return true;
        }
        let hit = if o.all_match {
            self.greps.iter().all(|r| r.is_match(&c.message))
        } else {
            self.greps.iter().any(|r| r.is_match(&c.message))
        };
        hit != o.invert_grep
    }
}

/// Git pathspecs: a path matches itself and everything under it; `*`, `?` and `[…]` glob
/// across `/`.
pub struct Pathspec {
    items: Vec<(String, Option<globset::GlobMatcher>)>,
}

impl Pathspec {
    pub fn new(paths: &[String]) -> Result<Pathspec> {
        let mut items = vec![];
        for p in paths {
            let p = p.trim_start_matches("./").trim_end_matches('/').to_string();
            let glob = if p.contains(['*', '?', '[']) {
                Some(globset::GlobBuilder::new(&p).literal_separator(false).build()?.compile_matcher())
            } else {
                None
            };
            items.push((p, glob));
        }
        Ok(Pathspec { items })
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn matches(&self, path: &str) -> bool {
        self.items.is_empty()
            || self.items.iter().any(|(p, g)| match g {
                Some(g) => g.is_match(path) || path.starts_with(&format!("{p}/")),
                None => p.is_empty() || path == p || path.starts_with(&format!("{p}/")),
            })
    }
}

/// One commit ready to print.
pub struct Shown {
    pub hash: Hash,
    pub commit: Arc<Commit>,
    pub changes: Option<Vec<Change>>,
}

/// Ordered, filtered commits for `log`.
pub async fn walk(ctx: &Ctx, o: &LogOpts, spec: &Pathspec, need_diff: bool) -> Result<Vec<Shown>> {
    let mut positives = vec![];
    let mut negatives = vec![];
    for r in &o.revs {
        if let Some((a, b)) = r.split_once("...") {
            let a = if a.is_empty() { ctx.head.context("HEAD has no commits")? } else { ctx.resolve(a).await? };
            let b = if b.is_empty() { ctx.head.context("HEAD has no commits")? } else { ctx.resolve(b).await? };
            positives.extend([a, b]);
            if let Some(m) = ctx.repo.merge_base(a, b).await? {
                negatives.push(m);
            }
        } else if let Some((a, b)) = r.split_once("..") {
            let a = if a.is_empty() { ctx.head.context("HEAD has no commits")? } else { ctx.resolve(a).await? };
            let b = if b.is_empty() { ctx.head.context("HEAD has no commits")? } else { ctx.resolve(b).await? };
            negatives.push(a);
            positives.push(b);
        } else if let Some(n) = r.strip_prefix('^') {
            negatives.push(ctx.resolve(n).await?);
        } else {
            positives.push(ctx.resolve(r).await?);
        }
    }
    if o.all {
        positives.extend(ctx.refs().await?.into_iter().map(|(_, h)| h));
    }
    if positives.is_empty() && o.revs.is_empty() && !o.all {
        match ctx.head {
            Some(h) => positives.push(h),
            None => return Err(fatal(format!("your current branch '{}' does not have any commits yet", ctx.branch))),
        }
    }
    // Linear history: everything at or below the merge base with a negative is excluded.
    let mut stop: HashMap<Hash, u64> = HashMap::new();
    for p in &positives {
        let mut floor = None;
        for n in &negatives {
            if let Some(m) = ctx.repo.merge_base(*p, *n).await? {
                let g = ctx.commit(&m).await?.generation;
                floor = Some(floor.map_or(g, |f: u64| f.max(g)));
            }
        }
        if let Some(f) = floor {
            stop.insert(*p, f);
        }
    }
    let matchers = Matchers::new(o)?;
    // Date-ordered queue (Git's default order); ties go to the earlier-inserted commit.
    let mut heap: BinaryHeap<(i64, std::cmp::Reverse<u64>, Hash, Option<u64>)> = BinaryHeap::new();
    let mut seen = HashSet::new();
    let mut ctr = 0u64;
    for p in &positives {
        let c = ctx.commit(p).await?;
        if seen.insert(*p) {
            heap.push((GitDate::parse(&c.committer_date)?.secs, std::cmp::Reverse(ctr), *p, stop.get(p).copied()));
            ctr += 1;
        }
    }
    let mut out = vec![];
    let mut skipped = 0;
    let mut walking = true;
    let wants_diff = need_diff || !spec.is_empty();
    // Commits pass the cheap filters one by one; their diffs are computed a batch at a time,
    // concurrently, since each one is a few round trips to the store.
    'outer: while walking {
        let mut batch: Vec<(Hash, Arc<Commit>)> = vec![];
        while batch.len() < if wants_diff { 64 } else { 1 } {
            let Some((date, _, h, floor)) = heap.pop() else {
                walking = false;
                break;
            };
            let c = ctx.commit(&h).await?;
            if floor.is_some_and(|f| c.generation <= f) {
                continue;
            }
            if o.since.is_some_and(|s| date < s) {
                // Git stops walking at the first commit older than --since.
                walking = false;
                break;
            }
            if let Some(p) = c.parent {
                if seen.insert(p) {
                    let pc = ctx.commit(&p).await?;
                    heap.push((GitDate::parse(&pc.committer_date)?.secs, std::cmp::Reverse(ctr), p, floor));
                    ctr += 1;
                }
            }
            if o.until.is_some_and(|u| date > u) || o.since_filter.is_some_and(|s| date < s) {
                continue;
            }
            let nparents = c.meta.get("git.parents").map(|p| p.split_whitespace().count()).unwrap_or(c.parent.is_some() as usize);
            if nparents < o.min_parents || o.max_parents.is_some_and(|m| nparents > m) {
                continue;
            }
            if !matchers.accepts(o, &c) {
                continue;
            }
            batch.push((h, c));
        }
        let diffs: Vec<Option<Vec<Change>>> = if wants_diff {
            use futures::StreamExt;
            futures::stream::iter(batch.iter().map(|(_, c)| async move {
                // The commit's own change list when it has one; a manifest diff otherwise.
                if let Some(cs) = c.change_list() {
                    return Ok(Some(cs));
                }
                let from = ctx.repo.root_of(c.parent).await?;
                ctx.repo.diff(from, c.root).await.map(Some)
            }))
            .buffered(16)
            .collect::<Vec<Result<_>>>()
            .await
            .into_iter()
            .collect::<Result<_>>()?
        } else {
            batch.iter().map(|_| None).collect()
        };
        for ((h, c), all) in batch.into_iter().zip(diffs) {
            let changes = match all {
                Some(all) => {
                    if !spec.is_empty() && !all.iter().any(|ch| spec.matches(&ch.path)) {
                        continue;
                    }
                    Some(if o.full_diff { all } else { all.into_iter().filter(|ch| spec.matches(&ch.path)).collect() })
                }
                None => None,
            };
            if skipped < o.skip {
                skipped += 1;
                continue;
            }
            out.push(Shown { hash: h, commit: c, changes });
            if o.max_count.is_some_and(|m| out.len() >= m) {
                break 'outer;
            }
        }
    }
    if o.max_count == Some(0) {
        out.clear();
    }
    if o.reverse {
        out.reverse();
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Formatting

pub const C_COMMIT: &str = "\x1b[33m";
pub const C_RESET: &str = "\x1b[m";
const C_HEAD: &str = "\x1b[1;36m";
const C_BRANCH: &str = "\x1b[1;32m";

/// Subject: the first paragraph, lines right-trimmed and joined by spaces.
pub fn subject(msg: &str) -> String {
    let mut parts = vec![];
    for line in msg.trim_start_matches(['\n', '\r']).split('\n') {
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        parts.push(t);
    }
    parts.join(" ")
}

/// Body: what follows the subject paragraph and the blank lines after it.
pub fn body(msg: &str) -> &str {
    let msg = skip_blank_lines(msg);
    let mut rest = msg;
    // Skip the subject paragraph.
    loop {
        let (line, next) = match rest.find('\n') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        if line.trim().is_empty() || rest.is_empty() {
            break;
        }
        rest = next;
    }
    skip_blank_lines(rest)
}

fn skip_blank_lines(mut s: &str) -> &str {
    loop {
        let (line, next) = match s.find('\n') {
            Some(i) => (&s[..i], &s[i + 1..]),
            None => return if s.trim().is_empty() { "" } else { s },
        };
        if !line.trim().is_empty() {
            return s;
        }
        s = next;
    }
}

/// Git's `format_sanitized_subject` (`%f`): title characters kept, other runs become one
/// '-', runs of '.' collapse, trailing '.'/'-' trimmed.
fn sanitized(subj: &str) -> String {
    let b = subj.as_bytes();
    let mut out: Vec<u8> = vec![];
    let mut space = 2;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_alphanumeric() || c == b'.' || c == b'_' {
            if space == 1 {
                out.push(b'-');
            }
            space = 0;
            out.push(c);
            if c == b'.' {
                while i + 1 < b.len() && b[i + 1] == b'.' {
                    i += 1;
                }
            }
        } else {
            space |= 1;
        }
        i += 1;
    }
    while matches!(out.last(), Some(b'.') | Some(b'-')) {
        out.pop();
    }
    String::from_utf8(out).unwrap_or_default()
}

fn expand_tabs(line: &str, tab: usize) -> String {
    let mut out = String::new();
    let mut col = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let n = tab - col % tab;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(ch);
            col += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    out
}

pub struct Printer<'a> {
    pub ctx: &'a Ctx,
    pub o: &'a LogOpts,
    pub color: bool,
    pub decorations: HashMap<Hash, Vec<(String, bool)>>,
    pub abbrev: usize,
    pub abbrev_commit: bool,
    pub diff: DiffPrinter<'a>,
    decorate: bool,
    pub shown_one: bool,
}

impl<'a> Printer<'a> {
    /// `any_merge`: whether a commit to be shown is a merge (its `Merge:` line abbreviates).
    pub async fn new(ctx: &'a Ctx, o: &'a LogOpts, tty: bool, any_merge: bool) -> Result<Printer<'a>> {
        let color = o.color.unwrap_or(tty);
        let decorate = o.decorate.unwrap_or(if tty { Decorate::Short } else { Decorate::No });
        let needs_decor = decorate != Decorate::No || matches!(&o.format, Format::User { fmt, .. } if fmt.contains("%d") || fmt.contains("%D"));
        let mut decorations: HashMap<Hash, Vec<(String, bool)>> = HashMap::new();
        if needs_decor {
            let full = decorate == Decorate::Full;
            let name = |b: &str| if full { format!("refs/heads/{b}") } else { b.to_string() };
            let refs = ctx.refs().await?;
            let head_branch_tip = refs.iter().find(|(b, _)| *b == ctx.branch).map(|(_, h)| *h);
            if let Some(h) = ctx.head {
                if head_branch_tip == Some(h) {
                    decorations.entry(h).or_default().push((format!("HEAD -> {}", name(&ctx.branch)), true));
                } else {
                    decorations.entry(h).or_default().push(("HEAD".into(), true));
                }
            }
            for (b, h) in refs {
                if b == ctx.branch && ctx.head == Some(h) {
                    continue;
                }
                decorations.entry(h).or_default().push((name(&b), false));
            }
        }
        let abbrev_commit = o.abbrev_commit.unwrap_or(matches!(o.format, Format::Oneline | Format::Reference));
        // The default length needs every commit id; only compute it if something abbreviates.
        let uses_short = abbrev_commit
            || (any_merge && !matches!(o.format, Format::Oneline | Format::Raw | Format::User { .. }))
            || matches!(&o.format, Format::User { fmt, .. } if fmt.contains("%h") || fmt.contains("%p") || fmt.contains("%t"))
            || (o.diff.any_output() && (o.diff.patch || o.diff.raw));
        let abbrev = match o.abbrev {
            Some(a) => a,
            None if uses_short => ctx.default_abbrev().await?,
            None => 7,
        };
        Ok(Printer {
            ctx,
            o,
            color,
            decorations,
            abbrev,
            abbrev_commit,
            diff: DiffPrinter::new(ctx, &o.diff, color, abbrev).await?,
            decorate: decorate != Decorate::No,
            shown_one: false,
        })
    }

    fn short(&self, id: &str) -> String {
        id[..self.abbrev.min(id.len())].to_string()
    }

    /// ` (HEAD -> main, feature)` with Git's colors.
    fn decoration(&self, h: &Hash, color: bool) -> String {
        let Some(ds) = self.decorations.get(h) else { return String::new() };
        let (cc, cr) = if color { (C_COMMIT, C_RESET) } else { ("", "") };
        let mut s = String::new();
        let mut prefix = " (";
        for (name, is_head) in ds {
            s.push_str(cc);
            s.push_str(prefix);
            s.push_str(cr);
            if let Some(b) = name.strip_prefix("HEAD -> ") {
                if color {
                    s.push_str(&format!("{C_HEAD}HEAD{C_RESET}{C_COMMIT} -> {C_RESET}{C_BRANCH}{b}"));
                } else {
                    s.push_str(name);
                }
            } else if color {
                s.push_str(if *is_head { C_HEAD } else { C_BRANCH });
                s.push_str(name);
            } else {
                s.push_str(name);
            }
            s.push_str(cr);
            prefix = ", ";
        }
        s.push_str(cc);
        s.push(')');
        s.push_str(cr);
        s
    }

    fn decoration_list(&self, h: &Hash) -> String {
        self.decorations.get(h).map(|ds| ds.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()
    }

    fn date(&self, raw: &str) -> String {
        match GitDate::parse(raw) {
            Ok(d) => format_date(d, &self.o.date.0, self.o.date.1),
            Err(_) => raw.to_string(),
        }
    }

    fn date_as(&self, raw: &str, mode: DateMode) -> String {
        GitDate::parse(raw).map(|d| format_date(d, &mode, false)).unwrap_or_default()
    }

    /// The header and message, as `pretty_print_commit` builds them.
    fn pretty(&self, s: &Shown) -> String {
        let c = &s.commit;
        let mut sb = String::new();
        let parents = self.ctx.parent_ids(c);
        if parents.len() > 1 && !matches!(self.o.format, Format::Oneline | Format::Raw) {
            sb.push_str("Merge:");
            for p in &parents {
                sb.push(' ');
                sb.push_str(&self.short(p));
            }
            sb.push('\n');
        }
        let date = |d: &str| self.date(d);
        match self.o.format {
            Format::Oneline => return subject(&c.message),
            Format::Short => sb.push_str(&format!("Author: {}\n", c.author)),
            Format::Medium => sb.push_str(&format!("Author: {}\nDate:   {}\n", c.author, date(&c.author_date))),
            Format::Full => sb.push_str(&format!("Author: {}\nCommit: {}\n", c.author, c.committer)),
            Format::Fuller => sb.push_str(&format!(
                "Author:     {}\nAuthorDate: {}\nCommit:     {}\nCommitDate: {}\n",
                c.author,
                date(&c.author_date),
                c.committer,
                date(&c.committer_date)
            )),
            Format::Raw => {
                sb.push_str(&format!("tree {}\n", c.root.hex()));
                for p in &parents {
                    sb.push_str(&format!("parent {p}\n"));
                }
                sb.push_str(&format!("author {} {}\ncommitter {} {}\n", c.author, c.author_date, c.committer, c.committer_date));
                if let Some(x) = c.meta.get("git.extra_headers") {
                    sb.push_str(x);
                }
            }
            Format::Reference | Format::User { .. } => unreachable!(),
        }
        sb.push('\n');
        let msg = skip_blank_lines(&c.message);
        let tab = matches!(self.o.format, Format::Medium | Format::Full | Format::Fuller);
        let mut first = true;
        for line in msg.split_inclusive('\n') {
            let l = line.strip_suffix('\n').unwrap_or(line);
            if l.trim().is_empty() {
                if first {
                    continue;
                }
                if self.o.format == Format::Short {
                    break;
                }
            }
            first = false;
            let l = if l.trim().is_empty() { "" } else { l };
            sb.push_str("    ");
            if tab { sb.push_str(&expand_tabs(l, 8)) } else { sb.push_str(l) }
            sb.push('\n');
        }
        let trimmed = sb.trim_end().len();
        sb.truncate(trimmed);
        sb.push('\n');
        sb
    }

    /// Expand a user format (`--format=`).
    fn user_format(&self, fmt: &str, s: &Shown) -> String {
        let c = &s.commit;
        let id = self.ctx.id(&s.hash, c);
        let parents = self.ctx.parent_ids(c);
        let (an, ae) = split_ident(&c.author);
        let (cn, ce) = split_ident(&c.committer);
        let mut out = String::new();
        let b = fmt.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'%' {
                let start = i;
                while i < b.len() && b[i] != b'%' {
                    i += 1;
                }
                out.push_str(&fmt[start..i]);
                continue;
            }
            i += 1;
            if i >= b.len() {
                out.push('%');
                break;
            }
            // %+x adds a newline before non-empty output; %-x removes newlines before empty
            // output; "% x" adds a space before non-empty output.
            let magic = match b[i] {
                b'+' | b'-' | b' ' if i + 1 < b.len() => {
                    i += 1;
                    Some(b[i - 1])
                }
                _ => None,
            };
            let rest = &fmt[i..];
            let (val, used): (Option<String>, usize) = self.placeholder(rest, s, &id, &parents, (an, ae), (cn, ce));
            match val {
                Some(v) => {
                    match magic {
                        Some(b'+') if !v.is_empty() => out.push('\n'),
                        Some(b'-') if v.is_empty() => {
                            while out.ends_with('\n') {
                                out.pop();
                            }
                        }
                        Some(b' ') if !v.is_empty() => out.push(' '),
                        _ => {}
                    }
                    out.push_str(&v);
                    i += used;
                }
                None => {
                    out.push('%');
                    if let Some(m) = magic {
                        out.push(m as char);
                    }
                }
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn placeholder(
        &self,
        rest: &str,
        s: &Shown,
        id: &str,
        parents: &[String],
        (an, ae): (&str, &str),
        (cn, ce): (&str, &str),
    ) -> (Option<String>, usize) {
        let c = &s.commit;
        let one = |v: String| (Some(v), 1);
        let two = |v: String| (Some(v), 2);
        let color = |code: &str| if self.color { code.to_string() } else { String::new() };
        let date_ph = |raw: &str, ch: u8| -> Option<String> {
            Some(match ch {
                b'd' => self.date(raw),
                b'D' => self.date_as(raw, DateMode::Rfc),
                b'r' => self.date_as(raw, DateMode::Relative),
                b't' => self.date_as(raw, DateMode::Unix),
                b'i' => self.date_as(raw, DateMode::Iso),
                b'I' => self.date_as(raw, DateMode::IsoStrict),
                b's' => self.date_as(raw, DateMode::Short),
                b'h' => self.date_as(raw, DateMode::Human),
                _ => return None,
            })
        };
        let ident_ph = |name: &str, email: &str, date: &str, ch: u8| -> Option<String> {
            Some(match ch {
                b'n' | b'N' => name.to_string(),
                b'e' | b'E' => email.to_string(),
                b'l' | b'L' => email.split('@').next().unwrap_or("").to_string(),
                other => return date_ph(date, other),
            })
        };
        let bytes = rest.as_bytes();
        match bytes[0] {
            b'H' => one(id.to_string()),
            b'h' => one(self.short(id)),
            b'T' => one(c.root.hex()),
            b't' => one(self.short(&c.root.hex())),
            b'P' => one(parents.join(" ")),
            b'p' => one(parents.iter().map(|p| self.short(p)).collect::<Vec<_>>().join(" ")),
            b'a' | b'c' if bytes.len() > 1 => {
                let v = if bytes[0] == b'a' {
                    ident_ph(an, ae, &c.author_date, bytes[1])
                } else {
                    ident_ph(cn, ce, &c.committer_date, bytes[1])
                };
                match v {
                    Some(v) => two(v),
                    None => (None, 0),
                }
            }
            b'd' => one(match self.decoration_list(&s.hash) {
                d if d.is_empty() => String::new(),
                d => format!(" ({d})"),
            }),
            b'D' => one(self.decoration_list(&s.hash)),
            b's' => one(subject(&c.message)),
            b'f' => one(sanitized(&subject(&c.message))),
            b'b' => one(body(&c.message).to_string()),
            b'B' => one(c.message.clone()),
            b'e' => one(String::new()),
            b'N' => one(String::new()),
            b'n' => one("\n".into()),
            b'%' => one("%".into()),
            b'm' => one(">".into()),
            b'x' if bytes.len() >= 3 => match u8::from_str_radix(&rest[1..3], 16) {
                Ok(v) => (Some((v as char).to_string()), 3),
                Err(_) => (None, 0),
            },
            b'C' => {
                for (name, code) in [("red", "\x1b[31m"), ("green", "\x1b[32m"), ("blue", "\x1b[34m"), ("reset", "\x1b[m")] {
                    if rest[1..].starts_with(name) {
                        return (Some(color(code)), 1 + name.len());
                    }
                }
                if rest[1..].starts_with('(') {
                    if let Some(end) = rest.find(')') {
                        let spec = &rest[2..end];
                        let force = spec.starts_with("always,");
                        let spec = spec.trim_start_matches("always,");
                        let on = self.color || force;
                        let code = if spec == "auto" || spec == "auto,reset" { String::new() } else { ansi(spec) };
                        return (Some(if on { code } else { String::new() }), end + 1);
                    }
                }
                (None, 0)
            }
            b'w' | b'<' | b'>' if rest[1..].starts_with('(') || rest[1..].starts_with("<(") || rest[1..].starts_with(">(") => {
                // Column/wrapping directives: accepted, not applied.
                match rest.find(')') {
                    Some(end) => (Some(String::new()), end + 1),
                    None => (None, 0),
                }
            }
            _ => (None, 0),
        }
    }

    /// Print one commit (and its diff) exactly as `git log` lays it out.
    pub async fn show(&mut self, s: &Shown, out: &mut dyn Write) -> Result<()> {
        let use_terminator = matches!(self.o.format, Format::Oneline | Format::Reference | Format::User { terminator: true, .. });
        if self.shown_one && !use_terminator {
            out.write_all(b"\n")?;
        }
        self.shown_one = true;
        let id = self.ctx.id(&s.hash, &s.commit);
        let shown_id = if self.abbrev_commit { self.short(&id) } else { id.clone() };
        let msg: String;
        let empty_user = matches!(&self.o.format, Format::User { fmt, .. } if fmt.is_empty());
        match &self.o.format {
            Format::User { fmt, .. } => {
                msg = self.user_format(fmt, s);
            }
            Format::Reference => {
                // Git's reference format defaults to short dates.
                let fmt = if self.o.date.0 == DateMode::Normal && !self.o.date.1 { "%h (%s, %as)" } else { "%h (%s, %ad)" };
                msg = self.user_format(fmt, s);
            }
            _ => {
                let mut line = String::new();
                if self.color {
                    line.push_str(C_COMMIT);
                }
                if self.o.format != Format::Oneline {
                    line.push_str("commit ");
                }
                line.push_str(&shown_id);
                if self.o.parents {
                    for p in self.ctx.parent_ids(&s.commit) {
                        line.push(' ');
                        line.push_str(&if self.abbrev_commit { self.short(&p) } else { p });
                    }
                }
                if self.decorate {
                    line.push_str(&self.decoration(&s.hash, self.color));
                }
                if self.color {
                    line.push_str(C_RESET);
                }
                line.push(if self.o.format == Format::Oneline { ' ' } else { '\n' });
                out.write_all(line.as_bytes())?;
                msg = self.pretty(s);
            }
        }
        out.write_all(msg.as_bytes())?;
        if use_terminator && !empty_user {
            out.write_all(b"\n")?;
        }
        if let Some(changes) = &s.changes {
            if self.o.diff.any_output() && !changes.is_empty() {
                if self.o.format != Format::Oneline && !empty_user {
                    if self.o.diff.stat_and_patch() {
                        out.write_all(b"---")?;
                    }
                    out.write_all(b"\n")?;
                }
                self.diff.print(s, changes, out).await?;
            }
        }
        Ok(())
    }
}

/// ANSI code for a Git color spec like `yellow`, `bold red`, `dim`.
fn ansi(spec: &str) -> String {
    let mut codes = vec![];
    let mut fg_set = false;
    for w in spec.split_whitespace() {
        let n = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"].iter().position(|c| *c == w);
        match (w, n) {
            ("reset", _) => return "\x1b[m".into(),
            ("bold", _) => codes.push("1".to_string()),
            ("dim", _) => codes.push("2".to_string()),
            ("italic", _) => codes.push("3".to_string()),
            ("ul", _) => codes.push("4".to_string()),
            ("blink", _) => codes.push("5".to_string()),
            ("reverse", _) => codes.push("7".to_string()),
            (_, Some(n)) if !fg_set => {
                codes.push(format!("3{n}"));
                fg_set = true;
            }
            (_, Some(n)) => codes.push(format!("4{n}")),
            _ => {}
        }
    }
    if codes.is_empty() { String::new() } else { format!("\x1b[{}m", codes.join(";")) }
}
