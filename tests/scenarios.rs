//! Scenario tests: transcripts of shell sessions (`tests/scenarios/*.t`, the cram format
//! Mercurial and Sapling test with) replayed against the built `pail`, and often against real
//! Git beside it. See `tests/scenarios/README.md`.
//!
//! ```text
//! Prose lines are comments.
//!   $ cd alice && commit a.txt v1 "add a"      a command (`  > ` continues it)
//!   [main #1] add a                            its output (stdout and stderr, in order)
//!   [4]                                        its exit status, when not 0
//! ```
//!
//! Each file runs as one bash session (cwd, variables and functions persist) in a fresh
//! temporary directory. Output is normalized before comparing: the directory becomes `$TMP`
//! and every id (hex, 7+ characters with a digit) becomes `#n`, numbered by first appearance,
//! so transcripts stay stable while commit ids change with the clock. An expected line ending
//! in ` (glob)` matches with `*` as a wildcard.
//!
//! `PAIL_BLESS=1 cargo test --test scenarios` rewrites the files with what actually ran;
//! `PAIL_SCENARIO=<name>` runs only the files whose name contains it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Helpers every scenario can use (see the README for what each one is for).
const PRELUDE: &str = r#"
export HOME="$TMP/home" STORE="file://$TMP/store"
export PAIL_CONFIG_GLOBAL="$TMP/home/pail-config" PAIL_AUTHOR="Tester <t@example.com>"
export PAIL_PAGER=cat PAGER=cat EDITOR=true VISUAL=true
# Fixed dates: commit ids depend only on content, so reruns (and --dry-run) give the same ids.
export PAIL_DATE="1790000000 +0000" GIT_AUTHOR_DATE="1790000000 +0000" GIT_COMMITTER_DATE="1790000000 +0000"
export GIT_CONFIG_GLOBAL="$TMP/home/gitconfig" GIT_CONFIG_NOSYSTEM=1 GIT_EDITOR=true
export GIT_AUTHOR_NAME=Tester GIT_AUTHOR_EMAIL=t@example.com
export GIT_COMMITTER_NAME=Tester GIT_COMMITTER_EMAIL=t@example.com
mkdir -p "$HOME"
git config --global init.defaultBranch main
git config --global advice.detachedHead false
git config --global pull.rebase true

# `ginit <dir>`: a Git repository that starts like a pail one, with an empty first commit.
ginit() { git init -q "$1" && git -C "$1" commit -q --allow-empty -m "Initial empty commit"; }

# Who acts next: `as alice` (pail and git identities).
as() {
  local n="$1"
  export PAIL_AUTHOR="$n <$n@example.com>" GIT_AUTHOR_NAME="$n" GIT_AUTHOR_EMAIL="$n@example.com"
  export GIT_COMMITTER_NAME="$n" GIT_COMMITTER_EMAIL="$n@example.com"
}

# `commit <path> <content> <message>`: write, stage everything, commit (pail or git, by cwd).
commit() {
  mkdir -p "$(dirname "$1")" && printf '%s\n' "$2" > "$1"
  if [ -d .git ]; then git add -A && git commit -q -m "$3"
  else pail add -A >/dev/null && pail commit -m "$3"; fi
}

# `state [<rev>]`: subjects of history, newest first, then every file and its first line.
# The same in a pail working copy and a Git one, so parity is `diff`.
state() {
  if [ -d .git ]; then git log --format=%s ${1:-HEAD}; else pail log --format=%s $1; fi
  echo "--"
  find . -type f -not -path './.pail/*' -not -path './.git/*' | LC_ALL=C sort | while read -r f; do
    echo "${f#./}: $(head -n1 "$f")"
  done
}

# `todo <<EOF … EOF`: the next `rebase -i` (pail or git) gets this todo list instead of its
# own. Lines are `<verb> <subject> [<text>]`: each subject is replaced by its commit's id.
todo() { cat > "$TMP/todo.want"; }
cat > "$TMP/apply-todo" <<'SH'
#!/bin/sh
# Rewrite the rebase todo file $1 to the wanted lines (`<verb> <subject>[|<text>]`).
awk -v want="$TMP/todo.want" '
  # Git writes `pick <sha> # <subject>`; pail writes `pick <id> <subject>`.
  $1 == "pick" || $1 == "p" { sha = $2; $1 = ""; $2 = ""; s = substr($0, 3); sub(/^# /, "", s); id[s] = sha }
  END {
    while ((getline line < want) > 0) {
      if (line ~ /^[ \t]*$/) continue
      verb = line; sub(/[ \t].*/, "", verb)
      rest = line; sub(/^[^ \t]+[ \t]+/, "", rest)
      subj = rest; text = ""
      p = index(rest, "|"); if (p) { subj = substr(rest, 1, p - 1); text = substr(rest, p + 1) }
      if (!(subj in id)) { print "apply-todo: no commit \"" subj "\"" > "/dev/stderr"; exit 1 }
      print verb " " id[subj] " " text
    }
  }' "$1" > "$1.new" && mv "$1.new" "$1"
SH
chmod +x "$TMP/apply-todo"
export PAIL_SEQUENCE_EDITOR="$TMP/apply-todo" GIT_SEQUENCE_EDITOR="$TMP/apply-todo"
"#;

struct Step {
    /// Line index of the `$` line in the file.
    at: usize,
    script: String,
    expected: Vec<String>,
}

struct Scenario {
    lines: Vec<String>,
    commands: Vec<Step>,
    requires: Vec<String>,
}

fn parse(text: &str) -> Scenario {
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut commands: Vec<Step> = vec![];
    let mut requires = vec![];
    // In a command's script (continuation lines allowed), in its output, or in prose.
    #[derive(PartialEq)]
    enum At {
        Script,
        Output,
        Prose,
    }
    let mut at = At::Prose;
    for (i, l) in lines.iter().enumerate() {
        if let Some(c) = l.strip_prefix("  $ ") {
            commands.push(Step {
                at: i,
                script: c.to_string(),
                expected: vec![],
            });
            at = At::Script;
        } else if let (Some(c), At::Script) = (l.strip_prefix("  > "), &at) {
            let last = commands.last_mut().unwrap();
            last.script.push('\n');
            last.script.push_str(c);
        } else if let (Some(out), false) = (l.strip_prefix("  "), at == At::Prose) {
            commands.last_mut().unwrap().expected.push(out.to_string());
            at = At::Output;
        } else {
            if let Some(r) = l.strip_prefix("#require ") {
                requires.extend(r.split_whitespace().map(str::to_string));
            }
            at = At::Prose;
        }
    }
    Scenario {
        lines,
        commands,
        requires,
    }
}

/// Run every command in one bash session; per command, its output lines and exit status.
fn run(s: &Scenario, tmp: &Path) -> Vec<(Vec<String>, i32)> {
    let marker = format!("@@PAIL-SCENARIO-{}@@", std::process::id());
    let mut script = String::from("exec 2>&1\n");
    script.push_str(PRELUDE);
    for (i, c) in s.commands.iter().enumerate() {
        script.push_str(&format!(
            "echo {marker} {i}\n{}\n__status=$?; echo; echo {marker} {i} $__status\n",
            c.script
        ));
    }
    let bin = Path::new(env!("CARGO_BIN_EXE_pail")).parent().unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .current_dir(tmp)
        .env_clear()
        .env("PATH", path)
        .env("TMP", tmp)
        .env(
            "EXTENSIONS",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions"),
        )
        .env("TMPDIR", tmp)
        .env("LANG", "C")
        .env("TZ", "UTC")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("bash");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut results: Vec<(Vec<String>, i32)> = vec![(vec![], 0); s.commands.len()];
    let mut cur: Option<usize> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{marker} ")) {
            let mut parts = rest.split_whitespace();
            let i: usize = parts.next().unwrap().parse().unwrap();
            match parts.next() {
                None => cur = Some(i),
                Some(code) => {
                    let r = &mut results[i];
                    // The `echo` before the status line ends a last line without a newline.
                    if r.0.last().is_some_and(|l| l.is_empty()) {
                        r.0.pop();
                    }
                    r.1 = code.parse().unwrap();
                    cur = None;
                }
            }
        } else if let Some(i) = cur {
            results[i].0.push(line.to_string());
        }
    }
    results
}

/// Ids and paths that change from run to run, made stable.
struct Normalizer {
    tmp: String,
    ids: HashMap<String, usize>,
    hex: regex::Regex,
}

impl Normalizer {
    fn new(tmp: &Path) -> Normalizer {
        Normalizer {
            tmp: tmp.display().to_string(),
            ids: HashMap::new(),
            hex: regex::Regex::new(r"\b[0-9a-f]{7,64}\b").unwrap(),
        }
    }

    fn line(&mut self, l: &str) -> String {
        let l = l.replace(&self.tmp, "$TMP");
        let ids = &mut self.ids;
        self.hex
            .replace_all(&l, |c: &regex::Captures| {
                let h = &c[0];
                if !h.bytes().any(|b| b.is_ascii_digit()) {
                    return h.to_string();
                }
                let n = ids.len() + 1;
                let k = *ids.entry(h[..7].to_string()).or_insert(n);
                format!("#{k}")
            })
            .into_owned()
    }
}

fn matches(expected: &str, actual: &str) -> bool {
    match expected.strip_suffix(" (glob)") {
        Some(pat) => {
            let re = format!(
                "^{}$",
                pat.split('*')
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(".*")
            );
            regex::Regex::new(&re).unwrap().is_match(actual)
        }
        None => expected == actual,
    }
}

/// The file as it should read: each command's output replaced by what ran (glob lines that
/// still match are kept).
fn transcript(s: &Scenario, results: &[(Vec<String>, i32)], norm: &mut Normalizer) -> String {
    let mut out: Vec<String> = vec![];
    let mut next = 0;
    for (c, (lines, code)) in s.commands.iter().zip(results) {
        let script_lines = 1 + c.script.matches('\n').count();
        out.extend(s.lines[next..c.at + script_lines].iter().cloned());
        let mut actual: Vec<String> = lines.iter().map(|l| norm.line(l)).collect();
        if *code != 0 {
            actual.push(format!("[{code}]"));
        }
        for (i, a) in actual.iter().enumerate() {
            match c.expected.get(i) {
                Some(e) if matches(e, a) => out.push(format!("  {e}")),
                _ => out.push(format!("  {a}")),
            }
        }
        next = c.at + script_lines + c.expected.len();
    }
    out.extend(s.lines[next..].iter().cloned());
    let mut t = out.join("\n");
    t.push('\n');
    t
}

fn have(tool: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A diff of the first command whose output differs.
fn first_difference(s: &Scenario, want: &str, got: &str) -> String {
    let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let i = w.iter().zip(&g).take_while(|(a, b)| a == b).count();
    let cmd = s
        .commands
        .iter()
        .rev()
        .find(|c| c.at <= i)
        .map(|c| c.script.as_str())
        .unwrap_or("");
    let show = |v: &[&str]| {
        v.iter()
            .skip(i)
            .take(12)
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "line {}, in `$ {cmd}`\n  expected:\n{}\n  actual:\n{}",
        i + 1,
        show(&w),
        show(&g)
    )
}

#[test]
fn scenarios() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios");
    let filter = std::env::var("PAIL_SCENARIO").ok();
    let bless = std::env::var_os("PAIL_BLESS").is_some();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "t"))
        .filter(|p| {
            filter
                .as_ref()
                .is_none_or(|f| p.file_name().unwrap().to_string_lossy().contains(f))
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no scenarios matched");
    let failures: Vec<String> = std::thread::scope(|sc| {
        let handles: Vec<_> = files
            .iter()
            .map(|f| {
                sc.spawn(move || {
                    let name = f.file_name().unwrap().to_string_lossy().to_string();
                    let text = std::fs::read_to_string(f).unwrap();
                    let s = parse(&text);
                    if let Some(t) = s.requires.iter().find(|t| !have(t)) {
                        eprintln!("{name}: skipped ({t} not found)");
                        return None;
                    }
                    let tmp = tempfile::tempdir().unwrap();
                    let root = std::fs::canonicalize(tmp.path()).unwrap();
                    let results = run(&s, &root);
                    let got = transcript(&s, &results, &mut Normalizer::new(&root));
                    if got == text {
                        return None;
                    }
                    if bless {
                        std::fs::write(f, &got).unwrap();
                        eprintln!("{name}: blessed");
                        return None;
                    }
                    Some(format!("{name}: {}", first_difference(&s, &text, &got)))
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect()
    });
    assert!(
        failures.is_empty(),
        "{} scenario(s) differ (PAIL_BLESS=1 to accept the new output):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
