//! Configuration in Git's format and with Git's layering.
//!
//! ```text
//! global   $PAIL_CONFIG_GLOBAL, else $XDG_CONFIG_HOME/pail/config, else ~/.config/pail/config
//! local    <repository>/config            (the repository's own file, as .git/config)
//! ```
//!
//! Later layers win. Files use Git's syntax: `[section]` or `[section "sub"]` headers,
//! `key = value` lines, `#`/`;` comments, double-quoted values with `\"`, `\\`, `\n`, `\t`.
//! Section and key names are case-insensitive, subsection names are not.
//!
//! Keys pail reads: `user.name`, `user.email`, `init.defaultBranch`, `core.pager`,
//! `core.abbrev`, `color.ui`, `log.date`, `log.decorate`, `diff.renames`; and in a
//! repository's own file `core.store`, `core.bare`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Global,
    Local,
}

/// One config file: its lines, kept so edits preserve comments and layout.
#[derive(Clone, Debug, Default)]
pub struct File {
    pub path: Option<PathBuf>,
    lines: Vec<Line>,
}

#[derive(Clone, Debug)]
enum Line {
    Section { name: String, sub: Option<String> },
    Entry { key: String, value: String },
    Other(String),
}

/// `section.sub.name` → (section lowercased, sub, name lowercased).
fn split_key(key: &str) -> Result<(String, Option<String>, String)> {
    let (first, rest) = key
        .split_once('.')
        .with_context(|| format!("key does not contain a section: {key}"))?;
    let (sub, name) = match rest.rsplit_once('.') {
        Some((sub, name)) => (Some(sub.to_string()), name),
        None => (None, rest),
    };
    if first.is_empty()
        || name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        bail!("invalid key: {key}");
    }
    Ok((first.to_ascii_lowercase(), sub, name.to_ascii_lowercase()))
}

fn unquote(raw: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    let mut chars = raw.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(o) => out.push(o),
                None => {}
            },
            '#' | ';' if !quoted => break,
            _ => out.push(c),
        }
    }
    if quoted {
        out
    } else {
        out.trim_end().to_string()
    }
}

fn quote(v: &str) -> String {
    let needs = v.is_empty()
        || v.starts_with(' ')
        || v.ends_with(' ')
        || v.contains(['#', ';', '"', '\\', '\n', '\t']);
    if !needs {
        return v.to_string();
    }
    let esc = v
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t");
    format!("\"{esc}\"")
}

impl File {
    pub fn parse(text: &str) -> File {
        let mut lines = vec![];
        for raw in text.lines() {
            let t = raw.trim();
            if t.starts_with('[') && t.contains(']') {
                let inner = &t[1..t.find(']').unwrap()];
                let (name, sub) = match inner.split_once(char::is_whitespace) {
                    Some((n, s)) => (
                        n.to_ascii_lowercase(),
                        Some(s.trim().trim_matches('"').to_string()),
                    ),
                    None => match inner.split_once('.') {
                        // Legacy [section.sub] form.
                        Some((n, s)) => (n.to_ascii_lowercase(), Some(s.to_string())),
                        None => (inner.to_ascii_lowercase(), None),
                    },
                };
                lines.push(Line::Section { name, sub });
            } else if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
                lines.push(Line::Other(raw.to_string()));
            } else {
                let (k, v) = match t.split_once('=') {
                    Some((k, v)) => (k.trim(), unquote(v)),
                    None => (t, "true".to_string()),
                };
                lines.push(Line::Entry {
                    key: k.to_ascii_lowercase(),
                    value: v,
                });
            }
        }
        File { path: None, lines }
    }

    pub fn load(path: &Path) -> File {
        let mut f = File::parse(&std::fs::read_to_string(path).unwrap_or_default());
        f.path = Some(path.to_path_buf());
        f
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for l in &self.lines {
            match l {
                Line::Section { name, sub: Some(s) } => {
                    out.push_str(&format!("[{name} \"{s}\"]\n"))
                }
                Line::Section { name, sub: None } => out.push_str(&format!("[{name}]\n")),
                Line::Entry { key, value } => {
                    out.push_str(&format!("\t{key} = {}\n", quote(value)))
                }
                Line::Other(s) => {
                    out.push_str(s);
                    out.push('\n');
                }
            }
        }
        out
    }

    pub fn save(&self) -> Result<()> {
        let p = self.path.as_ref().context("config file has no path")?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, self.render())?;
        std::fs::rename(&tmp, p)?;
        Ok(())
    }

    /// Every (full key, value) in file order.
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut out = vec![];
        let mut section: Option<(String, Option<String>)> = None;
        for l in &self.lines {
            match l {
                Line::Section { name, sub } => section = Some((name.clone(), sub.clone())),
                Line::Entry { key, value } => {
                    if let Some((n, s)) = &section {
                        let full = match s {
                            Some(s) => format!("{n}.{s}.{key}"),
                            None => format!("{n}.{key}"),
                        };
                        out.push((full, value.clone()));
                    }
                }
                Line::Other(_) => {}
            }
        }
        out
    }

    fn section_range(&self, name: &str, sub: &Option<String>) -> Option<(usize, usize)> {
        let start = self.lines.iter().position(
            |l| matches!(l, Line::Section { name: n, sub: s } if n == name && s == sub),
        )?;
        let end = self.lines[start + 1..]
            .iter()
            .position(|l| matches!(l, Line::Section { .. }))
            .map(|i| start + 1 + i)
            .unwrap_or(self.lines.len());
        Some((start, end))
    }

    /// Set a key (replacing its last value in this file, or adding it).
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let (name, sub, k) = split_key(key)?;
        match self.section_range(&name, &sub) {
            Some((start, end)) => {
                let existing = (start + 1..end)
                    .rev()
                    .find(|&i| matches!(&self.lines[i], Line::Entry { key, .. } if *key == k));
                match existing {
                    Some(i) => {
                        self.lines[i] = Line::Entry {
                            key: k,
                            value: value.to_string(),
                        }
                    }
                    None => self.lines.insert(
                        end,
                        Line::Entry {
                            key: k,
                            value: value.to_string(),
                        },
                    ),
                }
            }
            None => {
                self.lines.push(Line::Section { name, sub });
                self.lines.push(Line::Entry {
                    key: k,
                    value: value.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Remove every value of a key; false if there was none.
    pub fn unset(&mut self, key: &str) -> Result<bool> {
        let (name, sub, k) = split_key(key)?;
        let Some((start, end)) = self.section_range(&name, &sub) else {
            return Ok(false);
        };
        let before = self.lines.len();
        let mut i = start + 1;
        let mut end = end;
        while i < end {
            if matches!(&self.lines[i], Line::Entry { key, .. } if *key == k) {
                self.lines.remove(i);
                end -= 1;
            } else {
                i += 1;
            }
        }
        Ok(self.lines.len() != before)
    }
}

pub fn global_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PAIL_CONFIG_GLOBAL") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("pail").join("config"))
}

/// The layered configuration: global, then the repository's own.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub global: File,
    pub local: Option<File>,
}

impl Config {
    /// Global config plus, when given, the repository's `config` file.
    pub fn load(repo_dir: Option<&Path>) -> Config {
        let global = global_path().map(|p| File::load(&p)).unwrap_or_default();
        let local = repo_dir.map(|d| File::load(&d.join("config")));
        Config { global, local }
    }

    /// The effective value of a key (the last one, local over global).
    pub fn get(&self, key: &str) -> Option<String> {
        let (name, sub, k) = split_key(key).ok()?;
        let want = match &sub {
            Some(s) => format!("{name}.{s}.{k}"),
            None => format!("{name}.{k}"),
        };
        let mut found = None;
        for f in std::iter::once(&self.global).chain(self.local.iter()) {
            for (key, v) in f.entries() {
                if key == want {
                    found = Some(v);
                }
            }
        }
        found
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key).map(|v| {
            matches!(
                v.to_ascii_lowercase().as_str(),
                "true" | "yes" | "on" | "1" | ""
            )
        })
    }

    /// Every effective entry, with its scope, in order (as `git config --list --show-scope`).
    pub fn list(&self) -> Vec<(Scope, String, String)> {
        let mut out: Vec<(Scope, String, String)> = self
            .global
            .entries()
            .into_iter()
            .map(|(k, v)| (Scope::Global, k, v))
            .collect();
        if let Some(l) = &self.local {
            out.extend(l.entries().into_iter().map(|(k, v)| (Scope::Local, k, v)));
        }
        out
    }

    pub fn file_mut(&mut self, scope: Scope) -> Result<&mut File> {
        match scope {
            Scope::Global => {
                if self.global.path.is_none() {
                    self.global.path = global_path();
                }
                Ok(&mut self.global)
            }
            Scope::Local => self
                .local
                .as_mut()
                .context("not in a pail repository (use --global)"),
        }
    }

    /// The default branch for new repositories.
    pub fn default_branch(&self) -> String {
        self.get("init.defaultBranch")
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "main".to_string())
    }

    /// `Name <email>` for a role (`author`/`committer`): `PAIL_<ROLE>_NAME`/`_EMAIL`, then
    /// `user.name`/`user.email`. `PAIL_AUTHOR` (`Name <email>`) overrides both roles.
    pub fn identity(&self, role: &str) -> Result<String> {
        if let Ok(a) = std::env::var("PAIL_AUTHOR") {
            return Ok(a);
        }
        let up = role.to_ascii_uppercase();
        let name = std::env::var(format!("PAIL_{up}_NAME"))
            .ok()
            .or_else(|| self.get("user.name"));
        let email = std::env::var(format!("PAIL_{up}_EMAIL"))
            .ok()
            .or_else(|| self.get("user.email"));
        match (name, email) {
            (Some(n), Some(e)) if !n.trim().is_empty() => {
                Ok(format!("{} <{}>", n.trim(), e.trim()))
            }
            _ => bail!(
                "Author identity unknown\n\n*** Please tell me who you are.\n\nRun\n\n  pail config --global user.email \"you@example.com\"\n  pail config --global user.name \"Your Name\"\n\nto set your account's default identity.\nOmit --global to set the identity only in this repository."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_set_unset_roundtrip() {
        let mut f = File::parse(
            "# mine\n[user]\n\tname = Ada \"The\" Lovelace\n\temail = ada@x ; work\n[branch \"Feature\"]\n\tremote = origin\n",
        );
        assert_eq!(
            f.entries(),
            vec![
                ("user.name".into(), "Ada The Lovelace".into()),
                ("user.email".into(), "ada@x".into()),
                ("branch.Feature.remote".into(), "origin".into()),
            ]
        );
        f.set("user.NAME", "Grace # Hopper").unwrap();
        f.set("init.defaultBranch", "trunk").unwrap();
        let again = File::parse(&f.render());
        assert_eq!(
            again.entries()[0],
            ("user.name".into(), "Grace # Hopper".into())
        );
        assert!(
            again
                .entries()
                .contains(&("init.defaultbranch".into(), "trunk".into()))
        );
        assert!(f.render().starts_with("# mine\n"));
        assert!(f.unset("user.email").unwrap());
        assert!(
            !File::parse(&f.render())
                .entries()
                .iter()
                .any(|(k, _)| k == "user.email")
        );
    }
}
