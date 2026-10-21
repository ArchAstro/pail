//! Credentials for a store, from an installable provider: most people have no cloud
//! credentials, so pail asks an extension that can mint short-lived, delegated ones. Pail
//! itself knows only the protocol; which identity backs a credential (ArchAstro, gcloud,
//! a company SSO) and how narrowly it is scoped is the extension's business.
//!
//! ```text
//! extension   an executable named pail-credential-<name>, found in $PAIL_EXTENSIONS_DIR,
//!             else ~/.local/share/pail/extensions, else $PATH
//!               <exe> get        JSON request on stdin, credential on stdout   (required)
//!               <exe> describe   one line for `pail auth list`                 (optional)
//! ambient     no provider: the cloud SDK's chain (gcloud ADC, GOOGLE_*, AWS_*)
//! ```
//!
//! Chosen per store, Git-style, most specific URL prefix first, then the bare section:
//!
//! ```ini
//! [credential "gs://acme-vcs"]
//!     provider = archdev          # runs pail-credential-archdev
//! [credential "https://vcs.acme.dev"]
//!     helper = acme-sso token     # an uninstalled command, run as `<helper> get`
//! ```
//!
//! `PAIL_CREDENTIAL_PROVIDER=<name>` or `PAIL_CREDENTIAL_HELPER=<cmd>` override config. The
//! protocol (`pail-credential/1`):
//!
//! ```text
//! stdin   {"protocol":"pail-credential/1","store":"gs://acme-vcs/app","scheme":"gs"}
//! stdout  {"token":"ya29…","expires_at":1792530000}                 GCS, https:// proxies
//!         {"aws":{"access_key_id":…,"secret_access_key":…,"session_token":…},"expires_at":…}
//!         or a bare token line (assumed to live 45 minutes)
//! ```
//!
//! Credentials are cached until a minute before `expires_at` in
//! `$XDG_CACHE_HOME/pail/credentials/` (0600), so commands don't pay for the extension each
//! run; `pail auth logout` clears it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;

pub const PROTOCOL: &str = "pail-credential/1";
pub const PREFIX: &str = "pail-credential-";
/// A bare token's assumed lifetime (Google access tokens live an hour).
const BARE_TOKEN_SECS: u64 = 45 * 60;

/// Extensions that ship with pail and `pail auth install <name>` writes out.
pub const BUNDLED: &[(&str, &str)] = &[
    (
        "archdev",
        include_str!("../extensions/pail-credential-archdev"),
    ),
    (
        "gcloud",
        include_str!("../extensions/pail-credential-gcloud"),
    ),
    (
        "gcs-downscoped",
        include_str!("../extensions/pail-credential-gcs-downscoped"),
    ),
];

#[derive(Clone, Debug, PartialEq)]
pub enum Provider {
    Ambient,
    /// An installed extension.
    Extension {
        name: String,
        exe: PathBuf,
    },
    /// A command from `credential.helper`, run as `sh -c '<cmd> get'`.
    Command(String),
}

impl Provider {
    pub fn describe(&self) -> String {
        match self {
            Provider::Ambient => "ambient (the cloud SDK's credential chain)".into(),
            Provider::Extension { name, exe } => format!("{name} ({})", exe.display()),
            Provider::Command(c) => format!("command `{c}`"),
        }
    }
}

/// The provider for `store`, and the config key (or variable) that chose it.
#[derive(Clone, Debug)]
pub struct Choice {
    pub provider: Provider,
    pub source: String,
}

/// Where `pail auth install` puts extensions (searched before $PATH).
pub fn extensions_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("PAIL_EXTENSIONS_DIR") {
        return Some(PathBuf::from(d));
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("pail").join("extensions"))
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The executable for provider `name`.
pub fn find_extension(name: &str) -> Option<PathBuf> {
    let file = format!("{PREFIX}{name}");
    let path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    extensions_dir()
        .into_iter()
        .chain(path)
        .map(|d| d.join(&file))
        .find(|p| executable(p))
}

/// Every installed provider: (name, executable), extensions dir first, then $PATH.
pub fn installed() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = vec![];
    let path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    for dir in extensions_dir().into_iter().chain(path) {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let f = e.file_name().to_string_lossy().to_string();
            if let Some(name) = f.strip_prefix(PREFIX) {
                if !name.contains('.')
                    && executable(&e.path())
                    && !out.iter().any(|(n, _)| n == name)
                {
                    out.push((name.to_string(), e.path()));
                }
            }
        }
    }
    out.sort();
    out
}

fn by_name(name: &str, source: &str) -> Result<Provider> {
    if name == "ambient" {
        return Ok(Provider::Ambient);
    }
    let exe = find_extension(name).with_context(|| {
        format!("credential provider `{name}` ({source}) is not installed: no {PREFIX}{name} (try `pail auth install {name}`)")
    })?;
    Ok(Provider::Extension {
        name: name.to_string(),
        exe,
    })
}

pub fn choose(store: &str, cfg: &Config) -> Result<Choice> {
    for var in ["PAIL_CREDENTIAL_PROVIDER", "PAIL_CREDENTIAL_HELPER"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                let provider = if var == "PAIL_CREDENTIAL_HELPER" {
                    Provider::Command(v)
                } else {
                    by_name(v.trim(), var)?
                };
                return Ok(Choice {
                    provider,
                    source: var.into(),
                });
            }
        }
    }
    // Most specific `credential.<url>` section whose URL prefixes the store, then `credential`.
    let mut best: Option<(usize, String)> = None;
    for (_, key, _) in cfg.list() {
        let Some(rest) = key.strip_prefix("credential.") else {
            continue;
        };
        let Some((url, _)) = rest.rsplit_once('.') else {
            continue;
        };
        let u = url.trim_end_matches('/');
        let prefixes = store == u || store.starts_with(&format!("{u}/"));
        if prefixes && best.as_ref().is_none_or(|(n, _)| u.len() > *n) {
            best = Some((u.len(), url.to_string()));
        }
    }
    let sections: Vec<String> = best
        .map(|(_, u)| format!("credential.{u}."))
        .into_iter()
        .chain(["credential.".to_string()])
        .collect();
    for sec in sections {
        let source = sec.trim_end_matches('.').to_string();
        if let Some(name) = cfg.get(&format!("{sec}provider")).filter(|n| !n.is_empty()) {
            return Ok(Choice {
                provider: by_name(&name, &format!("{source}.provider"))?,
                source,
            });
        }
        if let Some(cmd) = cfg.get(&format!("{sec}helper")).filter(|c| !c.is_empty()) {
            return Ok(Choice {
                provider: Provider::Command(cmd),
                source,
            });
        }
    }
    Ok(Choice {
        provider: Provider::Ambient,
        source: "default".into(),
    })
}

/// What a provider says about itself (`describe`), if anything.
pub fn description(exe: &Path) -> Option<String> {
    let out = Command::new(exe)
        .arg("describe")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AwsKeys {
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
}

/// What a helper hands back.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Credential {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws: Option<AwsKeys>,
    /// Unix seconds.
    pub expires_at: u64,
}

impl Credential {
    fn fresh(&self) -> bool {
        self.expires_at > now() + 60
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Runs a provider and caches what it returns, in memory and on disk.
#[derive(Debug)]
pub struct Source {
    provider: Provider,
    store: String,
    mem: Mutex<Option<Credential>>,
}

impl Source {
    /// None for the ambient provider: the cloud SDK finds its own credentials.
    pub fn new(provider: &Provider, store: &str) -> Option<Arc<Source>> {
        (*provider != Provider::Ambient).then(|| {
            Arc::new(Source {
                provider: provider.clone(),
                store: store.to_string(),
                mem: Mutex::new(None),
            })
        })
    }

    fn cache_path(&self) -> Option<PathBuf> {
        let key = blake3::hash(format!("{:?}\n{}", self.provider, self.store).as_bytes()).to_hex();
        Some(cache_dir()?.join(&key[..32]))
    }

    /// Forget the cached credential (memory and disk): the store refused it.
    pub fn invalidate(&self) {
        *self.mem.lock().unwrap() = None;
        if let Some(p) = self.cache_path() {
            let _ = std::fs::remove_file(p);
        }
    }

    /// A fresh credential: memory, then disk, then the helper.
    pub fn get(&self) -> Result<Credential> {
        if let Some(c) = self.mem.lock().unwrap().clone().filter(Credential::fresh) {
            return Ok(c);
        }
        if let Some(c) = self
            .cache_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Credential>(&b).ok())
        {
            if c.fresh() {
                *self.mem.lock().unwrap() = Some(c.clone());
                return Ok(c);
            }
        }
        let c = self.run()?;
        if let Some(p) = self.cache_path() {
            let _ = write_private(&p, &serde_json::to_vec(&c)?);
        }
        *self.mem.lock().unwrap() = Some(c.clone());
        Ok(c)
    }

    fn run(&self) -> Result<Credential> {
        let scheme = self.store.split("://").next().unwrap_or("");
        let req = serde_json::json!({"protocol": PROTOCOL, "store": self.store, "scheme": scheme});
        let mut cmd = match &self.provider {
            Provider::Extension { exe, .. } => {
                let mut c = Command::new(exe);
                c.arg("get");
                c
            }
            Provider::Command(c) => {
                let mut sh = Command::new("sh");
                sh.args(["-c", &format!("{c} get")]);
                sh
            }
            Provider::Ambient => bail!("the ambient provider has no command"),
        };
        let what = self.provider.describe();
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("running credential provider {what}"))?;
        let _ = child
            .stdin
            .take()
            .expect("piped")
            .write_all(format!("{req}\n").as_bytes());
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "credential provider {what} failed ({}) for {}",
                out.status,
                self.store
            );
        }
        parse(&out.stdout).with_context(|| format!("credential provider {what} for {}", self.store))
    }
}

/// A provider's answer: JSON, or a bare token.
pub fn parse(out: &[u8]) -> Result<Credential> {
    let text = String::from_utf8_lossy(out);
    let text = text.trim();
    if text.starts_with('{') {
        let c: Credential = serde_json::from_str(text).context("bad credential JSON")?;
        if c.token.is_none() && c.aws.is_none() {
            bail!("credential has neither a token nor aws keys");
        }
        return Ok(c);
    }
    if text.is_empty() || text.contains(char::is_whitespace) {
        bail!("expected a JSON credential or a bare token");
    }
    Ok(Credential {
        token: Some(text.to_string()),
        aws: None,
        expires_at: now() + BARE_TOKEN_SECS,
    })
}

pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("pail").join("credentials"))
}

/// Forget every cached credential.
pub fn clear_cache() -> Result<usize> {
    let Some(dir) = cache_dir() else { return Ok(0) };
    let mut n = 0;
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        std::fs::remove_file(e.path())?;
        n += 1;
    }
    Ok(n)
}

fn write_private(p: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = p.parent().unwrap();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

// ---------- object_store adapters ----------

async fn fetch(src: &Arc<Source>) -> object_store::Result<Credential> {
    let s = src.clone();
    tokio::task::spawn_blocking(move || s.get())
        .await
        .map_err(|e| generic(e.to_string()))?
        .map_err(|e| generic(format!("{e:#}")))
}

fn generic(msg: String) -> object_store::Error {
    object_store::Error::Generic {
        store: "pail credential",
        source: msg.into(),
    }
}

#[derive(Debug)]
pub struct GcsProvider(pub Arc<Source>);

#[async_trait::async_trait]
impl object_store::CredentialProvider for GcsProvider {
    type Credential = object_store::gcp::GcpCredential;
    async fn get_credential(&self) -> object_store::Result<Arc<Self::Credential>> {
        let c = fetch(&self.0).await?;
        let bearer = c
            .token
            .ok_or_else(|| generic("the credential has no token for GCS".into()))?;
        Ok(Arc::new(object_store::gcp::GcpCredential { bearer }))
    }
}

#[derive(Debug)]
pub struct AwsProvider(pub Arc<Source>);

#[async_trait::async_trait]
impl object_store::CredentialProvider for AwsProvider {
    type Credential = object_store::aws::AwsCredential;
    async fn get_credential(&self) -> object_store::Result<Arc<Self::Credential>> {
        let c = fetch(&self.0).await?;
        let k = c
            .aws
            .ok_or_else(|| generic("the credential has no aws keys for S3".into()))?;
        Ok(Arc::new(object_store::aws::AwsCredential {
            key_id: k.access_key_id,
            secret_key: k.secret_access_key,
            token: k.session_token,
        }))
    }
}

/// Bearer tokens for `https://` stores.
#[derive(Debug)]
pub struct BearerProvider(pub Arc<Source>);

impl BearerProvider {
    pub async fn token(&self) -> object_store::Result<String> {
        fetch(&self.0)
            .await?
            .token
            .ok_or_else(|| generic("the credential has no token".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config"), text).unwrap();
        Config::load(Some(dir.path()))
    }

    #[test]
    fn the_most_specific_store_section_wins() {
        let ext = tempfile::tempdir().unwrap();
        for name in ["acme", "corp"] {
            let p = ext.path().join(format!("{PREFIX}{name}"));
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // SAFETY: tests in this module run with this variable set the same way.
        unsafe { std::env::set_var("PAIL_EXTENSIONS_DIR", ext.path()) };
        let c = cfg(
            "[credential]\n\tprovider = corp\n[credential \"gs://acme\"]\n\tprovider = acme\n[credential \"gs://acme/secret\"]\n\tprovider = ambient\n[credential \"https://vcs.acme.dev\"]\n\thelper = sso token\n",
        );
        let p = |u: &str| match choose(u, &c).unwrap().provider {
            Provider::Extension { name, .. } => name,
            Provider::Ambient => "ambient".into(),
            Provider::Command(c) => format!("cmd:{c}"),
        };
        assert_eq!(p("gs://acme/app"), "acme");
        assert_eq!(p("gs://acme/secret/x"), "ambient");
        assert_eq!(
            p("gs://acme-other/app"),
            "corp",
            "a prefix of the name is not a prefix of the path"
        );
        assert_eq!(p("https://vcs.acme.dev/app"), "cmd:sso token");
        assert_eq!(
            choose("s3://x/y", &cfg("")).unwrap().provider,
            Provider::Ambient
        );
        let missing = choose("gs://x/y", &cfg("[credential]\n\tprovider = nope\n"))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("pail auth install nope"), "{missing}");
    }

    #[test]
    fn helpers_answer_json_or_a_bare_token() {
        let c = parse(br#"{"token":"t","expires_at":5}"#).unwrap();
        assert_eq!((c.token.as_deref(), c.expires_at), (Some("t"), 5));
        let c = parse(br#"{"aws":{"access_key_id":"k","secret_access_key":"s"},"expires_at":9}"#)
            .unwrap();
        assert_eq!(c.aws.unwrap().access_key_id, "k");
        let c = parse(b"ya29.abc\n").unwrap();
        assert_eq!(c.token.as_deref(), Some("ya29.abc"));
        assert!(c.expires_at > now());
        assert!(parse(b"").is_err());
        assert!(parse(br#"{"expires_at":1}"#).is_err());
        assert!(parse(b"two words").is_err());
    }
}
