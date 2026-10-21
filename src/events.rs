//! Ref events: what extensions (PR bots, CI, mirrors, notifications) react to.
//!
//! A pail repository has no server to send webhooks, and needs none: every publish is a
//! write to `refs/<branch>`, and object stores already announce writes. `pail events` turns
//! those announcements into one JSON line per ref change:
//!
//! ```text
//! {"branch":"pr/12","commit":"9f…","old":"41…","source":"gcs","at":1792553000123}
//! ```
//!
//! Sources:
//!
//! - `gcs`: the bucket's own Pub/Sub notifications (`gcloud storage buckets notifications
//!   create gs://BUCKET --topic=T --object-prefix=<repo>/refs/ --event-types=OBJECT_FINALIZE,
//!   OBJECT_DELETE`), pulled from a subscription. No polling, no server.
//! - `poll`: list the refs every interval: for `file://`, emulators, or stores without
//!   notifications. Same output.
//!
//! Notifications are at-least-once and unordered, so an event is a nudge: the ref is re-read
//! and an event is emitted only when it differs from the last one emitted for that branch.
//! Consumers should be idempotent anyway (key work by commit).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::hash::Hash;
use crate::repo::Repo;

#[derive(Debug, Clone, Serialize)]
pub struct RefEvent {
    pub branch: String,
    /// The branch's commit now; None when it was deleted.
    pub commit: Option<Hash>,
    /// The commit this watcher last saw for the branch (None: new to it).
    pub old: Option<Hash>,
    pub source: &'static str,
    /// Unix milliseconds when the event was emitted.
    pub at: u64,
}

pub enum Source {
    Poll {
        interval: Duration,
    },
    GcsPubSub {
        subscription: String,
        token: Arc<crate::creds::Source>,
    },
}

/// `*` matches any run of characters (including `/`); otherwise literal.
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let mut rest = s;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
    }
    true
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Watch refs matching any of `globs` (all when empty) and call `emit` per change. With
/// `initial`, every matching branch is emitted once at start. Runs until `emit` errs.
pub async fn watch(
    repo: &Repo,
    source: Source,
    globs: &[String],
    initial: bool,
    mut emit: impl FnMut(RefEvent) -> Result<()>,
) -> Result<()> {
    let wanted = |b: &str| globs.is_empty() || globs.iter().any(|g| glob_match(g, b));
    let mut seen: HashMap<String, Option<Hash>> = HashMap::new();
    for (b, h) in repo.list_refs().await? {
        if wanted(&b) {
            if initial {
                emit(RefEvent {
                    branch: b.clone(),
                    commit: Some(h),
                    old: None,
                    source: "initial",
                    at: now_ms(),
                })?;
            }
            seen.insert(b, Some(h));
        }
    }
    let mut changed = |branch: String,
                       commit: Option<Hash>,
                       source: &'static str,
                       seen: &mut HashMap<String, Option<Hash>>|
     -> Result<()> {
        let old = seen.get(&branch).copied().flatten();
        if seen.get(&branch) == Some(&commit) || (commit.is_none() && !seen.contains_key(&branch)) {
            return Ok(()); // a duplicate or reordered notification
        }
        seen.insert(branch.clone(), commit);
        emit(RefEvent {
            branch,
            commit,
            old,
            source,
            at: now_ms(),
        })
    };
    match source {
        Source::Poll { interval } => loop {
            tokio::time::sleep(interval).await;
            let now: HashMap<String, Hash> = repo
                .list_refs()
                .await?
                .into_iter()
                .filter(|(b, _)| wanted(b))
                .collect();
            let gone: Vec<String> = seen
                .keys()
                .filter(|b| !now.contains_key(*b))
                .cloned()
                .collect();
            for (b, h) in now {
                changed(b, Some(h), "poll", &mut seen)?;
            }
            for b in gone {
                changed(b, None, "poll", &mut seen)?;
            }
        },
        Source::GcsPubSub {
            subscription,
            token,
        } => {
            let prefix = gcs_prefix(&repo.store.url)?;
            let client = reqwest::Client::new();
            let base = format!(
                "https://pubsub.googleapis.com/v1/{}",
                subscription.trim_start_matches('/')
            );
            loop {
                let bearer = {
                    let t = token.clone();
                    tokio::task::spawn_blocking(move || t.get())
                        .await??
                        .token
                        .context("the credential has no token for Pub/Sub")?
                };
                let resp = client
                    .post(format!("{base}:pull"))
                    .bearer_auth(&bearer)
                    .header("content-type", "application/json")
                    .body(serde_json::json!({"maxMessages": 100}).to_string())
                    .send()
                    .await
                    .context("pulling the subscription")?;
                let status = resp.status();
                let body: serde_json::Value =
                    serde_json::from_slice(&resp.bytes().await?).unwrap_or_default();
                if !status.is_success() {
                    bail!("Pub/Sub pull failed ({status}): {body}");
                }
                let mut acks = vec![];
                let mut branches = vec![];
                for m in body["receivedMessages"].as_array().into_iter().flatten() {
                    acks.push(m["ackId"].as_str().unwrap_or_default().to_string());
                    let object = m["message"]["attributes"]["objectId"]
                        .as_str()
                        .unwrap_or_default();
                    if let Some(branch) = object
                        .strip_prefix(&prefix)
                        .and_then(|k| k.strip_prefix("refs/"))
                    {
                        if wanted(branch) && !branches.iter().any(|b: &String| b == branch) {
                            branches.push(branch.to_string());
                        }
                    }
                }
                // The notification says something changed; the ref says what.
                for b in branches {
                    let commit = repo.read_ref(&b).await?.map(|(h, _)| h);
                    changed(b, commit, "gcs", &mut seen)?;
                }
                if !acks.is_empty() {
                    let resp = client
                        .post(format!("{base}:acknowledge"))
                        .bearer_auth(&bearer)
                        .header("content-type", "application/json")
                        .body(serde_json::json!({"ackIds": acks}).to_string())
                        .send()
                        .await?;
                    if !resp.status().is_success() {
                        bail!("Pub/Sub acknowledge failed ({})", resp.status());
                    }
                }
            }
        }
    }
}

/// The object-name prefix of a gs:// store's keys ("" or "repo/").
fn gcs_prefix(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("gs://")
        .context("gcs events need a gs:// store")?;
    let prefix = rest
        .split_once('/')
        .map(|(_, p)| p.trim_matches('/'))
        .unwrap_or("");
    Ok(if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("pr/*", "pr/12"));
        assert!(glob_match("*", "main"));
        assert!(glob_match("main", "main"));
        assert!(!glob_match("pr/*", "main"));
        assert!(glob_match("release/*/stable", "release/2026/stable"));
        assert!(!glob_match("_meta*", "main"));
        assert_eq!(gcs_prefix("gs://b/team/app").unwrap(), "team/app/");
        assert_eq!(gcs_prefix("gs://b").unwrap(), "");
    }
}
