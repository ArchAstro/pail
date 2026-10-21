//! Installing credential providers from GitHub, the way `gh extension install` does.
//!
//! A provider lives in its own repository named `pail-credential-<name>`. The repository's
//! `pail-extension.json` says what to install:
//!
//! ```json
//! {"name": "gcloud", "protocol": "pail-credential/1", "description": "…",
//!  "script": "pail-credential-gcloud"}                       a script in the repository
//! {"name": "sso", "protocol": "pail-credential/1",
//!  "binaries": {"darwin-arm64": "pail-credential-sso-darwin-arm64", …}}   release assets
//! ```
//!
//! Without a manifest, gh's conventions apply: an executable `pail-credential-<name>` at the
//! repository root, else a release asset `pail-credential-<name>-<os>-<arch>`.
//!
//! ```text
//! pail auth install gcloud                  ArchAstro/pail-credential-gcloud (official)
//! pail auth install acme/pail-credential-sso[@ref]
//! pail auth install ./pail-credential-x     a local file
//! ```
//!
//! The ref is the one given (pinned), else the latest release's tag, else the default branch.
//! It is resolved to a commit before anything is read, so a script comes from exactly one
//! commit. Each install lands in `<extensions>/pail-credential-<name>/` with a
//! `manifest.json` recording where it came from, the commit and the file's sha256;
//! `pail auth upgrade` re-resolves unpinned installs.
//!
//! GitHub is reached at `$PAIL_GITHUB_API` (default `https://api.github.com`, or a GitHub
//! Enterprise API) with `$GH_TOKEN`, `$GITHUB_TOKEN` or `gh auth token` when present, so
//! private repositories work for people who can read them.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::creds::{PREFIX, PROTOCOL, extensions_dir};

/// The owner `pail auth install <name>` reads official providers from.
pub const OFFICIAL_OWNER: &str = "ArchAstro";
pub const REPO_MANIFEST: &str = "pail-extension.json";
pub const INSTALL_MANIFEST: &str = "manifest.json";

/// A repository's `pail-extension.json`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RepoManifest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Path of a script in the repository.
    #[serde(default)]
    pub script: Option<String>,
    /// `<os>-<arch>` → release asset name.
    #[serde(default)]
    pub binaries: std::collections::BTreeMap<String, String>,
}

/// What `manifest.json` records about an installed provider.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Installed {
    pub name: String,
    /// `github` or `local`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The ref asked for (`@ref`), or the release tag / default branch it resolved from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    /// `script` or `binary`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub sha256: String,
    pub installed_at: u64,
}

/// Where a provider comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Local(PathBuf),
    GitHub {
        owner: String,
        repo: String,
        r#ref: Option<String>,
    },
}

/// `./file` (or any existing path), `owner/pail-credential-<name>[@ref]`, or `<name>[@ref]`
/// for an official provider.
pub fn parse_source(s: &str) -> Result<Source> {
    let p = Path::new(s);
    if s.starts_with('.') || s.starts_with('/') || s.starts_with('~') || p.exists() {
        return Ok(Source::Local(p.to_path_buf()));
    }
    let (spec, r#ref) = match s.split_once('@') {
        Some((a, r)) if !r.is_empty() => (a, Some(r.to_string())),
        Some(_) => bail!("`{s}`: empty ref after @"),
        None => (s, None),
    };
    let ok = |x: &str| {
        !x.is_empty()
            && x.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    let (owner, repo) = match spec.split_once('/') {
        Some((o, r)) => (o.to_string(), r.to_string()),
        None => (
            OFFICIAL_OWNER.to_string(),
            format!("{PREFIX}{}", spec.strip_prefix(PREFIX).unwrap_or(spec)),
        ),
    };
    if !ok(&owner) || !ok(&repo) || repo.contains('/') {
        bail!("`{s}` is not a file, a provider name or owner/{PREFIX}<name>");
    }
    if !repo.starts_with(PREFIX) || repo.len() == PREFIX.len() {
        bail!("{owner}/{repo}: provider repositories are named {PREFIX}<name>");
    }
    Ok(Source::GitHub { owner, repo, r#ref })
}

/// `<os>-<arch>` in release-asset naming (darwin-arm64, linux-amd64, …).
pub fn platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        o => o,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    };
    format!("{os}-{arch}")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The directory an installed provider lives in.
pub fn install_dir(name: &str) -> Result<PathBuf> {
    Ok(extensions_dir()
        .context("no extensions directory (set HOME or PAIL_EXTENSIONS_DIR)")?
        .join(format!("{PREFIX}{name}")))
}

/// What `manifest.json` says about provider `name`, if it was installed by `pail auth install`.
pub fn installed_manifest(name: &str) -> Option<Installed> {
    let p = install_dir(name).ok()?.join(INSTALL_MANIFEST);
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

/// Write `body` as provider `name` with its manifest. A flat `pail-credential-<name>` from
/// an older pail is replaced.
pub fn write_install(name: &str, body: &[u8], mut m: Installed, force: bool) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if !valid_name(name) {
        bail!("provider names are letters, digits, - and _ (got {name:?}); pass --name");
    }
    let dir = install_dir(name)?;
    let exe = dir.join(format!("{PREFIX}{name}"));
    if (exe.exists() || dir.is_file()) && !force {
        bail!(
            "{name} is already installed at {} (--force replaces it, `pail auth upgrade {name}` updates it)",
            exe.display()
        );
    }
    if dir.is_file() {
        std::fs::remove_file(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    // Write beside, then rename, so a running pail never sees half a provider.
    let tmp = dir.join(format!(".{PREFIX}{name}.tmp"));
    std::fs::write(&tmp, body)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &exe)?;
    m.name = name.to_string();
    m.sha256 = sha256_hex(body);
    m.installed_at = now();
    std::fs::write(
        dir.join(INSTALL_MANIFEST),
        serde_json::to_string_pretty(&m)? + "\n",
    )?;
    Ok(exe)
}

/// GitHub's REST API, as the person running pail.
pub struct GitHub {
    api: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl GitHub {
    pub fn new() -> Result<GitHub> {
        let api =
            std::env::var("PAIL_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into());
        let token = ["GH_TOKEN", "GITHUB_TOKEN"]
            .iter()
            .find_map(|v| std::env::var(v).ok().filter(|t| !t.trim().is_empty()))
            .or_else(|| {
                // Like gh itself: reuse the gh login when there is one.
                let o = std::process::Command::new("gh")
                    .args(["auth", "token"])
                    .stderr(std::process::Stdio::null())
                    .output()
                    .ok()?;
                let t = String::from_utf8(o.stdout).ok()?.trim().to_string();
                (o.status.success() && !t.is_empty()).then_some(t)
            });
        let http = reqwest::Client::builder()
            .user_agent(concat!("pail/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(GitHub {
            api: api.trim_end_matches('/').to_string(),
            token,
            http,
        })
    }

    async fn get(&self, path: &str, accept: &str) -> Result<Option<Vec<u8>>> {
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{path}", self.api)
        };
        let mut req = self
            .http
            .get(&url)
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = resp.bytes().await?.to_vec();
        if !status.is_success() {
            let msg = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| String::from_utf8_lossy(&body).chars().take(200).collect());
            bail!("GET {url}: {status}: {msg}");
        }
        Ok(Some(body))
    }

    async fn json(&self, path: &str) -> Result<Option<serde_json::Value>> {
        match self.get(path, "application/vnd.github+json").await? {
            Some(b) => Ok(Some(serde_json::from_slice(&b)?)),
            None => Ok(None),
        }
    }
}

/// What a GitHub source resolved to and the bytes to install.
pub struct Fetched {
    pub name: String,
    pub body: Vec<u8>,
    pub manifest: Installed,
    pub description: Option<String>,
}

/// Resolve and download a provider from GitHub.
pub async fn fetch(owner: &str, repo: &str, want: Option<&str>) -> Result<Fetched> {
    let gh = GitHub::new()?;
    let full = format!("{owner}/{repo}");
    let not_found = || {
        anyhow::anyhow!(
            "{full}: repository not found (if it is private, sign in with `gh auth login` or set GH_TOKEN)"
        )
    };
    let info = gh
        .json(&format!("/repos/{full}"))
        .await?
        .ok_or_else(not_found)?;
    // The ref: as given, else the latest release, else the default branch.
    let (r#ref, release) = match want {
        Some(r) => (
            r.to_string(),
            gh.json(&format!("/repos/{full}/releases/tags/{r}")).await?,
        ),
        None => match gh.json(&format!("/repos/{full}/releases/latest")).await? {
            Some(rel) => (
                rel["tag_name"]
                    .as_str()
                    .context("release without a tag")?
                    .to_string(),
                Some(rel),
            ),
            None => (
                info["default_branch"]
                    .as_str()
                    .unwrap_or("main")
                    .to_string(),
                None,
            ),
        },
    };
    let commit = gh
        .get(
            &format!("/repos/{full}/commits/{ref}"),
            "application/vnd.github.sha",
        )
        .await?
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .with_context(|| format!("{full}: no ref {ref:?}"))?;
    let raw = async |path: String| {
        gh.get(
            &format!("/repos/{full}/contents/{path}?ref={commit}"),
            "application/vnd.github.raw",
        )
        .await
    };
    let manifest: RepoManifest = match raw(REPO_MANIFEST.into()).await? {
        Some(b) => serde_json::from_slice(&b)
            .with_context(|| format!("{full}: invalid {REPO_MANIFEST}"))?,
        None => RepoManifest::default(),
    };
    if let Some(p) = &manifest.protocol {
        if p != PROTOCOL {
            bail!("{full} speaks {p}; this pail speaks {PROTOCOL}");
        }
    }
    let name = manifest
        .name
        .clone()
        .unwrap_or_else(|| repo.strip_prefix(PREFIX).unwrap_or(repo).to_string());
    let exe_name = format!("{PREFIX}{name}");
    let plat = platform();
    let asset_name = manifest
        .binaries
        .get(&plat)
        .cloned()
        .unwrap_or_else(|| format!("{exe_name}-{plat}"));
    let mut m = Installed {
        name: name.clone(),
        source: "github".into(),
        repo: Some(full.clone()),
        r#ref: Some(r#ref.clone()),
        commit: Some(commit.clone()),
        pinned: want.is_some(),
        kind: String::new(),
        path: None,
        sha256: String::new(),
        installed_at: 0,
    };
    // A script, unless the manifest only lists binaries.
    let script = manifest
        .script
        .clone()
        .or_else(|| manifest.binaries.is_empty().then(|| exe_name.clone()));
    if let Some(path) = script {
        if let Some(body) = raw(path.clone()).await? {
            m.kind = "script".into();
            m.path = Some(path);
            return Ok(Fetched {
                name,
                body,
                manifest: m,
                description: manifest.description,
            });
        }
        if manifest.script.is_some() {
            bail!(
                "{full}@{ref}: {REPO_MANIFEST} names script {path:?}, which is not in the repository"
            );
        }
    }
    let rel = release.with_context(|| {
        format!("{full}@{ref}: no script {exe_name} in the repository and no release to download a binary from")
    })?;
    let asset = rel["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str() == Some(asset_name.as_str()))
        .with_context(|| {
            let names: Vec<&str> = rel["assets"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a["name"].as_str())
                .collect();
            format!(
                "{full}@{ref}: no release asset {asset_name} for {plat} (assets: {})",
                names.join(", ")
            )
        })?;
    let url = asset["url"].as_str().context("asset without a url")?;
    let body = gh
        .get(url, "application/octet-stream")
        .await?
        .with_context(|| format!("{full}: {asset_name} vanished"))?;
    m.kind = "binary".into();
    m.path = Some(asset_name);
    Ok(Fetched {
        name,
        body,
        manifest: m,
        description: manifest.description,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources() {
        assert_eq!(
            parse_source("gcloud").unwrap(),
            Source::GitHub {
                owner: "ArchAstro".into(),
                repo: "pail-credential-gcloud".into(),
                r#ref: None
            }
        );
        assert_eq!(
            parse_source("acme/pail-credential-sso@v1.2").unwrap(),
            Source::GitHub {
                owner: "acme".into(),
                repo: "pail-credential-sso".into(),
                r#ref: Some("v1.2".into())
            }
        );
        assert_eq!(parse_source("./x").unwrap(), Source::Local("./x".into()));
        assert!(parse_source("acme/sso").is_err());
        assert!(parse_source("gcloud@").is_err());
    }
}
