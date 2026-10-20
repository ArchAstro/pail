//! The bucket. Immutable content-addressed objects plus compare-and-swap on refs.
//!
//! Remote stores (S3, GCS) go through `object_store` and use native conditional writes:
//! S3 `If-Match`/`If-None-Match`, GCS `ifGenerationMatch`. The local `file://` store
//! implements the same CAS contract with a lock file so tests and laptops behave identically.

use std::ops::Range;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{
    GetOptions, ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion,
};
use serde::{Deserialize, Serialize};

/// Opaque version token of a mutable object (S3 ETag, GCS generation, local content hash).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

pub struct ObjInfo {
    pub key: String,
    pub size: u64,
    pub modified: i64,
}

pub enum Cas {
    Ok(Version),
    Conflict,
}

/// Request counters, so callers can report cost (requests drive object-store cost).
#[derive(Default, Debug)]
pub struct Stats {
    pub gets: AtomicU64,
    pub puts: AtomicU64,
    /// Compare-and-swap writes (ref updates).
    pub cas: AtomicU64,
    pub bytes_down: AtomicU64,
    pub bytes_up: AtomicU64,
}

impl Stats {
    pub fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.gets.load(Ordering::Relaxed),
            self.puts.load(Ordering::Relaxed),
            self.bytes_down.load(Ordering::Relaxed),
            self.bytes_up.load(Ordering::Relaxed),
        )
    }
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<dyn ObjectStore>,
    prefix: String,
    /// Set for `file://` stores; CAS is implemented locally.
    local_root: Option<PathBuf>,
    pub url: String,
    pub stats: Arc<Stats>,
}

impl Store {
    /// Open `file:///abs/dir`, `s3://bucket/prefix` or `gs://bucket/prefix`.
    ///
    /// S3 honours the standard `AWS_*` env vars (`AWS_ENDPOINT`, `AWS_ALLOW_HTTP`, ...).
    /// GCS honours `GOOGLE_*` env vars; `PAIL_GCS_TOKEN` supplies an OAuth token (e.g. from
    /// `gcloud auth print-access-token`); `PAIL_GCS_BASE_URL` points it at an emulator.
    pub fn open(url: &str) -> Result<Store> {
        let parsed = url::Url::parse(url).with_context(|| format!("invalid store url {url}"))?;
        let prefix = parsed.path().trim_matches('/').to_string();
        let stats = Arc::new(Stats::default());
        match parsed.scheme() {
            "file" => {
                let root = PathBuf::from(parsed.path());
                std::fs::create_dir_all(&root)?;
                let fs = LocalFileSystem::new_with_prefix(&root)?;
                Ok(Store {
                    inner: Arc::new(fs),
                    prefix: String::new(),
                    local_root: Some(root),
                    url: url.to_string(),
                    stats,
                })
            }
            "s3" => {
                let bucket = parsed.host_str().ok_or_else(|| anyhow!("missing bucket"))?;
                let s3 = AmazonS3Builder::from_env()
                    .with_bucket_name(bucket)
                    .with_conditional_put(S3ConditionalPut::ETagMatch)
                    .build()?;
                Ok(Store { inner: Arc::new(s3), prefix, local_root: None, url: url.to_string(), stats })
            }
            "gs" => {
                let bucket = parsed.host_str().ok_or_else(|| anyhow!("missing bucket"))?;
                let mut b = GoogleCloudStorageBuilder::from_env().with_bucket_name(bucket);
                if let Ok(base) = std::env::var("PAIL_GCS_BASE_URL") {
                    b = b.with_base_url(&base).with_bearer_token("emulator");
                } else if let Ok(token) = std::env::var("PAIL_GCS_TOKEN") {
                    // e.g. `PAIL_GCS_TOKEN=$(gcloud auth print-access-token)`
                    b = b.with_bearer_token(token);
                }
                Ok(Store {
                    inner: Arc::new(b.build()?),
                    prefix,
                    local_root: None,
                    url: url.to_string(),
                    stats,
                })
            }
            s => bail!("unsupported store scheme {s}:// (use file://, s3:// or gs://)"),
        }
    }

    fn path(&self, key: &str) -> Path {
        if self.prefix.is_empty() { Path::from(key) } else { Path::from(format!("{}/{key}", self.prefix)) }
    }

    pub async fn get(&self, key: &str) -> Result<Bytes> {
        self.get_opt(key).await?.map(|(b, _)| b).ok_or_else(|| anyhow!("object not found: {key}"))
    }

    pub async fn get_owned(&self, key: String) -> Result<Bytes> {
        self.get(&key).await
    }

    /// Read an object with its version token, or `None` if it does not exist.
    pub async fn get_opt(&self, key: &str) -> Result<Option<(Bytes, Version)>> {
        self.stats.gets.fetch_add(1, Ordering::Relaxed);
        if let Some(root) = &self.local_root {
            let p = root.join(key);
            return match std::fs::read(&p) {
                Ok(b) => {
                    self.stats.bytes_down.fetch_add(b.len() as u64, Ordering::Relaxed);
                    let v = local_version(&b);
                    Ok(Some((Bytes::from(b), v)))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            };
        }
        let path = self.path(key);
        let got = retry(|| async {
            match self.inner.get_opts(&path, GetOptions::default()).await {
                Ok(r) => {
                    let v = Version { etag: r.meta.e_tag.clone(), version: r.meta.version.clone() };
                    Ok(Some((r.bytes().await?, v)))
                }
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(e),
            }
        })
        .await
        .with_context(|| format!("get {key}"))?;
        if let Some((b, _)) = &got {
            self.stats.bytes_down.fetch_add(b.len() as u64, Ordering::Relaxed);
        }
        Ok(got)
    }

    /// Fetch several byte ranges of one object; adjacent ranges are coalesced into one request.
    pub async fn get_ranges(&self, key: &str, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        self.stats.gets.fetch_add(1, Ordering::Relaxed);
        let path = self.path(key);
        let out = retry(|| self.inner.get_ranges(&path, ranges)).await.with_context(|| format!("range read {key}"))?;
        let n: usize = out.iter().map(|b| b.len()).sum();
        self.stats.bytes_down.fetch_add(n as u64, Ordering::Relaxed);
        Ok(out)
    }

    /// Write an immutable object. Returns false if it already existed (which is fine:
    /// content addressing means the existing bytes are identical).
    pub async fn put_new(&self, key: &str, data: Bytes) -> Result<bool> {
        self.stats.puts.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_up.fetch_add(data.len() as u64, Ordering::Relaxed);
        let path = self.path(key);
        // Safe to retry: content-addressed, so a retried create that finds the object
        // already written (by our earlier attempt) is a success.
        retry(|| async {
            let opts = PutOptions { mode: PutMode::Create, ..Default::default() };
            match self.inner.put_opts(&path, PutPayload::from(data.clone()), opts).await {
                Ok(_) => Ok(true),
                Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
                // Some S3 implementations answer a lost If-None-Match race with 412.
                Err(object_store::Error::Precondition { .. }) => Ok(false),
                Err(e) => Err(e),
            }
        })
        .await
        .with_context(|| format!("put {key}"))
    }

    /// Unconditional write (used for best-effort side records like intents and reflogs).
    pub async fn put(&self, key: &str, data: Bytes) -> Result<()> {
        self.stats.puts.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_up.fetch_add(data.len() as u64, Ordering::Relaxed);
        let path = self.path(key);
        retry(|| self.inner.put(&path, PutPayload::from(data.clone()))).await?;
        Ok(())
    }

    /// Compare-and-swap. `expected = None` means "create only if absent".
    pub async fn cas(&self, key: &str, data: Bytes, expected: Option<&Version>) -> Result<Cas> {
        self.stats.puts.fetch_add(1, Ordering::Relaxed);
        self.stats.cas.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_up.fetch_add(data.len() as u64, Ordering::Relaxed);
        if let Some(root) = &self.local_root {
            return local_cas(root, key, &data, expected);
        }
        let mode = match expected {
            None => PutMode::Create,
            Some(v) => PutMode::Update(UpdateVersion { e_tag: v.etag.clone(), version: v.version.clone() }),
        };
        let opts = PutOptions { mode, ..Default::default() };
        match self.inner.put_opts(&self.path(key), PutPayload::from(data), opts).await {
            Ok(r) => Ok(Cas::Ok(Version { etag: r.e_tag, version: r.version })),
            Err(object_store::Error::AlreadyExists { .. }) | Err(object_store::Error::Precondition { .. }) => {
                Ok(Cas::Conflict)
            }
            Err(e) => Err(e).with_context(|| format!("cas {key}")),
        }
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        if let Some(root) = &self.local_root {
            let _ = std::fs::remove_file(root.join(key));
            return Ok(());
        }
        match self.inner.delete(&self.path(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// List keys under a prefix, relative to the store root, with sizes.
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        Ok(self.list_meta(prefix).await?.into_iter().map(|o| (o.key, o.size)).collect())
    }

    /// List keys under a prefix with size and last-modified time (unix seconds).
    /// An object's size, or None if it doesn't exist.
    pub async fn size(&self, key: &str) -> Result<Option<u64>> {
        let p = self.path(key);
        match retry(|| self.inner.head(&p)).await {
            Ok(m) => Ok(Some(m.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn list_meta(&self, prefix: &str) -> Result<Vec<ObjInfo>> {
        let p = self.path(prefix.trim_end_matches('/'));
        let strip = if self.prefix.is_empty() { 0 } else { self.prefix.len() + 1 };
        let items: Vec<_> = match retry(|| self.inner.list(Some(&p)).try_collect()).await {
            Ok(v) => v,
            Err(object_store::Error::NotFound { .. }) => vec![],
            Err(e) => return Err(e.into()),
        };
        let mut out: Vec<ObjInfo> = items
            .into_iter()
            .map(|m| ObjInfo {
                key: m.location.as_ref()[strip..].to_string(),
                size: m.size,
                modified: m.last_modified.timestamp(),
            })
            .filter(|o| !o.key.starts_with(".lock") && !o.key.contains(".tmp"))
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }
}

fn local_version(b: &[u8]) -> Version {
    Version { etag: Some(blake3::hash(b).to_hex()[..16].to_string()), version: None }
}

fn local_cas(root: &FsPath, key: &str, data: &[u8], expected: Option<&Version>) -> Result<Cas> {
    use fs2::FileExt;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(root.join(".lock"))?;
    lock.lock_exclusive()?;
    let path = root.join(key);
    let current = match std::fs::read(&path) {
        Ok(b) => Some(local_version(&b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let ok = match (expected, &current) {
        (None, None) => true,
        (Some(e), Some(c)) => e.etag == c.etag,
        _ => false,
    };
    if !ok {
        return Ok(Cas::Conflict);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, &path)?;
    Ok(Cas::Ok(local_version(data)))
}

/// Retry transient failures of idempotent operations (connection resets, 5xx, throttling).
/// Definitive answers (not found, already exists, precondition failed) are returned at once.
async fn retry<T, F, Fut>(mut op: F) -> object_store::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = object_store::Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e @ object_store::Error::NotFound { .. })
            | Err(e @ object_store::Error::AlreadyExists { .. })
            | Err(e @ object_store::Error::Precondition { .. })
            | Err(e @ object_store::Error::NotModified { .. })
            | Err(e @ object_store::Error::NotImplemented { .. }) => return Err(e),
            Err(e) if attempt >= 6 => return Err(e),
            Err(_) => {
                attempt += 1;
                let ms = (100u64 << attempt).min(5_000);
                tokio::time::sleep(std::time::Duration::from_millis(ms / 2 + rand::random::<u64>() % (ms / 2))).await;
            }
        }
    }
}
