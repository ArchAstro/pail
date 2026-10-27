//! `Policy::parse`, `Policy::decide` and `policy::request_op` checked against the Quint model
//! of branch policy (`verify/policy.qnt`, scenarios in `verify/policy_scenarios.qnt`).
//!
//! `load` renders the model's document as JSON and parses it; `decide` and `requestOp` ask the
//! real functions. After each, the parsed policy (or that parsing failed) and the answer must
//! equal the model's. The gateway's Go port runs the same scenarios through its copy of the
//! generated Go adapter.

#[path = "policy_refinement/generated.rs"]
mod generated;

use std::collections::BTreeMap;

use pail::policy::{self, Identity, Op, Policy};
use quint_refinements::{NormalizedRuntimeEvidence, RuntimeValue};
use serde_json::{Value, json};

#[test]
fn policy_refines_the_policy_model() {
    let results = generated::refine_all::<PolicyUnderTest>().unwrap_or_else(|e| panic!("{e}"));
    for r in &results {
        eprintln!(
            "{} refined {} obligations",
            r.scenario, r.evaluated_obligations
        );
    }
    assert_eq!(
        results.len(),
        16,
        "every scenario in verify/policy_scenarios.qnt ran"
    );
}

enum Loaded {
    None,
    Invalid,
    Valid(Policy),
}

enum Answer {
    None,
    Decided(bool),
    Mapped(Op),
}

struct PolicyUnderTest {
    policy: Loaded,
    answer: Answer,
}

// ---------- model values to real ones ----------

fn variant(v: &RuntimeValue) -> Result<(&str, &RuntimeValue), String> {
    match v {
        RuntimeValue::Variant { tag, value } => Ok((tag, value)),
        // A unit constructor.
        RuntimeValue::Text(tag) => Ok((tag, v)),
        other => Err(format!("expected a variant, got {other:?}")),
    }
}

fn text(v: &RuntimeValue) -> Result<String, String> {
    match v {
        RuntimeValue::Text(t) => Ok(t.clone()),
        other => Err(format!("expected text, got {other:?}")),
    }
}

/// A model `Text`: a string as its characters.
fn chars(v: &RuntimeValue) -> Result<String, String> {
    match v {
        RuntimeValue::List(cs) => cs.iter().map(text).collect(),
        other => Err(format!("expected a list of characters, got {other:?}")),
    }
}

fn texts(v: &RuntimeValue) -> Result<Vec<Value>, String> {
    match v {
        RuntimeValue::List(l) => l.iter().map(|t| text(t).map(Value::from)).collect(),
        other => Err(format!("expected a list, got {other:?}")),
    }
}

fn entries(v: &RuntimeValue) -> Result<&BTreeMap<RuntimeValue, RuntimeValue>, String> {
    match v {
        RuntimeValue::Map(m) => Ok(m),
        other => Err(format!("expected a map, got {other:?}")),
    }
}

/// A field's JSON value (`Field` and `RuleField` share their shapes).
fn field_json(v: &RuntimeValue) -> Result<Value, String> {
    let (tag, value) = variant(v)?;
    Ok(match tag {
        "FNull" | "RNull" => Value::Null,
        "FInt" | "RInt" => match value {
            RuntimeValue::Int(i) => json!(i),
            other => return Err(format!("expected an integer, got {other:?}")),
        },
        "FText" | "RText" => Value::from(text(value)?),
        "FTexts" | "RTexts" => Value::Array(texts(value)?),
        "FTextsThenNull" | "RTextsThenNull" => {
            let mut l = texts(value)?;
            l.push(Value::Null);
            Value::Array(l)
        }
        "FRules" => Value::Object(
            entries(value)?
                .iter()
                .map(|(glob, rule)| Ok((chars(glob)?, rule_json(rule)?)))
                .collect::<Result<_, String>>()?,
        ),
        other => return Err(format!("unknown field {other}")),
    })
}

fn rule_json(v: &RuntimeValue) -> Result<Value, String> {
    let (tag, value) = variant(v)?;
    match tag {
        "RuleNull" => Ok(Value::Null),
        "RuleObject" => object_json(value),
        other => Err(format!("unknown rule {other}")),
    }
}

fn object_json(v: &RuntimeValue) -> Result<Value, String> {
    Ok(Value::Object(
        entries(v)?
            .iter()
            .map(|(name, f)| Ok((text(name)?, field_json(f)?)))
            .collect::<Result<_, String>>()?,
    ))
}

fn caller(v: &RuntimeValue) -> Result<Identity, String> {
    let RuntimeValue::Record(r) = v else {
        return Err(format!("expected a caller, got {v:?}"));
    };
    let principal = text(r.get("principal").ok_or("caller has no principal")?)?;
    let groups = match r.get("groups") {
        Some(RuntimeValue::Set(g)) => g.iter().map(text).collect::<Result<Vec<_>, _>>()?,
        other => return Err(format!("expected groups, got {other:?}")),
    };
    Ok(Identity::new(&principal, &groups))
}

fn op(v: &RuntimeValue) -> Result<Op, String> {
    match variant(v)?.0 {
        "Read" => Ok(Op::Read),
        "Write" => Ok(Op::Write),
        "Delete" => Ok(Op::Delete),
        other => Err(format!("unknown op {other}")),
    }
}

// ---------- real values to model ones ----------

fn tagged(tag: &str, value: RuntimeValue) -> RuntimeValue {
    RuntimeValue::Variant {
        tag: tag.to_owned(),
        value: Box::new(value),
    }
}

/// A unit constructor, which the model's values carry as its name.
fn unit(tag: &str) -> RuntimeValue {
    RuntimeValue::Text(tag.to_owned())
}

fn record<const N: usize>(fields: [(&str, RuntimeValue); N]) -> RuntimeValue {
    RuntimeValue::Record(fields.map(|(k, v)| (k.to_owned(), v)).into_iter().collect())
}

fn names(l: &[String]) -> RuntimeValue {
    RuntimeValue::List(l.iter().cloned().map(RuntimeValue::Text).collect())
}

fn to_chars(s: &str) -> RuntimeValue {
    RuntimeValue::List(
        s.chars()
            .map(|c| RuntimeValue::Text(c.to_string()))
            .collect(),
    )
}

fn op_value(o: Op) -> RuntimeValue {
    unit(match o {
        Op::Read => "Read",
        Op::Write => "Write",
        Op::Delete => "Delete",
    })
}

fn policy_value(p: &Policy) -> RuntimeValue {
    let branches = p
        .branches
        .iter()
        .map(|(glob, r)| {
            let approvals = match r.approvals {
                Some(n) => tagged("NeedsApprovals", RuntimeValue::Int(n.into())),
                None => unit("NoApprovals"),
            };
            let rule = record([
                ("writers", names(&r.writers)),
                ("approvals", approvals),
                ("requiredChecks", names(&r.required_checks)),
            ]);
            (to_chars(glob), rule)
        })
        .collect();
    record([
        ("readers", names(&p.readers)),
        ("admins", names(&p.admins)),
        ("gc", names(&p.gc)),
        ("branches", RuntimeValue::Map(branches)),
    ])
}

impl PolicyUnderTest {
    fn state(&self) -> RuntimeValue {
        let policy = match &self.policy {
            Loaded::None => unit("NoPolicy"),
            Loaded::Invalid => unit("Invalid"),
            Loaded::Valid(p) => tagged("Valid", policy_value(p)),
        };
        let answer = match self.answer {
            Answer::None => unit("NoAnswer"),
            Answer::Decided(true) => unit("Allowed"),
            Answer::Decided(false) => unit("Denied"),
            Answer::Mapped(o) => tagged("Mapped", op_value(o)),
        };
        record([("policy", policy), ("answer", answer)])
    }
}

#[derive(Clone)]
struct Snapshot(RuntimeValue);

impl NormalizedRuntimeEvidence for Snapshot {
    fn resolve_name(&self, name: &str) -> Result<RuntimeValue, String> {
        match name {
            "state" => Ok(self.0.clone()),
            other => Err(format!("unknown model name {other}")),
        }
    }

    fn resolve_call(
        &self,
        _operator: &str,
        _arguments: &[RuntimeValue],
    ) -> Option<Result<RuntimeValue, String>> {
        None
    }
}

impl generated::Implementation for PolicyUnderTest {
    type Evidence = Snapshot;

    /// No policy read yet (the model starts with none).
    fn from_initial_state(_initial: &RuntimeValue) -> Result<Self, String> {
        Ok(PolicyUnderTest {
            policy: Loaded::None,
            answer: Answer::None,
        })
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot(self.state())
    }

    fn load(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let doc = object_json(arguments.first().ok_or("load takes a document")?)?;
        self.policy = match Policy::parse(doc.to_string().as_bytes()) {
            Ok(p) => Loaded::Valid(p),
            Err(_) => Loaded::Invalid,
        };
        self.answer = Answer::None;
        Ok(())
    }

    fn decide(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [c, o, key] = arguments else {
            return Err(format!("decide takes three arguments, got {arguments:?}"));
        };
        let Loaded::Valid(p) = &self.policy else {
            return Err("decide without a valid policy".into());
        };
        self.answer = Answer::Decided(p.decide(&caller(c)?, op(o)?, &chars(key)?).allow);
        Ok(())
    }

    fn request_op(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [method, key, inm] = arguments else {
            return Err(format!(
                "requestOp takes three arguments, got {arguments:?}"
            ));
        };
        let inm = text(inm)?;
        let inm = (!inm.is_empty()).then_some(inm.as_str());
        self.answer = Answer::Mapped(policy::request_op(&text(method)?, &chars(key)?, inm));
        Ok(())
    }
}
