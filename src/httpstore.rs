//! `https://host/<repo>`: a store reached through a pail proxy (`pail serve`) instead of the
//! bucket, for people without bucket credentials. The proxy holds those; clients send a bearer
//! token from their credential provider. The protocol is the few operations pail needs:
//!
//! ```text
//! GET    <base>/_o/<key>            object (Range: bytes=a-b for part); ETag = version token
//! HEAD   <base>/_o/<key>            size and version
//! PUT    <base>/_o/<key>            write; If-None-Match: * creates, If-Match: <etag> swaps
//!                                   (412 when the condition fails)
//! DELETE <base>/_o/<key>
//! GET    <base>/_ls?prefix=<p>      [{"key","size","modified"}] under the prefix
//! ```
//!
//! Version tokens are opaque to the client: whatever the proxy's store uses (an S3 ETag, a GCS
//! generation) round-trips through ETag / If-Match.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, TimeZone, Utc};
use futures::stream::{self, BoxStream, StreamExt};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetRange, GetResult, GetResultPayload, ListResult, MultipartUpload,
    ObjectMeta, ObjectStore, PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    Result,
};
use reqwest::StatusCode;
use reqwest::header::{
    CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_MATCH, IF_NONE_MATCH, LAST_MODIFIED, RANGE,
};
use serde::{Deserialize, Serialize};

use crate::creds::BearerProvider;

const STORE: &str = "pail https";

#[derive(Serialize, Deserialize)]
pub struct ListEntry {
    pub key: String,
    pub size: u64,
    pub modified: i64,
}

pub struct HttpStore {
    base: String,
    client: reqwest::Client,
    auth: Option<Arc<BearerProvider>>,
}

impl fmt::Debug for HttpStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HttpStore({})", self.base)
    }
}

impl fmt::Display for HttpStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pail proxy at {}", self.base)
    }
}

fn generic(msg: impl Into<String>) -> object_store::Error {
    object_store::Error::Generic {
        store: STORE,
        source: msg.into().into(),
    }
}

impl HttpStore {
    pub fn new(base: &str, auth: Option<Arc<BearerProvider>>) -> Result<HttpStore> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| generic(e.to_string()))?;
        Ok(HttpStore {
            base: base.trim_end_matches('/').to_string(),
            client,
            auth,
        })
    }

    fn url(&self, location: &Path) -> String {
        // Keys are made of safe characters (hex, `/`, `.`, `-`, `_`), so no escaping is needed.
        format!("{}/_o/{}", self.base, location.as_ref())
    }

    async fn send(&self, req: reqwest::RequestBuilder, path: &str) -> Result<reqwest::Response> {
        let req = match &self.auth {
            Some(a) => req.bearer_auth(a.token().await?),
            None => req,
        };
        let resp = req
            .send()
            .await
            .map_err(|e| generic(format!("{path}: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        let source: Box<dyn std::error::Error + Send + Sync> =
            format!("{status}: {}", body.trim()).into();
        let path = path.to_string();
        Err(match status {
            StatusCode::NOT_FOUND => object_store::Error::NotFound { path, source },
            StatusCode::PRECONDITION_FAILED => object_store::Error::Precondition { path, source },
            StatusCode::CONFLICT => object_store::Error::AlreadyExists { path, source },
            StatusCode::NOT_MODIFIED => object_store::Error::NotModified { path, source },
            StatusCode::UNAUTHORIZED => object_store::Error::Unauthenticated { path, source },
            StatusCode::FORBIDDEN => object_store::Error::PermissionDenied { path, source },
            _ => object_store::Error::Generic {
                store: STORE,
                source,
            },
        })
    }
}

fn header<'a>(r: &'a reqwest::Response, name: reqwest::header::HeaderName) -> Option<&'a str> {
    r.headers().get(name).and_then(|v| v.to_str().ok())
}

fn modified(r: &reqwest::Response) -> DateTime<Utc> {
    header(r, LAST_MODIFIED)
        .and_then(|s| DateTime::parse_from_rfc2822(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
}

#[async_trait]
impl ObjectStore for HttpStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        let body: Bytes = payload.into();
        let mut req = self
            .client
            .put(self.url(location))
            .header(CONTENT_LENGTH, body.len())
            .body(body);
        let create = matches!(opts.mode, PutMode::Create);
        req = match opts.mode {
            PutMode::Overwrite => req,
            PutMode::Create => req.header(IF_NONE_MATCH, "*"),
            PutMode::Update(v) => req.header(
                IF_MATCH,
                v.e_tag.ok_or_else(|| generic("update without a version"))?,
            ),
        };
        let resp = match self.send(req, location.as_ref()).await {
            // A failed create means the object is there.
            Err(object_store::Error::Precondition { path, source }) if create => {
                return Err(object_store::Error::AlreadyExists { path, source });
            }
            r => r?,
        };
        Ok(PutResult {
            e_tag: header(&resp, ETAG).map(str::to_string),
            version: None,
            extensions: Default::default(),
        })
    }

    async fn put_multipart_opts(
        &self,
        _: &Path,
        _: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        Err(object_store::Error::NotImplemented {
            operation: "multipart upload".into(),
            implementer: STORE.into(),
        })
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        let url = self.url(location);
        let mut req = if options.head {
            self.client.head(&url)
        } else {
            self.client.get(&url)
        };
        if let Some(r) = &options.range {
            req = req.header(
                RANGE,
                match r {
                    GetRange::Bounded(r) => {
                        format!("bytes={}-{}", r.start, r.end.saturating_sub(1))
                    }
                    GetRange::Offset(o) => format!("bytes={o}-"),
                    GetRange::Suffix(n) => format!("bytes=-{n}"),
                },
            );
        }
        let resp = self.send(req, location.as_ref()).await?;
        let e_tag = header(&resp, ETAG).map(str::to_string);
        let last_modified = modified(&resp);
        // HEAD answers carry the size in X-Pail-Size (their Content-Length is the empty body's).
        let len: u64 = header(
            &resp,
            reqwest::header::HeaderName::from_static("x-pail-size"),
        )
        .or_else(|| header(&resp, CONTENT_LENGTH))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
        // "bytes a-b/total" for a range answer.
        let (range, size) = match header(&resp, CONTENT_RANGE).and_then(parse_content_range) {
            Some((r, total)) => (r, total),
            None => (0..len, len),
        };
        let meta = ObjectMeta {
            location: location.clone(),
            last_modified,
            size,
            e_tag,
            version: None,
        };
        let body = if options.head {
            Bytes::new()
        } else {
            resp.bytes().await.map_err(|e| generic(e.to_string()))?
        };
        Ok(GetResult {
            payload: GetResultPayload::Stream(stream::once(async move { Ok(body) }).boxed()),
            meta,
            range,
            attributes: Default::default(),
            extensions: Default::default(),
        })
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        let me = HttpStore {
            base: self.base.clone(),
            client: self.client.clone(),
            auth: self.auth.clone(),
        };
        let me = Arc::new(me);
        locations
            .then(move |p| {
                let me = me.clone();
                async move {
                    let p = p?;
                    match me.send(me.client.delete(me.url(&p)), p.as_ref()).await {
                        Ok(_) | Err(object_store::Error::NotFound { .. }) => Ok(p),
                        Err(e) => Err(e),
                    }
                }
            })
            .boxed()
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        let url = format!("{}/_ls", self.base);
        let prefix = prefix.map(|p| p.as_ref().to_string()).unwrap_or_default();
        let me = HttpStore {
            base: self.base.clone(),
            client: self.client.clone(),
            auth: self.auth.clone(),
        };
        stream::once(async move {
            let q: String = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("prefix", &prefix)
                .finish();
            let resp = me
                .send(me.client.get(format!("{url}?{q}")), &prefix)
                .await?;
            let body = resp.bytes().await.map_err(|e| generic(e.to_string()))?;
            let items: Vec<ListEntry> =
                serde_json::from_slice(&body).map_err(|e| generic(e.to_string()))?;
            Ok::<_, object_store::Error>(stream::iter(items.into_iter().map(|e| {
                Ok(ObjectMeta {
                    location: Path::from(e.key),
                    last_modified: Utc
                        .timestamp_opt(e.modified, 0)
                        .single()
                        .unwrap_or_default(),
                    size: e.size,
                    e_tag: None,
                    version: None,
                })
            })))
        })
        .map(|r| match r {
            Ok(s) => s.boxed(),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    async fn list_with_delimiter(&self, _: Option<&Path>) -> Result<ListResult> {
        Err(object_store::Error::NotImplemented {
            operation: "list_with_delimiter".into(),
            implementer: STORE.into(),
        })
    }

    async fn copy_opts(&self, _: &Path, _: &Path, _: CopyOptions) -> Result<()> {
        Err(object_store::Error::NotImplemented {
            operation: "copy".into(),
            implementer: STORE.into(),
        })
    }
}

fn parse_content_range(s: &str) -> Option<(Range<u64>, u64)> {
    let rest = s.strip_prefix("bytes ")?;
    let (r, total) = rest.split_once('/')?;
    let (a, b) = r.split_once('-')?;
    let (a, b): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
    Some((a..b + 1, total.parse().ok()?))
}
