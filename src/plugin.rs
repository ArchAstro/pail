//! Plugins, the way `git` and `cargo` have them: `pail <name> args…` runs `pail-<name>` from
//! PATH when `<name>` is not a built-in command. Built-ins always win.
//!
//! The plugin gets the store pail resolved in `PAIL_STORE` and its own binary in `PAIL_BIN`, so
//! it can call `pail` for everything (the plugins are programs that only use the CLI). The exit
//! code is the plugin's.

use std::path::{Path, PathBuf};

/// `pail-credential-<name>` programs are credential providers, not plugins.
const NOT_PLUGINS: &str = "credential-";
/// pail's own helper binaries, installed beside it.
const OWN_BINARIES: [&str; 3] = ["bench", "gcs-emulator", "mirror"];

fn is_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with(NOT_PLUGINS)
        && !OWN_BINARIES.contains(&name)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_executable(p: &Path) -> bool {
    let Ok(m) = std::fs::metadata(p) else {
        return false;
    };
    if !m.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// The program that `pail <name>` would run.
pub fn find(name: &str) -> Option<PathBuf> {
    if !is_plugin_name(name) {
        return None;
    }
    let file = format!("pail-{name}");
    path_dirs()
        .into_iter()
        .map(|d| d.join(&file))
        .find(|p| is_executable(p))
}

/// Every plugin on PATH: (name, path), sorted, the first on PATH winning.
pub fn list() -> Vec<(String, PathBuf)> {
    let mut found = std::collections::BTreeMap::new();
    for d in path_dirs() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let file = e.file_name().to_string_lossy().to_string();
            let Some(name) = file.strip_prefix("pail-") else {
                continue;
            };
            let name = name.strip_suffix(".exe").unwrap_or(name);
            if is_plugin_name(name) && is_executable(&e.path()) {
                found.entry(name.to_string()).or_insert_with(|| e.path());
            }
        }
    }
    found.into_iter().collect()
}

/// What the global flags before the command name say.
pub struct Split<'a> {
    /// `--store` if given.
    pub store: Option<String>,
    /// The first argument that is not a global flag, and where it is.
    pub name: Option<(usize, &'a str)>,
}

/// Find the command word in `args` (without argv[0]), skipping pail's global flags.
pub fn split(args: &[String]) -> Split<'_> {
    let mut store = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--store" => {
                store = args.get(i + 1).cloned();
                i += 2;
            }
            "--repo" | "--ids" => i += 2,
            "--skill" | "--list" | "--no-pager" | "-P" => i += 1,
            _ if a.starts_with("--store=") => {
                store = Some(a["--store=".len()..].to_string());
                i += 1;
            }
            _ if a.starts_with("--repo=") || a.starts_with("--ids=") => i += 1,
            _ if a.starts_with('-') => return Split { store, name: None },
            _ => {
                return Split {
                    store,
                    name: Some((i, a)),
                };
            }
        }
    }
    Split { store, name: None }
}

/// The store a plugin should use: `--store`, `PAIL_STORE`, else the working copy or
/// repository the current directory is in.
fn resolve_store(flag: Option<String>) -> Option<String> {
    let s = flag
        .or_else(|| std::env::var("PAIL_STORE").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            let cwd = std::fs::canonicalize(std::env::current_dir().ok()?).ok()?;
            match crate::local::discover(&cwd).ok()? {
                crate::local::Found::Worktree(_, l) | crate::local::Found::Bare(l) => {
                    Some(l.config.store.clone())
                }
            }
        })?;
    crate::rev::store_url(&s).ok()
}

/// Run `pail-<name>` with `rest` as its arguments and never return. `args` are the arguments
/// to pail itself, for `--store`.
pub fn exec(program: &Path, args: &[String], rest: &[String]) -> ! {
    let mut c = std::process::Command::new(program);
    c.args(rest);
    if let Some(store) = resolve_store(split(args).store) {
        c.env("PAIL_STORE", store);
    }
    if let Ok(me) = std::env::current_exe() {
        c.env("PAIL_BIN", me);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = c.exec();
        eprintln!("error: running {}: {e}", program.display());
        std::process::exit(127);
    }
    #[cfg(not(unix))]
    match c.status() {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("error: running {}: {e}", program.display());
            std::process::exit(127);
        }
    }
}

/// The "Plugins" block of `pail help`, empty when there are none.
pub fn help_section() -> String {
    let plugins = list();
    if plugins.is_empty() {
        return String::new();
    }
    let mut s = String::from("\nPlugins (pail-<name> on PATH; `pail <name> --help`)\n");
    for (name, _) in plugins {
        s.push_str(&format!("  {name:25} pail-{name}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn splits_global_flags_from_the_command() {
        let a = v(&["--store", "file:///x", "-P", "pr", "list"]);
        let s = split(&a);
        assert_eq!(s.store.as_deref(), Some("file:///x"));
        assert_eq!(s.name, Some((3, "pr")));
        let a = v(&["--store=gs://b/r", "log"]);
        assert_eq!(split(&a).name, Some((1, "log")));
        assert!(split(&v(&["--bogus", "pr"])).name.is_none());
    }

    #[test]
    fn plugin_names_are_plain() {
        assert!(is_plugin_name("pr"));
        assert!(!is_plugin_name("../x"));
        assert!(!is_plugin_name("credential-gcloud"));
        assert!(!is_plugin_name(""));
    }
}
