//! `pail obj`: single objects in the repository's sibling area (`<repo>.<area>/`), with the
//! store's own preconditions. See docs/actions.md §5.

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::store::Version;

/// Exit code: the key is absent (`get`).
pub const ABSENT_EXIT: i32 = 3;
/// Exit code: a precondition failed (same as `pail put --expect`).
pub const PRECONDITION_EXIT: i32 = 4;

/// The version token callers pass back: base64url (unpadded) of the version's JSON.
pub fn encode_version(v: &Version) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).expect("version serializes"))
}

pub fn decode_version(token: &str) -> Result<Version> {
    let raw = URL_SAFE_NO_PAD
        .decode(token.trim())
        .context("not a version token (expected the output of `pail obj ls|get|put`)")?;
    serde_json::from_slice(&raw).context("not a version token")
}

/// A key relative to the area: non-empty, no leading `/`, no `.` or `..` segments.
pub fn check_key(key: &str) -> Result<()> {
    if key.is_empty() || key.starts_with('/') || key.ends_with('/') {
        bail!("invalid key `{key}`");
    }
    if key
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        bail!("invalid key `{key}`");
    }
    Ok(())
}

pub fn check_area(area: &str) -> Result<()> {
    if area.is_empty() || area.starts_with('.') || area.contains("..") || area.contains('/') {
        bail!("invalid area name `{area}`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trips() {
        let v = Version {
            etag: Some("\"abc\"".into()),
            version: Some("1730".into()),
        };
        let t = encode_version(&v);
        assert!(!t.contains(['=', '+', '/', '\n']));
        assert_eq!(decode_version(&t).unwrap(), v);
        assert!(decode_version("!!").is_err());
    }

    #[test]
    fn keys() {
        assert!(check_key("queue/x/1.json").is_ok());
        for bad in ["", "/a", "a/", "a//b", "a/../b", "./a"] {
            assert!(check_key(bad).is_err(), "{bad}");
        }
    }
}
