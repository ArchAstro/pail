//! Branch policy: who may read, write and delete what in a repository, in one format that
//! every enforcement point uses.
//!
//! ```json
//! {
//!   "version": 1,
//!   "readers": ["*"],
//!   "admins": ["role:admin"],
//!   "gc": ["role:gc"],
//!   "branches": {
//!     "main":      {"writers": ["lander"], "approvals": 1, "required_checks": ["test"]},
//!     "release/*": {"writers": ["lander"], "approvals": 2},
//!     "_policy":   {"writers": ["role:admin"]},
//!     "*":         {"writers": ["*"]}
//!   }
//! }
//! ```
//!
//! It lives on the repository's `_policy` branch as `policy.json`, so it is versioned and
//! protected by its own rule. Identities are strings (`user:…`, `group:…`, `role:…`, any
//! name such as `lander`, or `*` for everyone); a caller has a principal and groups.
//!
//! Branch globs are exact names or end in `*`; the most specific rule (an exact name, then the
//! longest prefix) decides. A branch no rule names is writable by nobody.
//!
//! What a write to each object needs:
//!
//! ```text
//! refs/<b>, intents/<b>/…, reflog/<b>/…   a writer of <b> (combining stays among <b>'s writers)
//! commits/ pages/ packs/ chunks/          create: a writer of any branch; delete/overwrite: gc
//! locks/shared-…                          a writer of any branch (its lease while it writes)
//! locks/exclusive-…                       gc (its lease while it deletes)
//! HEAD                                    an admin
//! anything, read                          a reader
//! ```
//!
//! Enforcement points: `pail serve` checks every request ([`Policy::decide`]); a broker mints
//! tokens whose GCS access boundary encodes the same rules ([`Policy::gcs_boundary`],
//! `pail policy token`); a lander reads a branch's merge requirements ([`BranchRule`]).

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const BRANCH: &str = "_policy";
pub const FILE: &str = "policy.json";

const CONTENT: &[&str] = &["commits/", "pages/", "packs/", "chunks/"];
/// Longest condition put in one access-boundary rule (STS accepted 664, rejected 856).
const CONDITION_BUDGET: usize = 600;
/// GCS's limit on rules in one Credential Access Boundary.
const MAX_BOUNDARY_RULES: usize = 10;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct BranchRule {
    #[serde(default)]
    pub writers: Vec<String>,
    /// Approvals from people other than the author before a lander moves the branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_checks: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Policy {
    pub version: u32,
    #[serde(default = "everyone")]
    pub readers: Vec<String>,
    #[serde(default)]
    pub admins: Vec<String>,
    #[serde(default)]
    pub gc: Vec<String>,
    #[serde(default)]
    pub branches: BTreeMap<String, BranchRule>,
}

fn everyone() -> Vec<String> {
    vec!["*".into()]
}

/// Who is asking: a principal and the groups or roles it holds.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub principal: String,
    pub groups: Vec<String>,
}

impl Identity {
    pub fn new(principal: &str, groups: &[String]) -> Identity {
        Identity {
            principal: principal.to_string(),
            groups: groups.to_vec(),
        }
    }

    /// Whether any of `who` names this identity.
    pub fn is_any(&self, who: &[String]) -> bool {
        who.iter()
            .any(|w| w == "*" || *w == self.principal || self.groups.iter().any(|g| g == w))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read,
    /// Create a new object, or overwrite/compare-and-swap an existing ref.
    Write,
    /// Delete, or overwrite an immutable object.
    Delete,
}

/// The op a request to `key` is, as `pail serve` and the gateway check it: reads are GET and
/// HEAD; branch objects are written and deleted by their writers; immutable content may only
/// be created, so an unconditional write (no `If-None-Match: *`) or a delete of it counts as
/// rewriting history.
pub fn request_op(method: &str, key: &str, if_none_match: Option<&str>) -> Op {
    let content = CONTENT.iter().any(|p| key.starts_with(p));
    match method {
        "GET" | "HEAD" => Op::Read,
        "PUT" if content && if_none_match != Some("*") => Op::Delete,
        "DELETE" if content => Op::Delete,
        _ => Op::Write,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub allow: bool,
    pub reason: String,
}

fn allow(reason: impl Into<String>) -> Decision {
    Decision {
        allow: true,
        reason: reason.into(),
    }
}

fn deny(reason: impl Into<String>) -> Decision {
    Decision {
        allow: false,
        reason: reason.into(),
    }
}

impl Policy {
    pub fn parse(bytes: &[u8]) -> Result<Policy> {
        let p: Policy = serde_json::from_slice(bytes).context("invalid policy.json")?;
        if p.version != 1 {
            bail!(
                "unsupported policy version {} (this pail reads version 1)",
                p.version
            );
        }
        for glob in p.branches.keys() {
            if glob.is_empty() || glob[..glob.len() - 1].contains('*') {
                bail!("branch pattern {glob:?}: use an exact name or a single trailing *");
            }
        }
        Ok(p)
    }

    /// The policy on the repository's `_policy` branch, if there is one.
    pub async fn load(repo: &crate::repo::Repo) -> Result<Option<Policy>> {
        let Some((head, _)) = repo.read_ref(BRANCH).await? else {
            return Ok(None);
        };
        let root = repo.root_of(Some(head)).await?;
        let Some(entry) = repo.get_entry(root, FILE).await? else {
            return Ok(None);
        };
        let data = repo.read_files(&[entry]).await?.remove(0);
        Ok(Some(Policy::parse(&data)?))
    }

    /// The rule for `branch`, and the pattern that matched: exact first, then the longest
    /// trailing-`*` prefix.
    pub fn rule_for(&self, branch: &str) -> Option<(&str, &BranchRule)> {
        if let Some((k, r)) = self.branches.get_key_value(branch) {
            return Some((k.as_str(), r));
        }
        self.branches
            .iter()
            .filter(|(glob, _)| {
                glob.strip_suffix('*')
                    .is_some_and(|p| branch.starts_with(p))
            })
            .max_by_key(|(glob, _)| glob.len())
            .map(|(k, r)| (k.as_str(), r))
    }

    pub fn may_write_branch(&self, id: &Identity, branch: &str) -> bool {
        self.rule_for(branch)
            .is_some_and(|(_, r)| id.is_any(&r.writers))
    }

    /// Whether some identity outside the branch's writers exists, i.e. not everyone may write.
    pub fn is_protected(&self, branch: &str) -> bool {
        !self
            .rule_for(branch)
            .is_some_and(|(_, r)| r.writers.iter().any(|w| w == "*"))
    }

    /// Whether `id` may write at least one branch (and so create content).
    fn writes_anything(&self, id: &Identity) -> bool {
        self.branches.values().any(|r| id.is_any(&r.writers))
    }

    /// May `id` do `op` to the object `key` (relative to the repository)?
    pub fn decide(&self, id: &Identity, op: Op, key: &str) -> Decision {
        if op == Op::Read {
            return if id.is_any(&self.readers) {
                allow("reader")
            } else {
                deny("not a reader of this repository")
            };
        }
        let branch_key = ["refs/", "intents/", "reflog/"]
            .iter()
            .find_map(|p| key.strip_prefix(p).map(|rest| (*p, rest)));
        if let Some((kind, rest)) = branch_key {
            let branch = match kind {
                "refs/" => rest.to_string(),
                // intents/<branch>/<id>.json and reflog/<branch>/<ts>: the branch is all but the last segment.
                _ => rest
                    .rsplit_once('/')
                    .map(|(b, _)| b.to_string())
                    .unwrap_or_default(),
            };
            return match self.rule_for(&branch) {
                Some((glob, r)) if id.is_any(&r.writers) => {
                    allow(format!("writer of {branch} (rule {glob:?})"))
                }
                Some((glob, _)) => deny(format!(
                    "{branch} is protected (rule {glob:?}): its writers do not include {}",
                    id.principal
                )),
                None => deny(format!("no rule lets anyone write {branch}")),
            };
        }
        if let Some(name) = key.strip_prefix("locks/") {
            return if name.starts_with("shared-") {
                if self.writes_anything(id) {
                    allow("a writer's lease")
                } else {
                    deny("not a writer of any branch")
                }
            } else if name.starts_with("exclusive-") && id.is_any(&self.gc) {
                allow("gc's lease")
            } else {
                deny("only gc takes the exclusive lease")
            };
        }
        if CONTENT.iter().any(|p| key.starts_with(p)) {
            return match op {
                Op::Delete if id.is_any(&self.gc) => allow("gc"),
                Op::Delete => deny("history is create-only: only gc may delete or overwrite it"),
                _ if self.writes_anything(id) => allow("create-only content"),
                _ => deny("not a writer of any branch"),
            };
        }
        if key == "HEAD" {
            return if id.is_any(&self.admins) {
                allow("admin")
            } else {
                deny("only admins set the default branch")
            };
        }
        deny(format!("{key} is outside pail's layout"))
    }

    /// A Google Cloud Storage Credential Access Boundary giving `id` exactly what this policy
    /// allows in the repository at `gs://<bucket>/<prefix>`: read (with listing), create-only
    /// content, full access to the refs/intents/reflog of branches it may write (minus more
    /// specific protected ones), delete for gc. At most four rules (GCS allows ten).
    pub fn gcs_boundary(&self, id: &Identity, bucket: &str, prefix: &str) -> Result<Value> {
        let prefix = prefix.trim_matches('/');
        let base = if prefix.is_empty() {
            format!("projects/_/buckets/{bucket}/objects/")
        } else {
            format!("projects/_/buckets/{bucket}/objects/{prefix}/")
        };
        let list_base = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        let resource = format!("//storage.googleapis.com/projects/_/buckets/{bucket}");
        let rule = |role: &str, expr: String| json!({"availableResource": resource, "availablePermissions": [format!("inRole:roles/storage.{role}")], "availabilityCondition": {"expression": expr}});
        let mut rules = vec![];
        if id.is_any(&self.readers) {
            rules.push(rule(
                "objectViewer",
                format!("resource.name.startsWith('{base}') || api.getAttribute('storage.googleapis.com/objectListPrefix', '').startsWith('{list_base}')"),
            ));
        }
        if self.writes_anything(id) {
            let content: Vec<String> = CONTENT
                .iter()
                .map(|c| format!("resource.name.startsWith('{base}{c}')"))
                .collect();
            rules.push(rule("objectCreator", content.join(" || ")));
            // Branch objects: each writable pattern, minus more specific patterns it may not write.
            let mut grants = vec![];
            for (glob, r) in &self.branches {
                if !id.is_any(&r.writers) {
                    continue;
                }
                let lit = glob.trim_end_matches('*');
                let exact = !glob.ends_with('*');
                let mut excluded = vec![];
                for (other, o) in &self.branches {
                    let olit = other.trim_end_matches('*');
                    let more_specific = other != glob
                        && olit.starts_with(lit)
                        && (olit.len() > lit.len()
                            || (!other.ends_with('*') && glob.ends_with('*')));
                    if !exact && more_specific && !id.is_any(&o.writers) {
                        excluded.push((olit.to_string(), !other.ends_with('*')));
                    }
                }
                for kind in ["refs/", "intents/", "reflog/"] {
                    let mut cond = if exact && kind == "refs/" {
                        format!("resource.name == '{base}refs/{lit}'")
                    } else if exact {
                        format!("resource.name.startsWith('{base}{kind}{lit}/')")
                    } else {
                        format!("resource.name.startsWith('{base}{kind}{lit}')")
                    };
                    for (olit, oexact) in &excluded {
                        let ex = if *oexact && kind == "refs/" {
                            format!("resource.name == '{base}refs/{olit}'")
                        } else if *oexact {
                            format!("resource.name.startsWith('{base}{kind}{olit}/')")
                        } else {
                            format!("resource.name.startsWith('{base}{kind}{olit}')")
                        };
                        cond = format!("{cond} && !({ex})");
                    }
                    grants.push(format!("({cond})"));
                }
            }
            // A writer's lease: written, refreshed and deleted while it writes.
            grants.push(format!("(resource.name.startsWith('{base}locks/shared-'))"));
            // STS rejects long conditions (measured 2026-10: 664 characters accepted, 856 not),
            // so pack the grants into as few rules as fit a conservative budget.
            let mut chunk = String::new();
            for g in grants {
                if !chunk.is_empty() && chunk.len() + 4 + g.len() > CONDITION_BUDGET {
                    rules.push(rule("objectAdmin", std::mem::take(&mut chunk)));
                }
                if g.len() > CONDITION_BUDGET {
                    bail!(
                        "a branch rule's condition is too long for a GCS access boundary ({} characters): use fewer protected patterns under one glob",
                        g.len()
                    );
                }
                chunk = if chunk.is_empty() {
                    g
                } else {
                    format!("{chunk} || {g}")
                };
            }
            if !chunk.is_empty() {
                rules.push(rule("objectAdmin", chunk));
            }
        }
        if id.is_any(&self.gc) {
            let content: Vec<String> = CONTENT
                .iter()
                .chain(&["locks/exclusive-"])
                .map(|c| format!("resource.name.startsWith('{base}{c}')"))
                .collect();
            rules.push(rule("objectAdmin", content.join(" || ")));
        }
        if id.is_any(&self.admins) {
            rules.push(rule(
                "objectAdmin",
                format!("resource.name == '{base}HEAD'"),
            ));
        }
        if rules.is_empty() {
            bail!("{} may not access this repository", id.principal);
        }
        if rules.len() > MAX_BOUNDARY_RULES {
            bail!(
                "this policy needs {} access-boundary rules for {}; GCS allows {MAX_BOUNDARY_RULES}",
                rules.len(),
                id.principal
            );
        }
        Ok(json!({"accessBoundary": {"accessBoundaryRules": rules}}))
    }
}

/// Exchange `source_token` (a credential with access to the bucket: the broker's) for one
/// limited by `boundary`, at Google's Security Token Service. Returns (token, expires_at).
pub async fn gcs_downscope(source_token: &str, boundary: &Value) -> Result<(String, u64)> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair(
            "grant_type",
            "urn:ietf:params:oauth:grant-type:token-exchange",
        )
        .append_pair(
            "subject_token_type",
            "urn:ietf:params:oauth:token-type:access_token",
        )
        .append_pair(
            "requested_token_type",
            "urn:ietf:params:oauth:token-type:access_token",
        )
        .append_pair("subject_token", source_token)
        .append_pair("options", &boundary.to_string())
        .finish();
    let resp = reqwest::Client::new()
        .post("https://sts.googleapis.com/v1/token")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await
        .context("calling Google STS")?;
    let status = resp.status();
    let body: Value = serde_json::from_slice(&resp.bytes().await?).unwrap_or_default();
    if !status.is_success() {
        bail!("STS refused the exchange ({status}): {body}");
    }
    let token = body["access_token"]
        .as_str()
        .context("STS returned no token")?
        .to_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    Ok((token, now + body["expires_in"].as_u64().unwrap_or(3600)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::parse(
            br#"{"version":1,"admins":["role:admin"],"gc":["role:gc"],"branches":{
                "main":{"writers":["lander"],"approvals":1,"required_checks":["test"]},
                "release/*":{"writers":["lander"],"approvals":2},
                "_policy":{"writers":["role:admin"]},
                "*":{"writers":["*"]}}}"#,
        )
        .unwrap()
    }

    #[test]
    fn decisions() {
        let p = policy();
        let alice = Identity::new("user:alice", &[]);
        let lander = Identity::new("lander", &[]);
        let admin = Identity::new("user:root", &["role:admin".into()]);
        let w = |id: &Identity, key: &str| p.decide(id, Op::Write, key).allow;
        assert!(!w(&alice, "refs/main"));
        assert!(!w(&alice, "refs/release/2026"));
        assert!(
            !w(&alice, "intents/main/0001-ab.json"),
            "no combining into a protected branch"
        );
        assert!(!w(&alice, "refs/_policy"));
        assert!(
            w(&alice, "refs/pr/hello")
                && w(&alice, "intents/pr/hello/1.json")
                && w(&alice, "refs/alice/wip")
        );
        assert!(w(&alice, "commits/abc") && w(&alice, "packs/x.pack"));
        assert!(!p.decide(&alice, Op::Delete, "packs/x.pack").allow);
        assert!(!p.decide(&lander, Op::Delete, "packs/x.pack").allow);
        assert!(
            p.decide(
                &Identity::new("svc", &["role:gc".into()]),
                Op::Delete,
                "packs/x.pack"
            )
            .allow
        );
        assert!(w(&lander, "refs/main") && w(&lander, "refs/release/2026"));
        assert!(w(&admin, "refs/_policy") && w(&admin, "HEAD") && !w(&alice, "HEAD"));
        assert!(p.decide(&alice, Op::Read, "refs/main").allow);
        assert!(p.is_protected("main") && p.is_protected("release/x") && !p.is_protected("pr/x"));
        assert_eq!(p.rule_for("release/2026").unwrap().0, "release/*");
        assert_eq!(p.rule_for("main").unwrap().1.required_checks, vec!["test"]);
        let strict =
            Policy::parse(br#"{"version":1,"branches":{"main":{"writers":["lander"]}}}"#).unwrap();
        assert!(
            !strict.decide(&alice, Op::Write, "refs/feature").allow,
            "unnamed branches are writable by nobody"
        );
        assert!(Policy::parse(br#"{"version":1,"branches":{"a*b":{"writers":["*"]}}}"#).is_err());
    }

    #[test]
    fn boundaries_encode_the_same_rules() {
        let p = policy();
        let b = p
            .gcs_boundary(&Identity::new("user:alice", &[]), "bkt", "team/app")
            .unwrap();
        let rules = b["accessBoundary"]["accessBoundaryRules"]
            .as_array()
            .unwrap();
        assert!(rules.len() >= 3 && rules.len() <= MAX_BOUNDARY_RULES);
        assert!(rules.iter().all(|r| {
            r["availabilityCondition"]["expression"]
                .as_str()
                .unwrap()
                .len()
                <= CONDITION_BUDGET
        }));
        let refs: String = rules[2..]
            .iter()
            .map(|r| {
                r["availabilityCondition"]["expression"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(" || ");
        let base = "projects/_/buckets/bkt/objects/team/app/";
        assert!(refs.contains(&format!("resource.name.startsWith('{base}refs/')")));
        assert!(refs.contains(&format!("!(resource.name == '{base}refs/main')")));
        assert!(refs.contains(&format!(
            "!(resource.name.startsWith('{base}refs/release/'))"
        )));
        assert!(refs.contains(&format!(
            "!(resource.name.startsWith('{base}intents/main/'))"
        )));
        assert!(refs.contains(&format!("!(resource.name == '{base}refs/_policy')")));
        let lander = p
            .gcs_boundary(&Identity::new("lander", &[]), "bkt", "team/app")
            .unwrap();
        let expr = lander["accessBoundary"]["accessBoundaryRules"].to_string();
        assert!(expr.contains(&format!("resource.name == '{base}refs/main'")));
        assert!(
            p.gcs_boundary(&Identity::new("user:mallory", &[]), "bkt", "x")
                .is_ok(),
            "readers include everyone here"
        );
    }
}
