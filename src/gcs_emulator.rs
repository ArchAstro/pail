//! A GCS emulator for testing pail's `Store` against the XML API that `object_store`'s
//! `GoogleCloudStorage` client speaks. fake-gcs-server does not implement these calls.
//!
//! Implements (buckets are implicit, any bearer token is accepted):
//! - `PUT /{bucket}/{key}`: write. `x-goog-if-generation-match: 0` creates only if absent,
//!   `N` requires the live generation to be `N`; a mismatch is `412`. `x-goog-copy-source`
//!   copies (with the same precondition).
//! - `GET`/`HEAD /{bucket}/{key}`: `ETag`, `Last-Modified`, `Content-Length`,
//!   `x-goog-generation`; `Range: bytes=a-b|a-|-n`, `If-Match`, `If-None-Match`, and
//!   `?generation=N` (only the live generation exists, as with versioning off).
//! - `DELETE /{bucket}/{key}`: `204`, or `404` if absent.
//! - `GET /{bucket}?list-type=2`: `ListBucketResult` with `prefix`, `delimiter`,
//!   `continuation-token`, `start-after`, `max-keys`.
//!
//! Semantics are GCS's: strongly consistent (one lock), and every write, including an
//! overwrite with identical bytes, takes a fresh generation from a counter that never
//! reuses a value, so delete then recreate also gets a new one. The `ETag` is a hash of the
//! bytes, as the XML API's is, so identical bytes share it while generations differ.
//!
//! Does not implement: multipart/resumable uploads (`501`), the JSON API, ACLs, metadata
//! beyond `Content-Type`, object versioning, `If-Modified-Since`, signed URLs, auth checks.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};

struct Object {
    data: Vec<u8>,
    generation: u64,
    etag: String,
    modified: DateTime<Utc>,
    content_type: String,
}

#[derive(Default)]
struct State {
    /// `bucket/key` to object.
    objects: BTreeMap<String, Object>,
    /// Last generation issued; GCS generations are microsecond timestamps, here a counter.
    last_generation: u64,
}

/// A running emulator; dropping it stops the server.
pub struct GcsEmulator {
    server: Arc<tiny_http::Server>,
    addr: std::net::SocketAddr,
}

impl GcsEmulator {
    /// Listen on `addr` (e.g. `127.0.0.1:0` for a free port) and serve on background threads.
    pub fn start(addr: &str) -> Result<GcsEmulator> {
        let server = Arc::new(tiny_http::Server::http(addr).map_err(|e| anyhow!("{addr}: {e}"))?);
        let addr = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| anyhow!("no ip address"))?;
        let state = Arc::new(Mutex::new(State::default()));
        let s = server.clone();
        std::thread::spawn(move || {
            for req in s.incoming_requests() {
                let state = state.clone();
                std::thread::spawn(move || handle(&state, req));
            }
        });
        Ok(GcsEmulator { server, addr })
    }

    /// The value for `PAIL_GCS_BASE_URL`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for GcsEmulator {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

fn reply(status: u16) -> Reply {
    Reply {
        status,
        headers: vec![],
        body: vec![],
    }
}

fn error(status: u16, msg: &str) -> Reply {
    let mut r = reply(status);
    r.headers.push(("Content-Type", "application/xml".into()));
    r.body = format!(
        "<?xml version='1.0' encoding='UTF-8'?><Error><Code>{status}</Code><Message>{msg}</Message></Error>"
    )
    .into_bytes();
    r
}

/// The request headers the emulator looks at.
const HEADERS: [&str; 6] = [
    "x-goog-if-generation-match",
    "x-goog-copy-source",
    "range",
    "if-match",
    "if-none-match",
    "content-type",
];

fn handle(state: &Mutex<State>, mut req: tiny_http::Request) {
    let method = req.method().as_str().to_uppercase();
    let url = req.url().to_string();
    let headers: Vec<(&str, String)> = HEADERS
        .iter()
        .filter_map(|n| {
            let h = req.headers().iter().find(|h| h.field.equiv(n))?;
            Some((*n, h.value.as_str().to_string()))
        })
        .collect();
    let mut body = vec![];
    let _ = req.as_reader().read_to_end(&mut body);
    let r = route(state, &method, &url, &headers, body);
    let mut resp = tiny_http::Response::from_data(r.body).with_status_code(r.status);
    for (k, v) in r.headers {
        if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            resp.add_header(h);
        }
    }
    let _ = req.respond(resp);
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (
            b[i],
            b.get(i + 1).and_then(|c| hex(*c)),
            b.get(i + 2).and_then(|c| hex(*c)),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn route(
    state: &Mutex<State>,
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Vec<u8>,
) -> Reply {
    let h = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.clone())
    };
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let query: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let q = |name: &str| {
        query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let path = path.strip_prefix('/').unwrap_or(path);
    let (bucket, key) = match path.split_once('/') {
        Some((b, k)) => (unescape(b), unescape(k)),
        None => (unescape(path), String::new()),
    };
    if q("uploads").is_some() || q("uploadId").is_some() {
        return error(501, "multipart uploads are not implemented");
    }
    if key.is_empty() {
        return match method {
            "GET" => list(state, &bucket, &q),
            _ => error(405, "method not allowed"),
        };
    }
    let full = format!("{bucket}/{key}");
    let mut st = state.lock().unwrap();
    match method {
        "PUT" => {
            let data = match h("x-goog-copy-source") {
                Some(src) => {
                    let src = unescape(&src);
                    match st.objects.get(src.trim_start_matches('/')) {
                        Some(o) => o.data.clone(),
                        None => return error(404, "copy source not found"),
                    }
                }
                None => body,
            };
            if let Some(want) = h("x-goog-if-generation-match") {
                let live = st.objects.get(&full).map(|o| o.generation).unwrap_or(0);
                if want.trim().parse::<u64>().ok() != Some(live) {
                    return error(412, "Precondition Failed");
                }
            }
            st.last_generation += 1;
            let generation = st.last_generation;
            let o = Object {
                etag: format!("\"{}\"", &blake3::hash(&data).to_hex()[..32]),
                data,
                generation,
                modified: Utc::now(),
                content_type: h("content-type")
                    .unwrap_or_else(|| "application/octet-stream".into()),
            };
            let mut r = reply(200);
            r.headers.push(("ETag", o.etag.clone()));
            r.headers
                .push(("x-goog-generation", generation.to_string()));
            st.objects.insert(full, o);
            r
        }
        "GET" | "HEAD" => {
            let Some(o) = st.objects.get(&full) else {
                return error(404, "No such object");
            };
            if q("generation").is_some_and(|g| g.parse() != Ok(o.generation)) {
                return error(404, "No such object generation");
            }
            if h("if-match").is_some_and(|m| m != o.etag) {
                return error(412, "Precondition Failed");
            }
            if h("if-none-match").is_some_and(|m| m == o.etag) {
                return reply(304);
            }
            let total = o.data.len() as u64;
            let mut r = reply(200);
            r.headers.push(("ETag", o.etag.clone()));
            r.headers.push((
                "Last-Modified",
                o.modified.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
            ));
            r.headers
                .push(("x-goog-generation", o.generation.to_string()));
            r.headers.push(("Content-Type", o.content_type.clone()));
            r.headers.push(("Accept-Ranges", "bytes".into()));
            match h("range").and_then(|s| parse_range(&s, total)) {
                Some(Some((a, b))) => {
                    r.status = 206;
                    r.headers
                        .push(("Content-Range", format!("bytes {a}-{}/{total}", b - 1)));
                    r.body = o.data[a as usize..b as usize].to_vec();
                }
                Some(None) => {
                    let mut e = error(416, "Requested Range Not Satisfiable");
                    e.headers
                        .push(("Content-Range", format!("bytes */{total}")));
                    return e;
                }
                None => r.body = o.data.clone(),
            }
            r
        }
        "DELETE" => match st.objects.remove(&full) {
            Some(_) => reply(204),
            None => error(404, "No such object"),
        },
        _ => error(405, "method not allowed"),
    }
}

/// `None` for a header that is not a single byte range (served whole), `Some(None)` for an
/// unsatisfiable range, else the half-open `(start, end)`.
fn parse_range(s: &str, total: u64) -> Option<Option<(u64, u64)>> {
    let (a, b) = s.strip_prefix("bytes=")?.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        (total.saturating_sub(n), total)
    } else {
        let end = if b.is_empty() {
            total
        } else {
            (b.parse::<u64>().ok()? + 1).min(total)
        };
        (a.parse().ok()?, end)
    };
    Some((start < total && end > start).then_some((start, end)))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn list<'a>(state: &Mutex<State>, bucket: &str, q: &impl Fn(&str) -> Option<&'a str>) -> Reply {
    let prefix = q("prefix").unwrap_or("");
    let delimiter = q("delimiter").filter(|d| !d.is_empty());
    let after = q("continuation-token").or(q("start-after")).unwrap_or("");
    let max: usize = q("max-keys")
        .and_then(|m| m.parse().ok())
        .unwrap_or(1000)
        .min(1000);
    let st = state.lock().unwrap();
    let start = format!("{bucket}/");
    // Objects and rolled-up common prefixes in key order.
    let mut entries: Vec<(String, Option<&Object>)> = vec![];
    for (full, o) in st.objects.range(start.clone()..) {
        let Some(key) = full.strip_prefix(&start) else {
            break;
        };
        if !key.starts_with(prefix) || key <= after {
            continue;
        }
        let rest = &key[prefix.len()..];
        match delimiter.and_then(|d| rest.find(d).map(|i| i + d.len())) {
            Some(i) => {
                let cp = format!("{prefix}{}", &rest[..i]);
                if entries.last().map(|(k, _)| k) != Some(&cp) {
                    entries.push((cp, None));
                }
            }
            None => entries.push((key.to_string(), Some(o))),
        }
    }
    let truncated = entries.len() > max;
    entries.truncate(max);
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult>");
    x += &format!(
        "<Name>{}</Name><Prefix>{}</Prefix>",
        xml_escape(bucket),
        xml_escape(prefix)
    );
    x += &format!(
        "<KeyCount>{}</KeyCount><MaxKeys>{max}</MaxKeys><IsTruncated>{truncated}</IsTruncated>",
        entries.len()
    );
    for (k, o) in &entries {
        match o {
            Some(o) => {
                let modified = o
                    .modified
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                x += &format!(
                    "<Contents><Key>{}</Key><Size>{}</Size><LastModified>{modified}</LastModified><ETag>{}</ETag></Contents>",
                    xml_escape(k),
                    o.data.len(),
                    xml_escape(&o.etag),
                );
            }
            None => {
                x += &format!(
                    "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                    xml_escape(k)
                )
            }
        }
    }
    if truncated {
        // A common prefix resumes after everything under it.
        let (last, o) = entries.last().unwrap();
        let token = if o.is_some() {
            last.clone()
        } else {
            format!("{last}\u{10ffff}")
        };
        x += &format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            xml_escape(&token)
        );
    }
    x += "</ListBucketResult>";
    let mut r = reply(200);
    r.headers.push(("Content-Type", "application/xml".into()));
    r.body = x.into_bytes();
    r
}
