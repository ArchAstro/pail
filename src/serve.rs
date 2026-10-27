//! `pail serve`: a stateless proxy that gives people without bucket credentials a store at
//! `https://host/<repo>` (protocol in `httpstore`). It holds the bucket credentials itself and
//! decides per request whether a bearer token may read or write a repository by asking an
//! auth command, so any identity system can sit in front (ArchAstro, an OIDC verifier, a
//! static token list):
//!
//! ```text
//! PAIL_AUTH_TOKEN=<bearer> PAIL_AUTH_REPO=<repo path> PAIL_AUTH_OP=read|write <cmd>
//!     exit 0: allowed (stdout's first line, if any, names the principal for the log)
//!     exit 3: a known token that may not do this (403)
//!     other:  the token is not accepted (401: the client fetches a new credential)
//! ```
//!
//! Answers are cached for a minute per (token, repo, op). Nothing else is kept between
//! requests, so any number of instances can serve one bucket.
//!
//! The auth command's stdout names the caller for the repository's policy (see `policy`): the
//! first line is the principal (`user:alice@acme.dev`), further lines its groups and roles
//! (`group:eng`, `role:admin`). When the repository has a `_policy` branch, every request is
//! also checked against it: a write to `refs/main` by someone the policy does not name as a
//! writer answers 403 with the reason.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use bytes::Bytes;

use crate::httpstore::ListEntry;
use crate::policy::{Identity, Op as PolicyOp, Policy};
use crate::store::{Cas, Store, Version};

/// Largest request body: a full pack plus headroom.
const MAX_BODY: usize = 96 << 20;
const AUTH_TTL: Duration = Duration::from_secs(60);

pub struct Proxy {
    root: String,
    auth: Option<String>,
    stores: Mutex<HashMap<String, Store>>,
    decisions: Mutex<HashMap<(String, String, &'static str), (u16, Identity, Instant)>>,
    policies: Mutex<HashMap<String, (Option<Arc<Policy>>, Instant)>>,
}

/// How long a repository's policy is trusted before it is read again.
const POLICY_TTL: Duration = Duration::from_secs(30);

struct Reply {
    status: u16,
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
}

impl Reply {
    fn new(status: u16, body: impl Into<Vec<u8>>) -> Reply {
        Reply {
            status,
            body: body.into(),
            headers: vec![],
        }
    }
    fn header(mut self, k: &'static str, v: String) -> Reply {
        self.headers.push((k, v));
        self
    }
}

/// A version token as an ETag, and back.
fn etag(v: &Version) -> String {
    format!(
        "\"{}\"",
        hex::encode(serde_json::to_vec(v).unwrap_or_default())
    )
}

fn unetag(s: &str) -> Option<Version> {
    serde_json::from_slice(&hex::decode(s.trim().trim_matches('"')).ok()?).ok()
}

fn valid_repo(repo: &str) -> bool {
    repo.split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    })
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

impl Proxy {
    pub fn new(root: &str, auth: Option<String>) -> Proxy {
        Proxy {
            root: root.trim_end_matches('/').to_string(),
            auth,
            stores: Mutex::new(HashMap::new()),
            decisions: Mutex::new(HashMap::new()),
            policies: Mutex::new(HashMap::new()),
        }
    }

    async fn policy(&self, repo: &str, store: &Store) -> Result<Option<Arc<Policy>>> {
        if let Some((p, at)) = self.policies.lock().unwrap().get(repo) {
            if at.elapsed() < POLICY_TTL {
                return Ok(p.clone());
            }
        }
        let r = crate::repo::Repo::with_store(store.clone(), None);
        let p = Policy::load(&r).await?.map(Arc::new);
        self.policies
            .lock()
            .unwrap()
            .insert(repo.to_string(), (p.clone(), Instant::now()));
        Ok(p)
    }

    fn store(&self, repo: &str) -> Result<Store> {
        if let Some(s) = self.stores.lock().unwrap().get(repo) {
            return Ok(s.clone());
        }
        let url = if repo.is_empty() {
            self.root.clone()
        } else {
            format!("{}/{repo}", self.root)
        };
        let s = Store::open(&url)?;
        self.stores
            .lock()
            .unwrap()
            .insert(repo.to_string(), s.clone());
        Ok(s)
    }

    /// Ok(principal) or the reply refusing the request.
    fn authorize(
        &self,
        token: Option<&str>,
        repo: &str,
        op: &'static str,
    ) -> Result<Identity, Reply> {
        let Some(cmd) = &self.auth else {
            return Ok(Identity::new("anonymous", &[]));
        };
        let Some(token) = token else {
            return Err(Reply::new(401, "a bearer token is required")
                .header("WWW-Authenticate", "Bearer".into()));
        };
        let key = (
            blake3::hash(token.as_bytes()).to_hex().to_string(),
            repo.to_string(),
            op,
        );
        let answer = |status: u16, id: Identity| match status {
            200 => Ok(id),
            403 => Err(Reply::new(403, format!("not allowed to {op} {repo}"))),
            _ => Err(Reply::new(401, "the token was not accepted")
                .header("WWW-Authenticate", "Bearer".into())),
        };
        if let Some((status, id, at)) = self.decisions.lock().unwrap().get(&key) {
            if at.elapsed() < AUTH_TTL {
                return answer(*status, id.clone());
            }
        }
        let out = Command::new("sh")
            .args(["-c", cmd])
            .env("PAIL_AUTH_TOKEN", token)
            .env("PAIL_AUTH_REPO", repo)
            .env("PAIL_AUTH_OP", op)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output();
        let mut id = Identity::new("anonymous", &[]);
        let status = match &out {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
                if let Some(who) = lines.next() {
                    id = Identity::new(who, &lines.map(str::to_string).collect::<Vec<_>>());
                    eprintln!("auth: {who} may {op} {repo}");
                }
                200
            }
            Ok(o) if o.status.code() == Some(3) => 403,
            Ok(_) => 401,
            // The auth command could not run: not a verdict on the token, so not cached.
            Err(e) => return Err(Reply::new(503, format!("auth command failed: {e}"))),
        };
        self.decisions
            .lock()
            .unwrap()
            .insert(key, (status, id.clone(), Instant::now()));
        answer(status, id)
    }

    async fn handle(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Bytes,
    ) -> Reply {
        match self.route(method, url, headers, body).await {
            Ok(r) | Err(r) => r,
        }
    }

    async fn route(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Bytes,
    ) -> Result<Reply, Reply> {
        let h = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        let path = path.trim_start_matches('/');
        let (repo, rest) = if let Some(i) = path
            .find("_o/")
            .filter(|&i| i == 0 || path.as_bytes()[i - 1] == b'/')
        {
            (path[..i].trim_end_matches('/'), &path[i..])
        } else if path == "_ls" || path.ends_with("/_ls") {
            (path.trim_end_matches("_ls").trim_end_matches('/'), "_ls")
        } else if path.is_empty() || path == "_health" {
            return Ok(Reply::new(200, "pail proxy\n"));
        } else {
            return Err(Reply::new(404, "not a pail proxy route"));
        };
        if !repo.is_empty() && !valid_repo(repo) {
            return Err(Reply::new(400, "bad repository path"));
        }
        let op = if matches!(method, "GET" | "HEAD") {
            "read"
        } else {
            "write"
        };
        let token = h("authorization")
            .and_then(|a| a.strip_prefix("Bearer "))
            .map(str::trim);
        let caller = self.authorize(token, repo, op)?;
        let store = self
            .store(repo)
            .map_err(|e| Reply::new(500, format!("{e:#}")))?;
        let fail = |e: anyhow::Error| Reply::new(502, format!("{e:#}"));
        let policy = self
            .policy(repo, &store)
            .await
            .map_err(|e| Reply::new(500, format!("reading the policy: {e:#}")))?;
        let check = |op: PolicyOp, key: &str| -> Result<(), Reply> {
            match &policy {
                Some(p) => {
                    let d = p.decide(&caller, op, key);
                    if d.allow {
                        Ok(())
                    } else {
                        Err(Reply::new(403, format!("policy: {}", d.reason)))
                    }
                }
                None => Ok(()),
            }
        };

        if rest == "_ls" {
            if method != "GET" {
                return Err(Reply::new(405, "GET only"));
            }
            let prefix = url::form_urlencoded::parse(query.as_bytes())
                .find(|(k, _)| k == "prefix")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default();
            check(PolicyOp::Read, &prefix)?;
            let items: Vec<ListEntry> = store
                .list_meta(&prefix)
                .await
                .map_err(fail)?
                .into_iter()
                .map(|o| ListEntry {
                    key: o.key,
                    size: o.size,
                    modified: o.modified,
                })
                .collect();
            return Ok(Reply::new(200, serde_json::to_vec(&items).unwrap())
                .header("Content-Type", "application/json".into()));
        }
        let key = &rest[3..];
        if !valid_key(key) {
            return Err(Reply::new(400, "bad key"));
        }
        let pop = crate::policy::request_op(method, key, h("if-none-match"));
        check(pop, key)?;
        match method {
            "HEAD" => match store.size(key).await.map_err(fail)? {
                Some(n) => Ok(Reply::new(200, vec![]).header("X-Pail-Size", n.to_string())),
                None => Err(Reply::new(404, vec![])),
            },
            "GET" => {
                if let Some(r) = h("range").and_then(parse_range) {
                    return match store.read_range(key, r.clone()).await.map_err(fail)? {
                        Some((b, total)) => {
                            let end = r.start + b.len() as u64;
                            Ok(Reply::new(206, b.to_vec()).header(
                                "Content-Range",
                                format!("bytes {}-{}/{total}", r.start, end.saturating_sub(1)),
                            ))
                        }
                        None => Err(Reply::new(404, "no such object")),
                    };
                }
                match store.get_opt(key).await.map_err(fail)? {
                    Some((b, v)) => Ok(Reply::new(200, b.to_vec()).header("ETag", etag(&v))),
                    None => Err(Reply::new(404, "no such object")),
                }
            }
            "PUT" => {
                let expected = match (h("if-none-match"), h("if-match")) {
                    (Some("*"), _) => Some(None),
                    (_, Some(t)) => Some(Some(
                        unetag(t).ok_or_else(|| Reply::new(400, "bad If-Match"))?,
                    )),
                    _ => None,
                };
                match expected {
                    Some(exp) => match store.cas(key, body, exp.as_ref()).await.map_err(fail)? {
                        Cas::Ok(v) => Ok(Reply::new(200, vec![]).header("ETag", etag(&v))),
                        Cas::Conflict => Err(Reply::new(412, "precondition failed")),
                    },
                    None => {
                        store.put(key, body).await.map_err(fail)?;
                        Ok(Reply::new(200, vec![]))
                    }
                }
            }
            "DELETE" => {
                store.delete(key).await.map_err(fail)?;
                Ok(Reply::new(204, vec![]))
            }
            _ => Err(Reply::new(405, "method not allowed")),
        }
    }
}

/// `bytes=a-b` (inclusive) as a half-open range; other forms are not used by pail.
fn parse_range(s: &str) -> Option<std::ops::Range<u64>> {
    let (a, b) = s.strip_prefix("bytes=")?.split_once('-')?;
    let (a, b): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
    (b >= a).then_some(a..b + 1)
}

pub async fn serve(root: &str, addr: &str, auth: Option<String>, anonymous: bool) -> Result<()> {
    if auth.is_none() && !anonymous {
        bail!(
            "pass --auth-command <cmd> to check tokens, or --allow-anonymous for a trusted network"
        );
    }
    let server = tiny_http::Server::http(addr).map_err(|e| anyhow!("listening on {addr}: {e}"))?;
    let mode = if auth.is_some() {
        "tokens checked by the auth command"
    } else {
        "anonymous access"
    };
    eprintln!("pail proxy for {root} at http://{addr}/<repo>  ({mode})");
    let proxy = Arc::new(Proxy::new(root, auth));
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        for mut req in server.incoming_requests() {
            let proxy = proxy.clone();
            let handle = handle.clone();
            std::thread::spawn(move || {
                let method = req.method().as_str().to_uppercase();
                let url = req.url().to_string();
                let headers: Vec<(String, String)> = req
                    .headers()
                    .iter()
                    .map(|h| (h.field.as_str().to_string(), h.value.as_str().to_string()))
                    .collect();
                let mut body = vec![];
                if req
                    .as_reader()
                    .take(MAX_BODY as u64 + 1)
                    .read_to_end(&mut body)
                    .is_err()
                    || body.len() > MAX_BODY
                {
                    let _ = req.respond(
                        tiny_http::Response::from_string("body too large").with_status_code(413),
                    );
                    return;
                }
                let reply =
                    handle.block_on(proxy.handle(&method, &url, &headers, Bytes::from(body)));
                let mut resp =
                    tiny_http::Response::from_data(reply.body).with_status_code(reply.status);
                for (k, v) in reply.headers {
                    if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                        resp.add_header(h);
                    }
                }
                let _ = req.respond(resp);
            });
        }
    })
    .await?;
    Ok(())
}
