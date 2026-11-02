//! More bucket-notification sources for `pail events`: queues fed by the store's service.
//!
//! ```text
//! S3     bucket ──▶ SQS (S3 event notification, or EventBridge)        --sqs-queue URL
//! Azure  container ──▶ Event Grid ──▶ Storage Queue                    --azure-queue URL
//! R2     bucket ──▶ event notification ──▶ Queue (HTTP pull consumer)  --r2-queue ACCOUNT/QUEUE
//! ```
//!
//! Each is a pull consumer: receive a batch, turn the messages into [`Hint`]s, and delete
//! (acknowledge) the batch only when the watcher says so ([`HintSource::ack`], which it
//! calls after the scan's events are out and the cursor is saved). A delete that is lost,
//! or a crash before it, only makes the queue deliver the batch again: a repeated hint
//! costs a read.
//!
//! As with GCS, a message is only a nudge: nothing in it is trusted, the watcher re-reads
//! the key it names. So a source needs to produce just "something under this prefix
//! changed". A message that does not parse is still such a nudge ([`Hint::Scan`], a full
//! re-list, as the safety poll does), and is logged: it is never dropped silently. A message
//! that parses but names another bucket or a key outside the repository is acknowledged
//! and ignored.
//!
//! This is the whole model (`verify/events/events.qnt`) needs: a notification names a key
//! (`readKey`), the re-list is the safety poll's step, and the acknowledgement follows the
//! save (`finish`).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::creds::{self, AwsKeys};
use crate::events::{Hint, HintSource, Spec, area_url, split_area};

// ---------- which source ----------

/// A notification source named by text: `gcs:SUBSCRIPTION`, `sqs:URL`, `azure:URL`,
/// `r2:ACCOUNT/QUEUE` or `poll`. The same text is `PAIL_EVENTS_SOURCE` and the
/// `events.source` config value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    Poll,
    Gcs(String),
    Sqs(String),
    Azure(String),
    R2(String),
}

impl SourceSpec {
    pub fn parse(text: &str) -> Result<SourceSpec> {
        let text = text.trim();
        if text == "poll" {
            return Ok(SourceSpec::Poll);
        }
        let (kind, value) = text
            .split_once(':')
            .filter(|(_, v)| !v.is_empty())
            .with_context(|| {
                format!(
                    "an events source is `poll`, `gcs:SUBSCRIPTION`, `sqs:URL`, `azure:URL` or `r2:ACCOUNT/QUEUE` (got `{text}`)"
                )
            })?;
        let v = value.to_string();
        Ok(match kind {
            "gcs" => SourceSpec::Gcs(v),
            "sqs" => SourceSpec::Sqs(v),
            "azure" => SourceSpec::Azure(v),
            "r2" => {
                if v.split_once('/')
                    .is_none_or(|(a, q)| a.is_empty() || q.is_empty())
                {
                    bail!("an R2 queue is `r2:ACCOUNT/QUEUE` (got `{text}`)");
                }
                SourceSpec::R2(v)
            }
            _ => bail!("unknown events source `{kind}` (gcs, sqs, azure, r2 or poll)"),
        })
    }
}

impl std::fmt::Display for SourceSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceSpec::Poll => write!(f, "poll"),
            SourceSpec::Gcs(v) => write!(f, "gcs:{v}"),
            SourceSpec::Sqs(v) => write!(f, "sqs:{v}"),
            SourceSpec::Azure(v) => write!(f, "azure:{v}"),
            SourceSpec::R2(v) => write!(f, "r2:{v}"),
        }
    }
}

/// The source to use and where it came from, in order: the command-line flag
/// (`flag`), `PAIL_EVENTS_SOURCE`, the most specific `events.<url>.source` of the config
/// whose URL prefixes the store (or `events.source`), else `poll`.
pub fn choose(
    flag: Option<SourceSpec>,
    env: Option<&str>,
    cfg: &crate::config::Config,
    store: &str,
) -> Result<(SourceSpec, String)> {
    if let Some(s) = flag {
        return Ok((s, "flag".into()));
    }
    if let Some(v) = env.filter(|v| !v.trim().is_empty()) {
        return Ok((
            SourceSpec::parse(v).context("PAIL_EVENTS_SOURCE")?,
            "PAIL_EVENTS_SOURCE".into(),
        ));
    }
    let mut best: Option<(usize, String)> = None;
    for (_, key, _) in cfg.list() {
        let Some(rest) = key.strip_prefix("events.") else {
            continue;
        };
        let Some((url, name)) = rest.rsplit_once('.') else {
            continue;
        };
        if name != "source" {
            continue;
        }
        let u = url.trim_end_matches('/');
        if (store == u || store.starts_with(&format!("{u}/")))
            && best.as_ref().is_none_or(|(n, _)| u.len() > *n)
        {
            best = Some((u.len(), url.to_string()));
        }
    }
    let keys = best
        .map(|(_, u)| format!("events.{u}.source"))
        .into_iter()
        .chain(["events.source".to_string()]);
    for key in keys {
        if let Some(v) = cfg.get(&key).filter(|v| !v.is_empty()) {
            return Ok((SourceSpec::parse(&v).with_context(|| key.clone())?, key));
        }
    }
    Ok((SourceSpec::Poll, "default".into()))
}

// ---------- which keys a message is about ----------

/// A store's bucket (or container) and the object-name prefix of its keys.
#[derive(Debug, PartialEq, Eq)]
pub struct StoreName {
    pub bucket: Option<String>,
    /// `""` or `repo/`.
    pub prefix: String,
}

/// `s3://b/team/app` and `gs://b/team/app` → bucket `b`, prefix `team/app/`; an Azure blob
/// URL `https://acct.blob.core.windows.net/c/team/app` → container `c`, the same prefix.
pub fn store_name(url: &str) -> Result<StoreName> {
    let (scheme, rest) = url.split_once("://").context("a store URL")?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (bucket, path) = match scheme {
        "s3" | "gs" => (Some(host.to_string()), path),
        "https" if host.ends_with(".blob.core.windows.net") => {
            let (c, p) = path.split_once('/').unwrap_or((path, ""));
            (Some(c.to_string()), p)
        }
        _ => (None, path),
    };
    let path = path.trim_matches('/');
    Ok(StoreName {
        bucket,
        prefix: if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        },
    })
}

/// What a message must name to matter: the repository's refs, and the watched area.
#[derive(Debug, Clone)]
pub struct Names {
    /// Messages naming another bucket are ignored (None: not checked).
    pub bucket: Option<String>,
    /// `repo/refs/`.
    pub refs: String,
    /// `repo.actions/` (the whole area; `--prefix` narrows inside it).
    pub area: Option<String>,
}

impl Names {
    /// `check_bucket` is false when the queue's service names buckets differently from the
    /// store's URL (Azure containers behind an `s3://` store).
    pub fn new(store_url: &str, spec: &Spec, check_bucket: bool) -> Result<Names> {
        let me = store_name(store_url)?;
        let area = match &spec.prefix {
            Some(p) => Some(store_name(&area_url(store_url, split_area(p)?.0)?)?.prefix),
            None => None,
        };
        Ok(Names {
            bucket: me.bucket.filter(|_| check_bucket),
            refs: format!("{}refs/", me.prefix),
            area,
        })
    }

    /// The hint for `object` of `bucket`, if it is one of ours.
    pub fn hint(&self, bucket: Option<&str>, object: &str) -> Option<Hint> {
        if let (Some(mine), Some(theirs)) = (self.bucket.as_deref(), bucket) {
            if mine != theirs {
                return None;
            }
        }
        if let Some(branch) = object.strip_prefix(&self.refs) {
            return Some(Hint::Ref(branch.to_string()));
        }
        self.area
            .as_deref()
            .and_then(|a| object.strip_prefix(a))
            .map(|k| Hint::Object(k.to_string()))
    }
}

// ---------- message parsers ----------

/// What a message says.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// The keys it names that matter here (possibly none: acknowledge and move on).
    Hints(Vec<Hint>),
    /// Not a change (S3's test event): nothing to do.
    Ignore,
    /// It does not parse: scan, and say why.
    Unparseable(String),
}

fn bad(why: impl Into<String>) -> Parsed {
    Parsed::Unparseable(why.into())
}

/// `%XX` and `+` (a space) of an S3 notification's key.
pub fn s3_key_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let h = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// An SQS message body fed by S3: an event notification (`{"Records":[…]}`, keys URL-encoded),
/// an EventBridge event (`{"detail-type":"Object Created","detail":{…}}`, keys as they are),
/// or either of them wrapped by SNS (`{"Type":"Notification","Message":"…"}`).
pub fn parse_s3(body: &str, names: &Names) -> Parsed {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return bad(format!("not JSON: {e}")),
    };
    parse_s3_value(&v, names, 0)
}

fn parse_s3_value(v: &serde_json::Value, names: &Names, depth: u32) -> Parsed {
    if v["Type"] == "Notification" {
        return match v["Message"].as_str() {
            Some(m) if depth < 2 => match serde_json::from_str(m) {
                Ok(inner) => parse_s3_value(&inner, names, depth + 1),
                Err(e) => bad(format!("the SNS message is not JSON: {e}")),
            },
            _ => bad("an SNS notification without a usable Message"),
        };
    }
    if v["Event"] == "s3:TestEvent" {
        return Parsed::Ignore;
    }
    if let Some(records) = v["Records"].as_array() {
        let mut hints = vec![];
        for r in records {
            let (Some(bucket), Some(key)) = (
                r["s3"]["bucket"]["name"].as_str(),
                r["s3"]["object"]["key"].as_str(),
            ) else {
                return bad("a record without s3.bucket.name and s3.object.key");
            };
            let Some(key) = s3_key_decode(key) else {
                return bad(format!("an undecodable object key `{key}`"));
            };
            hints.extend(names.hint(Some(bucket), &key));
        }
        return Parsed::Hints(hints);
    }
    if v["detail-type"].is_string() && v["detail"].is_object() {
        let d = &v["detail"];
        let (Some(bucket), Some(key)) = (d["bucket"]["name"].as_str(), d["object"]["key"].as_str())
        else {
            return bad("an EventBridge event without detail.bucket.name and detail.object.key");
        };
        return Parsed::Hints(names.hint(Some(bucket), key).into_iter().collect());
    }
    bad("neither an S3 notification nor an EventBridge event")
}

/// An Azure Storage Queue message fed by Event Grid: one event (or an array of them) of
/// the Event Grid schema (`eventType`) or CloudEvents (`type`), as JSON or base64 of it.
/// `Microsoft.Storage.BlobCreated` and `BlobDeleted` name a blob in `subject`
/// (`/blobServices/default/containers/C/blobs/NAME`); a rename names no old key, so it scans.
pub fn parse_event_grid(text: &str, names: &Names) -> Parsed {
    let text = text.trim();
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => match base64_decode(text).and_then(|b| serde_json::from_slice(&b).ok()) {
            Some(v) => v,
            None => return bad("neither JSON nor base64 of JSON"),
        },
    };
    let events: Vec<&serde_json::Value> = match &v {
        serde_json::Value::Array(a) => a.iter().collect(),
        o => vec![o],
    };
    let mut hints = vec![];
    for e in events {
        let Some(ty) = e["eventType"].as_str().or_else(|| e["type"].as_str()) else {
            return bad("an event without eventType or type");
        };
        match ty {
            "Microsoft.Storage.BlobCreated" | "Microsoft.Storage.BlobDeleted" => {
                let subject = e["subject"].as_str().unwrap_or_default();
                let Some((container, blob)) = subject
                    .strip_prefix("/blobServices/default/containers/")
                    .and_then(|r| r.split_once("/blobs/"))
                else {
                    return bad(format!("a blob event with the subject `{subject}`"));
                };
                hints.extend(names.hint(Some(container), blob));
            }
            "Microsoft.Storage.BlobRenamed" | "Microsoft.Storage.DirectoryRenamed" => {
                hints.push(Hint::Scan)
            }
            t if t.starts_with("Microsoft.Storage.") => {} // tiers, policies: not a change
            t => return bad(format!("an event of type `{t}`")),
        }
    }
    Parsed::Hints(hints)
}

/// A Cloudflare Queues message fed by R2 event notifications: `{"account":…,"action":
/// "PutObject","bucket":"b","object":{"key":"k",…},"eventTime":…}`.
pub fn parse_r2(body: &str, names: &Names) -> Parsed {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return bad(format!("not JSON: {e}")),
    };
    let (Some(action), Some(bucket), Some(key)) = (
        v["action"].as_str(),
        v["bucket"].as_str(),
        v["object"]["key"].as_str(),
    ) else {
        return bad("no action, bucket and object.key");
    };
    match action {
        "PutObject"
        | "CopyObject"
        | "CompleteMultipartUpload"
        | "DeleteObject"
        | "LifecycleDeletion" => Parsed::Hints(names.hint(Some(bucket), key).into_iter().collect()),
        a => bad(format!("an R2 action `{a}`")),
    }
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = vec![];
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// ---------- the generic queue consumer ----------

/// One message of a queue: `handle` is what deleting it takes, `body` what it says.
#[derive(Debug, Clone)]
pub struct Message {
    pub handle: String,
    pub body: String,
}

/// A service's queue.
#[async_trait]
pub trait Queue: Send {
    /// Wait for the next batch (long-poll, or poll and sleep): empty means "nothing yet".
    /// Must be safe to drop while waiting.
    async fn receive(&mut self) -> Result<Vec<Message>>;
    /// Delete the batch.
    async fn delete(&mut self, handles: &[String]) -> Result<()>;
}

/// A [`HintSource`] over a [`Queue`] and a message parser.
pub struct QueueSource {
    label: &'static str,
    queue: Box<dyn Queue>,
    names: Names,
    parse: fn(&str, &Names) -> Parsed,
    pending: Vec<String>,
}

impl QueueSource {
    pub fn new(
        label: &'static str,
        queue: Box<dyn Queue>,
        names: Names,
        parse: fn(&str, &Names) -> Parsed,
    ) -> QueueSource {
        QueueSource {
            label,
            queue,
            names,
            parse,
            pending: vec![],
        }
    }
}

#[async_trait]
impl HintSource for QueueSource {
    fn name(&self) -> &'static str {
        self.label
    }

    async fn pull(&mut self) -> Result<Vec<Hint>> {
        loop {
            let batch = self.queue.receive().await?;
            if batch.is_empty() {
                continue;
            }
            let mut hints = vec![];
            for m in &batch {
                match (self.parse)(&m.body, &self.names) {
                    Parsed::Hints(h) => hints.extend(h),
                    Parsed::Ignore => {}
                    Parsed::Unparseable(why) => {
                        eprintln!(
                            "pail events: {}: unparseable message ({why}); scanning instead",
                            self.label
                        );
                        hints.push(Hint::Scan);
                    }
                }
            }
            self.pending = batch.into_iter().map(|m| m.handle).collect();
            return Ok(hints);
        }
    }

    async fn ack(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.queue.delete(&self.pending).await?;
        self.pending.clear();
        Ok(())
    }
}

/// The source for `spec` over the store (`creds`: the store's credential provider, if it
/// has one). `GCS` is not here: it keeps its own implementation in `events.rs`.
pub fn open(
    kind: &SourceSpec,
    store_url: &str,
    spec: &Spec,
    creds: Option<Arc<creds::Source>>,
) -> Result<Box<dyn HintSource>> {
    let idle = Duration::from_secs(2);
    Ok(match kind {
        SourceSpec::Sqs(url) => Box::new(QueueSource::new(
            "sqs",
            Box::new(SqsQueue::new(url, AwsAuth::Chain { creds }, 20)?),
            Names::new(store_url, spec, true)?,
            parse_s3,
        )),
        SourceSpec::Azure(url) => Box::new(QueueSource::new(
            "azure",
            Box::new(AzureQueue::new(url, Bearer::from_creds(creds), idle)?),
            // the container is checked only when the store is itself an Azure blob URL
            Names::new(
                store_url,
                spec,
                store_url.contains(".blob.core.windows.net"),
            )?,
            parse_event_grid,
        )),
        SourceSpec::R2(q) => {
            let (account, queue) = q.split_once('/').context("ACCOUNT/QUEUE")?;
            let bearer = match std::env::var("CLOUDFLARE_API_TOKEN") {
                Ok(t) if !t.is_empty() => Bearer::Fixed(t),
                _ => Bearer::from_creds(creds),
            };
            Box::new(QueueSource::new(
                "r2",
                Box::new(R2Queue::new(R2_API, account, queue, bearer, idle)),
                Names::new(store_url, spec, true)?,
                parse_r2,
            ))
        }
        SourceSpec::Poll | SourceSpec::Gcs(_) => bail!("not a queue source: {kind}"),
    })
}

// ---------- credentials ----------

/// A bearer token: fixed, from pail's credential provider, or absent.
#[derive(Clone)]
pub enum Bearer {
    None,
    Fixed(String),
    Creds(Arc<creds::Source>),
}

impl Bearer {
    pub fn from_creds(c: Option<Arc<creds::Source>>) -> Bearer {
        c.map(Bearer::Creds).unwrap_or(Bearer::None)
    }

    async fn token(&self, what: &str) -> Result<String> {
        match self {
            Bearer::Fixed(t) => Ok(t.clone()),
            Bearer::Creds(c) => {
                let c = c.clone();
                tokio::task::spawn_blocking(move || c.get())
                    .await??
                    .token
                    .context("the credential has no token")
            }
            Bearer::None => bail!("{what}"),
        }
    }
}

/// AWS keys for signing.
pub enum AwsAuth {
    Fixed(AwsKeys),
    /// The store's credential provider if it has one; else `AWS_ACCESS_KEY_ID` and
    /// `AWS_SECRET_ACCESS_KEY` (`AWS_SESSION_TOKEN`) as the S3 store reads them; else the
    /// `aws` CLI's resolution (`aws configure export-credentials`: profiles, SSO, roles).
    Chain {
        creds: Option<Arc<creds::Source>>,
    },
}

impl AwsAuth {
    async fn keys(&self) -> Result<AwsKeys> {
        match self {
            AwsAuth::Fixed(k) => Ok(k.clone()),
            AwsAuth::Chain { creds } => {
                if let Some(c) = creds {
                    let c = c.clone();
                    let got = tokio::task::spawn_blocking(move || c.get()).await??;
                    return got
                        .aws
                        .context("the credential provider returned no aws keys for SQS");
                }
                if let (Ok(id), Ok(secret)) = (
                    std::env::var("AWS_ACCESS_KEY_ID"),
                    std::env::var("AWS_SECRET_ACCESS_KEY"),
                ) {
                    return Ok(AwsKeys {
                        access_key_id: id,
                        secret_access_key: secret,
                        session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
                    });
                }
                tokio::task::spawn_blocking(aws_cli_keys)
                    .await?
                    .context("no AWS credentials for SQS: set AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, `pail auth use` a provider, or sign in with the aws CLI")
            }
        }
    }
}

fn aws_cli_keys() -> Result<AwsKeys> {
    let out = std::process::Command::new("aws")
        .args(["configure", "export-credentials", "--format", "process"])
        .output()
        .context("running `aws configure export-credentials`")?;
    if !out.status.success() {
        bail!("`aws configure export-credentials` failed ({})", out.status);
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    Ok(AwsKeys {
        access_key_id: v["AccessKeyId"].as_str().context("AccessKeyId")?.into(),
        secret_access_key: v["SecretAccessKey"]
            .as_str()
            .context("SecretAccessKey")?
            .into(),
        session_token: v["SessionToken"].as_str().map(String::from),
    })
}

// ---------- AWS Signature Version 4 ----------

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// Sign a request. `headers` are the request's headers, names lowercase, with `host`;
/// `x-amz-date` and the session token are added here. Returns `(x-amz-date, authorization)`.
#[allow(clippy::too_many_arguments)]
pub fn sigv4(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(&str, &str)],
    payload: &[u8],
    region: &str,
    service: &str,
    keys: &AwsKeys,
    now: DateTime<Utc>,
) -> (String, String) {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let day = &amz_date[..8];
    let mut all: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        .collect();
    all.push(("x-amz-date".into(), amz_date.clone()));
    if let Some(t) = &keys.session_token {
        all.push(("x-amz-security-token".into(), t.clone()));
    }
    all.sort();
    let canonical_headers: String = all.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = all
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let mut q: Vec<&str> = query.split('&').filter(|p| !p.is_empty()).collect();
    q.sort();
    let canonical = format!(
        "{method}\n{path}\n{}\n{canonical_headers}\n{signed}\n{}",
        q.join("&"),
        hex::encode(Sha256::digest(payload))
    );
    let scope = format!("{day}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical.as_bytes()))
    );
    let mut k = hmac_sha256(
        format!("AWS4{}", keys.secret_access_key).as_bytes(),
        day.as_bytes(),
    )
    .to_vec();
    for part in [region, service, "aws4_request"] {
        k = hmac_sha256(&k, part.as_bytes()).to_vec();
    }
    let sig = hex::encode(hmac_sha256(&k, to_sign.as_bytes()));
    (
        amz_date,
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}",
            keys.access_key_id
        ),
    )
}

// ---------- SQS ----------

/// An SQS queue, over the JSON protocol (`X-Amz-Target: AmazonSQS.ReceiveMessage`).
pub struct SqsQueue {
    client: reqwest::Client,
    url: String,
    /// `scheme://host[:port]`: where requests go.
    origin: String,
    host: String,
    region: String,
    auth: AwsAuth,
    wait: u64,
}

fn sqs_region(host: &str) -> String {
    // sqs.us-east-1.amazonaws.com, sqs.cn-north-1.amazonaws.com.cn, us-east-1.queue.amazonaws.com
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() > 2 && parts[0] == "sqs" {
        return parts[1].to_string();
    }
    if parts.len() > 2 && parts[1] == "queue" {
        return parts[0].to_string();
    }
    std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|_| "us-east-1".into())
}

impl SqsQueue {
    /// `wait`: seconds each receive waits for a message (long polling, at most 20).
    pub fn new(queue_url: &str, auth: AwsAuth, wait: u64) -> Result<SqsQueue> {
        let u = url::Url::parse(queue_url).context("--sqs-queue is the queue's URL")?;
        let host = u.host_str().context("the queue URL has no host")?;
        let host = match u.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_string(),
        };
        Ok(SqsQueue {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(wait + 20))
                .build()?,
            url: queue_url.to_string(),
            origin: format!("{}://{host}", u.scheme()),
            region: sqs_region(u.host_str().unwrap_or_default()),
            host,
            auth,
            wait,
        })
    }

    async fn call(&self, target: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let payload = body.to_string();
        let keys = self.auth.keys().await?;
        let ctype = "application/x-amz-json-1.0";
        let target = format!("AmazonSQS.{target}");
        let (date, auth) = sigv4(
            "POST",
            "/",
            "",
            &[
                ("content-type", ctype),
                ("host", &self.host),
                ("x-amz-target", &target),
            ],
            payload.as_bytes(),
            &self.region,
            "sqs",
            &keys,
            Utc::now(),
        );
        let mut req = self
            .client
            .post(format!("{}/", self.origin))
            .header("content-type", ctype)
            .header("x-amz-target", &target)
            .header("x-amz-date", date)
            .header("authorization", auth);
        if let Some(t) = &keys.session_token {
            req = req.header("x-amz-security-token", t);
        }
        let resp = req
            .body(payload)
            .send()
            .await
            .with_context(|| format!("calling SQS {target}"))?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("SQS {target} failed ({status}): {text}");
        }
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }
}

#[async_trait]
impl Queue for SqsQueue {
    async fn receive(&mut self) -> Result<Vec<Message>> {
        let r = self
            .call(
                "ReceiveMessage",
                serde_json::json!({
                    "QueueUrl": self.url,
                    "MaxNumberOfMessages": 10,
                    "WaitTimeSeconds": self.wait,
                    "VisibilityTimeout": 120,
                }),
            )
            .await?;
        Ok(r["Messages"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| Message {
                handle: m["ReceiptHandle"].as_str().unwrap_or_default().into(),
                body: m["Body"].as_str().unwrap_or_default().into(),
            })
            .collect())
    }

    async fn delete(&mut self, handles: &[String]) -> Result<()> {
        for chunk in handles.chunks(10) {
            let entries: Vec<_> = chunk
                .iter()
                .enumerate()
                .map(|(i, h)| serde_json::json!({"Id": i.to_string(), "ReceiptHandle": h}))
                .collect();
            let r = self
                .call(
                    "DeleteMessageBatch",
                    serde_json::json!({"QueueUrl": self.url, "Entries": entries}),
                )
                .await?;
            if let Some(f) = r["Failed"].as_array().filter(|f| !f.is_empty()) {
                // the queue will deliver them again: a repeated hint costs a read
                eprintln!("pail events: sqs: {} message(s) were not deleted", f.len());
            }
        }
        Ok(())
    }
}

// ---------- Azure Storage Queue ----------

/// An Azure Storage Queue (`https://ACCOUNT.queue.core.windows.net/QUEUE`), authorized by
/// a SAS token in the URL's query, else by a bearer token (Microsoft Entra).
pub struct AzureQueue {
    client: reqwest::Client,
    /// The queue's URL without its query.
    base: String,
    /// The SAS query, if the URL had one.
    sas: Option<String>,
    bearer: Bearer,
    idle: Duration,
}

impl AzureQueue {
    pub fn new(queue_url: &str, bearer: Bearer, idle: Duration) -> Result<AzureQueue> {
        let mut u = url::Url::parse(queue_url).context("--azure-queue is the queue's URL")?;
        let sas = u.query().map(String::from).filter(|q| q.contains("sig="));
        u.set_query(None);
        Ok(AzureQueue {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()?,
            base: u.as_str().trim_end_matches('/').to_string(),
            sas,
            bearer,
            idle,
        })
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &str,
    ) -> Result<reqwest::Response> {
        let q = [query, self.sas.as_deref().unwrap_or("")]
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("&");
        let mut req = self
            .client
            .request(method, format!("{}{path}?{q}", self.base))
            .header("x-ms-version", "2021-08-06")
            .header(
                "x-ms-date",
                Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
            );
        if self.sas.is_none() {
            let t = self
                .bearer
                .token("the Azure queue needs a SAS token in --azure-queue's URL, or a credential provider that returns a token for https://storage.azure.com/")
                .await?;
            req = req.bearer_auth(t);
        }
        Ok(req.send().await.context("calling the Azure queue")?)
    }
}

/// The inner text of every `<tag>…</tag>` of `xml`.
fn xml_all<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = vec![];
    let mut rest = xml;
    while let Some(a) = rest.find(&open) {
        let after = &rest[a + open.len()..];
        let Some(b) = after.find(&close) else { break };
        out.push(&after[..b]);
        rest = &after[b + close.len()..];
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The messages of a "Get Messages" response, as `(id, pop receipt, text)`.
pub fn parse_azure_messages(xml: &str) -> Vec<(String, String, String)> {
    xml_all(xml, "QueueMessage")
        .into_iter()
        .map(|m| {
            let f = |t| {
                xml_all(m, t)
                    .first()
                    .map(|s| xml_unescape(s))
                    .unwrap_or_default()
            };
            (f("MessageId"), f("PopReceipt"), f("MessageText"))
        })
        .collect()
}

#[async_trait]
impl Queue for AzureQueue {
    async fn receive(&mut self) -> Result<Vec<Message>> {
        let resp = self
            .request(
                reqwest::Method::GET,
                "/messages",
                "numofmessages=32&visibilitytimeout=120&timeout=30",
            )
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("Azure queue get messages failed ({status}): {text}");
        }
        let msgs: Vec<Message> = parse_azure_messages(&text)
            .into_iter()
            .map(|(id, receipt, body)| Message {
                handle: format!("{id}\n{receipt}"),
                body,
            })
            .collect();
        if msgs.is_empty() {
            tokio::time::sleep(self.idle).await; // the Storage Queue API cannot long-poll
        }
        Ok(msgs)
    }

    async fn delete(&mut self, handles: &[String]) -> Result<()> {
        for h in handles {
            let (id, receipt) = h.split_once('\n').unwrap_or((h, ""));
            let q = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("popreceipt", receipt)
                .finish();
            let resp = self
                .request(reqwest::Method::DELETE, &format!("/messages/{id}"), &q)
                .await?;
            let s = resp.status();
            // 404: already deleted (or the pop receipt expired and it will come again)
            if !s.is_success() && s.as_u16() != 404 {
                bail!("Azure queue delete failed ({s})");
            }
        }
        Ok(())
    }
}

// ---------- Cloudflare Queues (HTTP pull consumer) ----------

pub const R2_API: &str = "https://api.cloudflare.com/client/v4";

/// A Cloudflare Queue with an HTTP pull consumer.
pub struct R2Queue {
    client: reqwest::Client,
    api: String,
    account: String,
    /// The queue's name, or its id.
    queue: String,
    id: Option<String>,
    bearer: Bearer,
    idle: Duration,
}

impl R2Queue {
    pub fn new(api: &str, account: &str, queue: &str, bearer: Bearer, idle: Duration) -> R2Queue {
        R2Queue {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("a client"),
            api: api.trim_end_matches('/').to_string(),
            account: account.to_string(),
            queue: queue.to_string(),
            id: None,
            bearer,
            idle,
        }
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let t = self
            .bearer
            .token("the R2 queue needs CLOUDFLARE_API_TOKEN (Queues read and write), or a credential provider that returns a token")
            .await?;
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.api))
            .bearer_auth(t);
        if let Some(b) = body {
            req = req
                .header("content-type", "application/json")
                .body(b.to_string());
        }
        let resp = req.send().await.context("calling the Cloudflare API")?;
        let status = resp.status();
        let v: serde_json::Value = serde_json::from_slice(&resp.bytes().await?).unwrap_or_default();
        if !status.is_success() || v["success"] == false {
            bail!("Cloudflare API {path} failed ({status}): {}", v["errors"]);
        }
        Ok(v)
    }

    /// The queue's id: `queue` as given if it is one, else looked up by name.
    async fn queue_id(&mut self) -> Result<String> {
        if let Some(id) = &self.id {
            return Ok(id.clone());
        }
        let looks_like_id =
            self.queue.len() == 32 && self.queue.bytes().all(|b| b.is_ascii_hexdigit());
        let id = if looks_like_id {
            self.queue.clone()
        } else {
            let v = self
                .call(
                    reqwest::Method::GET,
                    &format!("/accounts/{}/queues?per_page=100", self.account),
                    None,
                )
                .await?;
            v["result"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|q| q["queue_name"] == self.queue.as_str())
                .and_then(|q| q["queue_id"].as_str())
                .map(String::from)
                .with_context(|| {
                    format!("no queue named {} in account {}", self.queue, self.account)
                })?
        };
        self.id = Some(id.clone());
        Ok(id)
    }
}

#[async_trait]
impl Queue for R2Queue {
    async fn receive(&mut self) -> Result<Vec<Message>> {
        let id = self.queue_id().await?;
        let v = self
            .call(
                reqwest::Method::POST,
                &format!("/accounts/{}/queues/{id}/messages/pull", self.account),
                Some(serde_json::json!({"batch_size": 100, "visibility_timeout_ms": 120000})),
            )
            .await?;
        let msgs: Vec<Message> = v["result"]["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| Message {
                handle: m["lease_id"].as_str().unwrap_or_default().into(),
                // a JSON body arrives as an object or as a string of it
                body: match &m["body"] {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                },
            })
            .collect();
        if msgs.is_empty() {
            tokio::time::sleep(self.idle).await;
        }
        Ok(msgs)
    }

    async fn delete(&mut self, handles: &[String]) -> Result<()> {
        let id = self.queue_id().await?;
        let acks: Vec<_> = handles
            .iter()
            .map(|h| serde_json::json!({"lease_id": h}))
            .collect();
        self.call(
            reqwest::Method::POST,
            &format!("/accounts/{}/queues/{id}/messages/ack", self.account),
            Some(serde_json::json!({"acks": acks, "retries": []})),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Names {
        Names {
            bucket: Some("acme".into()),
            refs: "team/app/refs/".into(),
            area: Some("team/app.actions/".into()),
        }
    }

    fn hints(p: Parsed) -> Vec<Hint> {
        match p {
            Parsed::Hints(h) => h,
            other => panic!("not hints: {other:?}"),
        }
    }

    // The AWS documentation's Signature Version 4 example (iam ListUsers, 2015-08-30).
    #[test]
    fn sigv4_matches_the_aws_documentation_example() {
        let keys = AwsKeys {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let now = DateTime::parse_from_rfc3339("2015-08-30T12:36:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (date, auth) = sigv4(
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            &[
                (
                    "content-type",
                    "application/x-www-form-urlencoded; charset=utf-8",
                ),
                ("host", "iam.amazonaws.com"),
            ],
            b"",
            "us-east-1",
            "iam",
            &keys,
            now,
        );
        assert_eq!(date, "20150830T123600Z");
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    // RFC 4231, test case 2 (and 6, a key longer than the block).
    #[test]
    fn hmac_matches_rfc_4231() {
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex::encode(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn s3_event_notification_keys_are_url_encoded() {
        // docs.aws.amazon.com/AmazonS3/latest/userguide/notification-content-structure.html
        let body = r#"{"Records":[{"eventVersion":"2.1","eventSource":"aws:s3","awsRegion":"us-west-2",
          "eventTime":"1970-01-01T00:00:00.000Z","eventName":"ObjectCreated:Put",
          "s3":{"s3SchemaVersion":"1.0","configurationId":"testConfigRule",
            "bucket":{"name":"acme","ownerIdentity":{"principalId":"EXAMPLE"},"arn":"arn:aws:s3:::acme"},
            "object":{"key":"team/app/refs/pr%2F12","size":1024,"eTag":"abc","sequencer":"0A1B"}}},
          {"eventSource":"aws:s3","eventName":"ObjectRemoved:Delete",
          "s3":{"bucket":{"name":"acme"},"object":{"key":"team/app.actions/queue/a+b.json"}}},
          {"eventSource":"aws:s3","s3":{"bucket":{"name":"other"},"object":{"key":"team/app/refs/x"}}},
          {"eventSource":"aws:s3","s3":{"bucket":{"name":"acme"},"object":{"key":"team/app/commits/ab"}}}]}"#;
        assert_eq!(
            hints(parse_s3(body, &names())),
            [
                Hint::Ref("pr/12".into()),
                Hint::Object("queue/a b.json".into())
            ]
        );
    }

    #[test]
    fn s3_eventbridge_events_and_sns_wrappers() {
        // docs.aws.amazon.com/AmazonS3/latest/userguide/ev-events.html
        let created = r#"{"version":"0","id":"17793124-05d4-b198-2fde-7ededc63b103",
          "detail-type":"Object Created","source":"aws.s3","account":"111122223333",
          "time":"2021-11-12T00:00:00Z","region":"ca-central-1","resources":["arn:aws:s3:::acme"],
          "detail":{"version":"0","bucket":{"name":"acme"},
            "object":{"key":"team/app/refs/main","size":5,"etag":"b1946","sequencer":"617f"},
            "request-id":"N4N7GDK58NMKJ12R","requester":"123456789012","reason":"PutObject"}}"#;
        assert_eq!(
            hints(parse_s3(created, &names())),
            [Hint::Ref("main".into())]
        );
        let deleted = created
            .replace("Object Created", "Object Deleted")
            .replace("team/app/refs/main", "team/app/refs/pr/1 x");
        assert_eq!(
            hints(parse_s3(&deleted, &names())),
            [Hint::Ref("pr/1 x".into())],
            "EventBridge keys are not URL-encoded"
        );
        let sns = serde_json::json!({"Type":"Notification","MessageId":"1","TopicArn":"arn:aws:sns:us-east-1:1:t",
            "Message": created, "Timestamp":"2021-11-12T00:00:00Z"});
        assert_eq!(
            hints(parse_s3(&sns.to_string(), &names())),
            [Hint::Ref("main".into())]
        );
        let test = r#"{"Service":"Amazon S3","Event":"s3:TestEvent","Time":"2014-10-13T15:57:02.089Z","Bucket":"acme","RequestId":"5582815E1AEA5ADF","HostId":"8cLeGAmw098X5cv4Zkwcmo8vvZa3eH3eKxsPzbB9wrR+YstdA6Knx4Ip8EXAMPLE"}"#;
        assert_eq!(parse_s3(test, &names()), Parsed::Ignore);
    }

    #[test]
    fn s3_messages_that_do_not_parse_say_so() {
        for body in [
            "not json",
            r#"{"hello":"world"}"#,
            r#"{"Records":[{"s3":{}}]}"#,
            r#"{"Type":"Notification"}"#,
            r#"{"detail-type":"Object Created","detail":{}}"#,
        ] {
            assert!(
                matches!(parse_s3(body, &names()), Parsed::Unparseable(_)),
                "{body}"
            );
        }
    }

    #[test]
    fn event_grid_blob_events() {
        // learn.microsoft.com/azure/event-grid/event-schema-blob-storage
        let created = r#"{"topic":"/subscriptions/1/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct",
          "subject":"/blobServices/default/containers/acme/blobs/team/app/refs/pr/12",
          "eventType":"Microsoft.Storage.BlobCreated","id":"831e1650-001e-001b-66ab-eeb76e069631",
          "data":{"api":"PutBlockList","contentType":"text/plain","contentLength":524288,"blobType":"BlockBlob",
            "url":"https://acct.blob.core.windows.net/acme/team/app/refs/pr/12","sequencer":"00000000000004420000000000028963"},
          "dataVersion":"","metadataVersion":"1","eventTime":"2017-06-26T18:41:00.9584103Z"}"#;
        assert_eq!(
            hints(parse_event_grid(created, &names())),
            [Hint::Ref("pr/12".into())]
        );
        let deleted = created
            .replace("BlobCreated", "BlobDeleted")
            .replace("refs/pr/12", "refs/old");
        assert_eq!(
            hints(parse_event_grid(&deleted, &names())),
            [Hint::Ref("old".into())]
        );
        // a batch, an object of the area, another container, and the same base64-encoded
        let batch = format!(
            "[{}, {}, {}]",
            created.replace("refs/pr/12", "refs/a"),
            created.replace(
                "blobs/team/app/refs/pr/12",
                "blobs/team/app.actions/queue/q.json"
            ),
            created.replace("containers/acme", "containers/other")
        );
        let want = [Hint::Ref("a".into()), Hint::Object("queue/q.json".into())];
        assert_eq!(hints(parse_event_grid(&batch, &names())), want);
        let b64 = {
            const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut s = String::new();
            for c in batch.as_bytes().chunks(3) {
                let n = (c[0] as u32) << 16
                    | (*c.get(1).unwrap_or(&0) as u32) << 8
                    | *c.get(2).unwrap_or(&0) as u32;
                for i in 0..4 {
                    if i <= c.len() {
                        s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
                    } else {
                        s.push('=');
                    }
                }
            }
            s
        };
        assert_eq!(hints(parse_event_grid(&b64, &names())), want);
        // CloudEvents schema
        let ce = r#"{"specversion":"1.0","type":"Microsoft.Storage.BlobCreated","source":"/subscriptions/1/x",
          "id":"1","subject":"/blobServices/default/containers/acme/blobs/team/app/refs/main","data":{}}"#;
        assert_eq!(
            hints(parse_event_grid(ce, &names())),
            [Hint::Ref("main".into())]
        );
        // a rename names no old key: scan; a tier change is not a change
        let renamed = created.replace("BlobCreated", "BlobRenamed");
        assert_eq!(hints(parse_event_grid(&renamed, &names())), [Hint::Scan]);
        let tier = created.replace("BlobCreated", "BlobTierChanged");
        assert_eq!(hints(parse_event_grid(&tier, &names())), []);
        for body in [
            "###",
            r#"{"x":1}"#,
            r#"{"eventType":"Other.Thing"}"#,
            r#"{"eventType":"Microsoft.Storage.BlobCreated","subject":"/x"}"#,
        ] {
            assert!(
                matches!(parse_event_grid(body, &names()), Parsed::Unparseable(_)),
                "{body}"
            );
        }
    }

    #[test]
    fn r2_event_notifications() {
        // developers.cloudflare.com/r2/buckets/event-notifications/#message-format
        let put = r#"{"account":"3f4b7e3dcab231cbfdaa90a6a28bd548","action":"PutObject","bucket":"acme",
          "object":{"key":"team/app/refs/pr/12","size":16,"eTag":"1eb2"},"eventTime":"2024-05-24T19:36:01.434Z"}"#;
        assert_eq!(hints(parse_r2(put, &names())), [Hint::Ref("pr/12".into())]);
        let del = put.replace("PutObject", "DeleteObject");
        assert_eq!(hints(parse_r2(&del, &names())), [Hint::Ref("pr/12".into())]);
        let copy = r#"{"account":"a","action":"CopyObject","bucket":"acme","object":{"key":"team/app.actions/q/x.json"},
          "copySource":{"bucket":"acme","object":"team/app/refs/old"},"eventTime":"2024-05-24T19:36:01.434Z"}"#;
        assert_eq!(
            hints(parse_r2(copy, &names())),
            [Hint::Object("q/x.json".into())]
        );
        assert_eq!(
            hints(parse_r2(&put.replace("\"acme\"", "\"other\""), &names())),
            []
        );
        for body in [
            "",
            r#"{"action":"PutObject"}"#,
            r#"{"action":"Nope","bucket":"acme","object":{"key":"k"}}"#,
        ] {
            assert!(
                matches!(parse_r2(body, &names()), Parsed::Unparseable(_)),
                "{body}"
            );
        }
    }

    #[test]
    fn azure_get_messages_response() {
        // learn.microsoft.com/rest/api/storageservices/get-messages
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<QueueMessagesList>
  <QueueMessage>
    <MessageId>5974b586-0df3-4e2d-ad0c-18e3892bfca2</MessageId>
    <InsertionTime>Fri, 09 Oct 2009 21:04:30 GMT</InsertionTime>
    <ExpirationTime>Fri, 16 Oct 2009 21:04:30 GMT</ExpirationTime>
    <PopReceipt>YzQ4Yzg1MDItYTc0Ny00OWNjLTkxYTUtZGM0MDFiZDAwYzEw</PopReceipt>
    <TimeNextVisible>Fri, 09 Oct 2009 23:29:20 GMT</TimeNextVisible>
    <DequeueCount>1</DequeueCount>
    <MessageText>{&quot;a&quot;:1}</MessageText>
  </QueueMessage>
</QueueMessagesList>"#;
        let m = parse_azure_messages(xml);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, "5974b586-0df3-4e2d-ad0c-18e3892bfca2");
        assert_eq!(m[0].2, r#"{"a":1}"#);
        assert!(parse_azure_messages("<QueueMessagesList />").is_empty());
    }

    #[test]
    fn names_from_store_urls() {
        assert_eq!(
            store_name("s3://b/team/app").unwrap(),
            StoreName {
                bucket: Some("b".into()),
                prefix: "team/app/".into()
            }
        );
        assert_eq!(store_name("gs://b").unwrap().prefix, "");
        assert_eq!(
            store_name("https://acct.blob.core.windows.net/c/x/").unwrap(),
            StoreName {
                bucket: Some("c".into()),
                prefix: "x/".into()
            }
        );
        let spec = Spec {
            globs: vec![],
            refs: true,
            prefix: Some("actions/".into()),
        };
        let n = Names::new("s3://b/team/app", &spec, true).unwrap();
        assert_eq!(n.refs, "team/app/refs/");
        assert_eq!(n.area.as_deref(), Some("team/app.actions/"));
        assert_eq!(n.bucket.as_deref(), Some("b"));
        assert_eq!(
            Names::new("s3://b/team/app", &spec, false).unwrap().bucket,
            None
        );
    }

    #[test]
    fn source_specs_round_trip_and_resolve() {
        for t in [
            "poll",
            "gcs:projects/p/subscriptions/s",
            "sqs:https://sqs.us-east-1.amazonaws.com/1/q",
            "azure:https://a.queue.core.windows.net/q",
            "r2:acct/queue",
        ] {
            assert_eq!(SourceSpec::parse(t).unwrap().to_string(), t);
        }
        assert!(SourceSpec::parse("r2:queue-only").is_err());
        assert!(SourceSpec::parse("kafka:x").is_err());
        assert!(SourceSpec::parse("sqs").is_err());

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config"),
            "[events]\n\tsource = sqs:https://q/global\n[events \"s3://acme/app\"]\n\tsource = r2:a/q\n",
        )
        .unwrap();
        let cfg = crate::config::Config::load(Some(dir.path()));
        let pick = |flag: Option<SourceSpec>, env: Option<&str>, store: &str| {
            let (s, from) = choose(flag, env, &cfg, store).unwrap();
            (s.to_string(), from)
        };
        assert_eq!(pick(None, None, "s3://acme/app").0, "r2:a/q");
        assert_eq!(pick(None, None, "s3://acme/app/sub").0, "r2:a/q");
        assert_eq!(
            pick(None, None, "s3://acme/other").0,
            "sqs:https://q/global"
        );
        assert_eq!(
            pick(None, Some("poll"), "s3://acme/app"),
            ("poll".into(), "PAIL_EVENTS_SOURCE".into())
        );
        assert_eq!(
            pick(
                Some(SourceSpec::Gcs("s".into())),
                Some("poll"),
                "s3://acme/app"
            )
            .1,
            "flag"
        );
        let empty = crate::config::Config::load(None);
        assert_eq!(
            choose(None, None, &empty, "gs://b/x").unwrap().0,
            SourceSpec::Poll
        );
    }
}
