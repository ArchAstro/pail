//! Runs tests/policy-vectors.json against `Policy::decide`. The same file is copied into other
//! enforcement points (the ArchDev gateway's Go port) so they cannot drift from this one.

use pail::policy::{Identity, Op, Policy};
use serde_json::Value;

fn vectors() -> Value {
    serde_json::from_str(include_str!("policy-vectors.json")).expect("vectors parse")
}

#[test]
fn decide_matches_vectors() {
    let v = vectors();
    let policies: std::collections::BTreeMap<String, Policy> = v["policies"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, p)| {
            (
                name.clone(),
                Policy::parse(p.to_string().as_bytes()).unwrap(),
            )
        })
        .collect();
    let cases = v["cases"].as_array().unwrap();
    assert!(cases.len() > 50);
    for c in cases {
        let policy = &policies[c["policy"].as_str().unwrap()];
        let groups: Vec<String> = c["identity"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g.as_str().unwrap().to_string())
            .collect();
        let id = Identity::new(c["identity"]["principal"].as_str().unwrap(), &groups);
        let op = match c["op"].as_str().unwrap() {
            "read" => Op::Read,
            "write" => Op::Write,
            "delete" => Op::Delete,
            o => panic!("unknown op {o}"),
        };
        let key = c["key"].as_str().unwrap();
        let d = policy.decide(&id, op, key);
        assert_eq!(
            d.allow,
            c["allow"].as_bool().unwrap(),
            "{c} -> {} ({})",
            d.allow,
            d.reason
        );
    }
}

#[test]
fn invalid_policies_are_rejected() {
    for p in vectors()["invalid_policies"].as_array().unwrap() {
        assert!(Policy::parse(p.to_string().as_bytes()).is_err(), "{p}");
    }
}
