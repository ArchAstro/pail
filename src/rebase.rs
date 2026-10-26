//! Rewriting a branch's history. A plan lists what to do with each of the branch's own
//! commits, in Git's rebase-todo verbs; it runs in memory from each commit's change list (no
//! working copy, no stopped state) and is published as one rewrite ([`Repo::rewrite_ref`]).
//!
//! ```text
//! todo / plan file / defaults ─▶ Plan ─▶ build (prepare commits; a conflict changes nothing)
//!                                          ─▶ store commits ─▶ one CAS on the branch (epoch + 1)
//! ```
//!
//! Interactive and scripted rebases are the same plan: `-i` edits it as todo text, `--plan`
//! reads it as JSON, `--print-plan` writes the default one.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::hash::Hash;
use crate::repo::{Delta, PreparedCommit, Repo, rewritten_fields};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Keep the commit.
    Pick,
    /// Keep it with a new message.
    Reword,
    /// Meld into the previous commit, appending this commit's message.
    Squash,
    /// Meld into the previous commit, dropping this commit's message.
    Fixup,
    /// Remove it.
    Drop,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Pick => "pick",
            Action::Reword => "reword",
            Action::Squash => "squash",
            Action::Fixup => "fixup",
            Action::Drop => "drop",
        }
    }

    pub fn parse(s: &str) -> Option<Action> {
        Some(match s {
            "p" | "pick" => Action::Pick,
            "r" | "reword" => Action::Reword,
            "s" | "squash" => Action::Squash,
            "f" | "fixup" => Action::Fixup,
            "d" | "drop" => Action::Drop,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub action: Action,
    pub commit: Hash,
    /// The new message (`reword`), or the message to append (`squash`; default: the
    /// commit's own). A `pick` with a message is a reword.
    pub message: Option<String>,
}

/// A rewrite of `branch`: the commits after `base` up to `expected` become `steps`, on
/// `onto`.
#[derive(Clone, Debug)]
pub struct Plan {
    pub branch: String,
    /// The tip the plan was made from.
    pub expected: Hash,
    /// Where the branch's own commits start (exclusive).
    pub base: Hash,
    pub onto: Hash,
    pub steps: Vec<Step>,
}

/// A plan as JSON: commits as revisions the caller resolves (full ids when pail writes it).
///
/// ```json
/// {"branch": "feature", "expect": "<tip>", "base": "<commit>", "onto": "<commit>",
///  "steps": [{"pick": "a1"}, {"reword": "b2", "message": "New subject"},
///            {"squash": "c3"}, {"fixup": "d4"}, {"drop": "e5"}]}
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlanFile {
    pub branch: String,
    pub expect: String,
    pub base: String,
    pub onto: String,
    pub steps: Vec<StepFile>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reword: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl StepFile {
    /// The step's action and commit revision: exactly one verb must be set.
    pub fn verb(&self) -> Result<(Action, &str)> {
        let verbs = [
            (Action::Pick, &self.pick),
            (Action::Reword, &self.reword),
            (Action::Squash, &self.squash),
            (Action::Fixup, &self.fixup),
            (Action::Drop, &self.drop),
        ];
        let set: Vec<(Action, &str)> = verbs
            .iter()
            .filter_map(|(a, v)| v.as_deref().map(|v| (*a, v)))
            .collect();
        match set.as_slice() {
            [one] => Ok(*one),
            _ => bail!("each step names exactly one of pick, reword, squash, fixup, drop"),
        }
    }
}

impl Plan {
    pub fn to_file(&self) -> PlanFile {
        PlanFile {
            branch: self.branch.clone(),
            expect: self.expected.hex(),
            base: self.base.hex(),
            onto: self.onto.hex(),
            steps: self
                .steps
                .iter()
                .map(|s| {
                    let id = Some(s.commit.hex());
                    let mut f = StepFile {
                        message: s.message.clone(),
                        ..Default::default()
                    };
                    match s.action {
                        Action::Pick => f.pick = id,
                        Action::Reword => f.reword = id,
                        Action::Squash => f.squash = id,
                        Action::Fixup => f.fixup = id,
                        Action::Drop => f.drop = id,
                    }
                    f
                })
                .collect(),
        }
    }
}

/// The branch's own commits after `base` up to `tip`, oldest first.
pub async fn range(repo: &Repo, base: Hash, tip: Hash) -> Result<Vec<Hash>> {
    let mut out = vec![];
    let mut cur = Some(tip);
    while let Some(h) = cur.filter(|h| *h != base) {
        out.push(h);
        cur = repo.commit_meta(&h).await?.parent;
    }
    anyhow::ensure!(
        cur == Some(base),
        "{} is not an ancestor of {}",
        base.short(),
        tip.short()
    );
    out.reverse();
    Ok(out)
}

/// The plan that keeps every commit: `pick` each, oldest first.
pub async fn default_plan(
    repo: &Repo,
    branch: &str,
    expected: Hash,
    base: Hash,
    onto: Hash,
) -> Result<Plan> {
    let steps = range(repo, base, expected)
        .await?
        .into_iter()
        .map(|commit| Step {
            action: Action::Pick,
            commit,
            message: None,
        })
        .collect();
    Ok(Plan {
        branch: branch.to_string(),
        expected,
        base,
        onto,
        steps,
    })
}

/// Git's `--autosquash`: a commit whose subject is `fixup! <s>` or `squash! <s>` moves right
/// after the first earlier commit whose subject is (or starts with) `<s>`, or whose id does,
/// and melds into it.
pub async fn autosquash(repo: &Repo, plan: &mut Plan) -> Result<()> {
    let mut subjects = HashMap::new();
    for s in &plan.steps {
        let c = repo.commit_meta(&s.commit).await?;
        subjects.insert(s.commit, c.subject().to_string());
    }
    let mut out: Vec<Step> = vec![];
    let mut moved = HashSet::new();
    for (i, s) in plan.steps.iter().enumerate() {
        if moved.contains(&i) {
            continue;
        }
        out.push(s.clone());
        // Later fixups of this one, in order.
        let target = &subjects[&s.commit];
        for (j, f) in plan.steps.iter().enumerate().skip(i + 1) {
            if moved.contains(&j) {
                continue;
            }
            let Some((action, wanted)) = melds(&subjects[&f.commit]) else {
                continue;
            };
            if target == wanted
                || target.starts_with(wanted)
                || (wanted.len() >= 4 && s.commit.hex().starts_with(wanted))
            {
                moved.insert(j);
                out.push(Step {
                    action,
                    commit: f.commit,
                    message: None,
                });
            }
        }
    }
    plan.steps = out;
    Ok(())
}

fn melds(subject: &str) -> Option<(Action, &str)> {
    let (action, mut rest) = if let Some(r) = subject.strip_prefix("fixup! ") {
        (Action::Fixup, r)
    } else if let Some(r) = subject.strip_prefix("squash! ") {
        (Action::Squash, r)
    } else {
        return None;
    };
    // `fixup! fixup! x` targets x.
    while let Some(r) = rest
        .strip_prefix("fixup! ")
        .or_else(|| rest.strip_prefix("squash! "))
    {
        rest = r;
    }
    Some((action, rest))
}

/// The todo text `-i` opens: one line per step, Git's verbs.
pub async fn todo_text(repo: &Repo, plan: &Plan) -> Result<String> {
    let mut s = String::new();
    for st in &plan.steps {
        let c = repo.commit_meta(&st.commit).await?;
        s.push_str(&format!(
            "{} {} {}\n",
            st.action.name(),
            &st.commit.hex()[..12],
            c.subject()
        ));
    }
    s.push_str(&format!(
        "\n# Rebase {}..{} onto {} ({} commit(s))\n{TODO_HELP}",
        plan.base.short(),
        plan.expected.short(),
        plan.onto.short(),
        plan.steps.len()
    ));
    Ok(s)
}

const TODO_HELP: &str = "\
#
# Commands:
# p, pick <commit> = use commit
# r, reword <commit> [<subject>] = use commit with a new message (this line's text as its
#                    subject; unchanged text opens the editor on the whole message)
# s, squash <commit> = meld into previous commit, combining both messages
# f, fixup <commit> = meld into previous commit, discarding this commit's message
# d, drop <commit> = remove commit
#
# Lines can be reordered. Removing a line drops that commit; removing everything aborts.
# Nothing is published until the whole plan applies: a conflict changes nothing.
";

/// A todo line's parts: action, commit revision, and the rest of the line.
pub struct TodoLine<'a> {
    pub action: Action,
    pub rev: &'a str,
    pub text: &'a str,
}

pub fn parse_todo(text: &str) -> Result<Vec<TodoLine<'_>>> {
    let mut out = vec![];
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, char::is_whitespace);
        let verb = parts.next().unwrap_or("");
        let action = Action::parse(verb).with_context(|| {
            format!(
                "todo line {}: unknown command {verb:?} (pick, reword, squash, fixup, drop)",
                n + 1
            )
        })?;
        let rev = parts
            .next()
            .with_context(|| format!("todo line {}: {verb} needs a commit", n + 1))?;
        out.push(TodoLine {
            action,
            rev,
            text: parts.next().unwrap_or("").trim(),
        });
    }
    Ok(out)
}

/// A built rewrite: the new commits, prepared but not stored.
pub struct Built {
    /// The new tip (== `onto` when every commit was dropped).
    pub tip: Hash,
    /// Each rewritten or dropped commit of the old history to its successor (None: dropped).
    pub map: BTreeMap<Hash, Option<Hash>>,
    /// Commits whose changes `onto` already had (dropped as empty).
    pub empty: Vec<Hash>,
    prepared: Vec<PreparedCommit>,
}

impl Built {
    /// How many commits the rewrite creates.
    pub fn created(&self) -> usize {
        self.prepared.len()
    }
}

/// Carry out `plan` in memory: each commit's change list applied on the new history, which
/// must find every path it touches as the commit's parent had it (or already as the commit
/// leaves it). A commit that still sits on its original parent is reused, not copied.
pub async fn build(repo: &Repo, plan: &Plan, committer: &str, date: &str) -> Result<Built> {
    let range = range(repo, plan.base, plan.expected).await?;
    let in_range: HashSet<Hash> = range.iter().copied().collect();
    let mut seen = HashSet::new();
    for s in &plan.steps {
        if !in_range.contains(&s.commit) {
            bail!(
                "{} is not one of {}'s commits after {}",
                s.commit.short(),
                plan.branch,
                plan.base.short()
            );
        }
        if !seen.insert(s.commit) {
            bail!("{} appears twice in the plan", s.commit.short());
        }
    }
    // Groups: a kept commit and the commits melded into it.
    let mut groups: Vec<Vec<&Step>> = vec![];
    for s in &plan.steps {
        match s.action {
            Action::Drop => {}
            Action::Squash | Action::Fixup => match groups.last_mut() {
                Some(g) => g.push(s),
                None => bail!(
                    "cannot {} {}: there is no previous commit to meld into",
                    s.action.name(),
                    s.commit.short()
                ),
            },
            Action::Pick | Action::Reword => groups.push(vec![s]),
        }
    }
    let mut map: BTreeMap<Hash, Option<Hash>> = range
        .iter()
        .filter(|h| {
            !plan
                .steps
                .iter()
                .any(|s| s.commit == **h && s.action != Action::Drop)
        })
        .map(|h| (*h, None))
        .collect();
    let mut parent = plan.onto;
    let mut prepared = vec![];
    let mut empty = vec![];
    for g in groups {
        let lead = g[0];
        let lc = repo.commit_meta(&lead.commit).await?;
        let rewords = lead.message.is_some();
        if g.len() == 1 && !rewords && lc.parent == Some(parent) {
            parent = lead.commit;
            continue;
        }
        let target = repo.root_of(Some(parent)).await?;
        let mut overlay = Delta::new();
        let mut message = lead.message.clone().unwrap_or_else(|| lc.message.clone());
        for s in &g {
            let c = repo.commit_meta(&s.commit).await?;
            let delta = repo.delta_of(&s.commit).await?;
            repo.check_applies(
                &plan.branch,
                repo.root_of(c.parent).await?,
                &delta,
                target,
                &overlay,
            )
            .await
            .with_context(|| format!("applying {} {}", s.commit.short(), c.subject()))?;
            overlay.extend(delta);
            if s.action == Action::Squash {
                let m = s.message.clone().unwrap_or_else(|| c.message.clone());
                message = format!("{}\n\n{}\n", message.trim_end(), m.trim_end());
            }
        }
        let sources: Vec<Hash> = g.iter().map(|s| s.commit).collect();
        let mut fields = rewritten_fields(&lc, &sources, committer, date);
        fields.message = message;
        let p = repo.prepare_commit(Some(parent), &overlay, &fields).await?;
        if repo.root_of(Some(p.commit)).await? == target {
            // Everything it changes is already there.
            for h in sources {
                map.insert(h, None);
                empty.push(h);
            }
            continue;
        }
        parent = p.commit;
        for h in sources {
            map.insert(h, Some(p.commit));
        }
        prepared.push(p);
    }
    Ok(Built {
        tip: parent,
        map,
        empty,
        prepared,
    })
}

/// Store a built rewrite's commits (nothing references them until the branch moves).
pub async fn store(repo: &Repo, built: &Built) -> Result<()> {
    futures::future::try_join_all(built.prepared.iter().map(|p| repo.store_commit(p, vec![])))
        .await?;
    Ok(())
}
