//! `pail events setup`: the provider CLI commands that wire a store's notifications to a
//! per-consumer queue or subscription, for the prefixes `pail events` watches:
//! `<repo>/refs/` and `<repo>.actions/`.
//!
//! The plan is data ([`plan`]): printing it is the default, and [`apply`] runs it only
//! after a confirmation (or `--yes`) and never with a placeholder left in a command.
//! Everything it creates is billable cloud resources, which is why printing is the default.

use anyhow::{Context, Result, bail};

use crate::event_sources::{SourceSpec, store_name};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Gcs,
    S3,
    Azure,
    R2,
}

impl Provider {
    pub fn parse(s: &str) -> Result<Provider> {
        Ok(match s {
            "gcs" => Provider::Gcs,
            "s3" => Provider::S3,
            "azure" => Provider::Azure,
            "r2" => Provider::R2,
            _ => bail!("unknown provider `{s}` (gcs, s3, azure, r2)"),
        })
    }
}

/// What the user told `setup`; everything left out is a placeholder (UPPER CASE) in the
/// printed commands.
#[derive(Debug, Default, Clone)]
pub struct Opts {
    pub consumer: String,
    pub provider: Option<Provider>,
    /// AWS region, Azure location.
    pub region: Option<String>,
    /// GCP project, Azure subscription id.
    pub project: Option<String>,
    /// AWS account id, Azure storage account, Cloudflare account id.
    pub account: Option<String>,
    pub resource_group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub title: String,
    pub argv: Vec<String>,
}

#[derive(Debug)]
pub struct Plan {
    pub provider: Provider,
    pub steps: Vec<Step>,
    /// The source `pail events` then uses (placeholders possible).
    pub source: SourceSpec,
    /// Things to know that are not commands.
    pub notes: Vec<String>,
    /// Placeholders that appear in `steps`: while any is left, the plan cannot be applied.
    pub unresolved: Vec<String>,
}

/// The provider for `store_url`: `gs://` is GCS, an `s3://` store behind an R2 endpoint
/// (`AWS_ENDPOINT`) is R2, any other `s3://` is S3, an Azure blob URL is Azure.
pub fn detect(store_url: &str, endpoint: Option<&str>) -> Option<Provider> {
    match store_url.split_once("://")?.0 {
        "gs" => Some(Provider::Gcs),
        "s3" if endpoint.is_some_and(|e| e.contains(".r2.cloudflarestorage.com")) => {
            Some(Provider::R2)
        }
        "s3" => Some(Provider::S3),
        "https" if store_url.contains(".blob.core.windows.net") => Some(Provider::Azure),
        _ => None,
    }
}

/// A word safe for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

pub fn render(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

struct Ph {
    unresolved: Vec<String>,
}

impl Ph {
    /// The value, or an UPPER CASE placeholder recorded as unresolved (`in_steps`: it ends
    /// up in a command, not only in the `pail events` flag).
    fn get(&mut self, v: &Option<String>, name: &str, in_steps: bool) -> String {
        match v {
            Some(v) => v.clone(),
            None => {
                if in_steps && !self.unresolved.iter().any(|u| u == name) {
                    self.unresolved.push(name.to_string());
                }
                name.to_string()
            }
        }
    }
}

fn s(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

pub fn plan(store_url: &str, endpoint: Option<&str>, o: &Opts) -> Result<Plan> {
    if o.consumer.is_empty()
        || o.consumer.len() > 40
        || !o
            .consumer
            .starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        || !o
            .consumer
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("--consumer is a short lowercase name of letters, digits and dashes (e.g. ci)");
    }
    let provider = o
        .provider
        .or_else(|| detect(store_url, endpoint))
        .with_context(|| {
            format!("cannot tell which provider hosts {store_url}; pass --provider gcs|s3|azure|r2")
        })?;
    let me = store_name(store_url)?;
    // the prefixes `pail events` watches: refs, and the sibling area when the store has a path
    let mut prefixes = vec![("refs", format!("{}refs/", me.prefix))];
    let mut notes = vec![];
    if let Some(repo) = me.prefix.strip_suffix('/') {
        prefixes.push(("actions", format!("{repo}.actions/")));
    } else {
        notes.push("the store has no path, so there is no <repo>.actions/ area to watch".into());
    }
    let name = format!("pail-{}", o.consumer);
    let mut ph = Ph { unresolved: vec![] };
    let (steps, source) = match provider {
        Provider::Gcs => gcs(&me.bucket, &prefixes, &name, o, &mut ph),
        Provider::S3 => s3(&me.bucket, &prefixes, &name, o, &mut ph, &mut notes),
        Provider::Azure => azure(&me.bucket, &prefixes, &name, o, &mut ph, &mut notes),
        Provider::R2 => r2(
            &me.bucket, &prefixes, &name, o, endpoint, &mut ph, &mut notes,
        ),
    };
    Ok(Plan {
        provider,
        steps,
        source,
        notes,
        unresolved: ph.unresolved,
    })
}

fn gcs(
    bucket: &Option<String>,
    prefixes: &[(&str, String)],
    name: &str,
    o: &Opts,
    ph: &mut Ph,
) -> (Vec<Step>, SourceSpec) {
    let bucket = bucket.clone().unwrap_or_default();
    let project = o.project.as_ref().map(|p| format!("--project={p}"));
    let with_project = |mut v: Vec<String>| {
        v.extend(project.clone());
        v
    };
    let mut steps = vec![Step {
        title: "a topic for the bucket's notifications".into(),
        argv: with_project(s(&["gcloud", "pubsub", "topics", "create", name])),
    }];
    for (what, prefix) in prefixes {
        steps.push(Step {
            title: format!("notify the topic of changes under {prefix} ({what})"),
            argv: with_project(vec![
                "gcloud".into(),
                "storage".into(),
                "buckets".into(),
                "notifications".into(),
                "create".into(),
                format!("gs://{bucket}"),
                format!("--topic={name}"),
                format!("--object-prefix={prefix}"),
                "--event-types=OBJECT_FINALIZE,OBJECT_DELETE".into(),
            ]),
        });
    }
    steps.push(Step {
        title: "the consumer's own subscription".into(),
        argv: with_project(vec![
            "gcloud".into(),
            "pubsub".into(),
            "subscriptions".into(),
            "create".into(),
            name.into(),
            format!("--topic={name}"),
        ]),
    });
    let project = ph.get(&o.project, "PROJECT", false);
    (
        steps,
        SourceSpec::Gcs(format!("projects/{project}/subscriptions/{name}")),
    )
}

fn s3(
    bucket: &Option<String>,
    prefixes: &[(&str, String)],
    name: &str,
    o: &Opts,
    ph: &mut Ph,
    notes: &mut Vec<String>,
) -> (Vec<Step>, SourceSpec) {
    let bucket = bucket.clone().unwrap_or_default();
    let region = ph.get(&o.region, "REGION", true);
    let account = ph.get(&o.account, "ACCOUNT_ID", true);
    let arn = format!("arn:aws:sqs:{region}:{account}:{name}");
    let url = format!("https://sqs.{region}.amazonaws.com/{account}/{name}");
    let policy = serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [{
            "Effect": "Allow",
            "Principal": {"Service": "s3.amazonaws.com"},
            "Action": "sqs:SendMessage",
            "Resource": arn,
            "Condition": {
                "ArnLike": {"aws:SourceArn": format!("arn:aws:s3:::{bucket}")},
                "StringEquals": {"aws:SourceAccount": account},
            },
        }],
    });
    let configs: Vec<_> = prefixes
        .iter()
        .map(|(what, prefix)| {
            serde_json::json!({
                "Id": format!("{name}-{what}"),
                "QueueArn": arn,
                "Events": ["s3:ObjectCreated:*", "s3:ObjectRemoved:*"],
                "Filter": {"Key": {"FilterRules": [{"Name": "prefix", "Value": prefix}]}},
            })
        })
        .collect();
    let steps = vec![
        Step {
            title: "the consumer's queue".into(),
            argv: s(&[
                "aws",
                "sqs",
                "create-queue",
                "--queue-name",
                name,
                "--region",
                &region,
            ]),
        },
        Step {
            title: "let S3 send to it".into(),
            argv: vec![
                "aws".into(),
                "sqs".into(),
                "set-queue-attributes".into(),
                "--region".into(),
                region.clone(),
                "--queue-url".into(),
                url.clone(),
                "--attributes".into(),
                serde_json::json!({"Policy": policy.to_string()}).to_string(),
            ],
        },
        Step {
            title: format!(
                "notify the queue of changes under {} (this REPLACES the bucket's whole notification configuration)",
                prefixes
                    .iter()
                    .map(|(_, p)| p.as_str())
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
            argv: vec![
                "aws".into(),
                "s3api".into(),
                "put-bucket-notification-configuration".into(),
                "--bucket".into(),
                bucket,
                "--notification-configuration".into(),
                serde_json::json!({"QueueConfigurations": configs}).to_string(),
            ],
        },
    ];
    notes.push("put-bucket-notification-configuration replaces the bucket's existing notification configuration: merge in what `aws s3api get-bucket-notification-configuration` shows first if the bucket already has some".into());
    notes.push(format!(
        "the consumer's identity needs sqs:ReceiveMessage, sqs:DeleteMessage and sqs:ChangeMessageVisibility on {arn}"
    ));
    (steps, SourceSpec::Sqs(url))
}

fn azure(
    container: &Option<String>,
    prefixes: &[(&str, String)],
    name: &str,
    o: &Opts,
    ph: &mut Ph,
    notes: &mut Vec<String>,
) -> (Vec<Step>, SourceSpec) {
    let container = ph.get(container, "CONTAINER", true);
    let account = ph.get(&o.account, "STORAGE_ACCOUNT", true);
    let rg = ph.get(&o.resource_group, "RESOURCE_GROUP", true);
    let sub = ph.get(&o.project, "SUBSCRIPTION_ID", true);
    let location = ph.get(&o.region, "LOCATION", true);
    let account_id = format!(
        "/subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Storage/storageAccounts/{account}"
    );
    let mut steps = vec![
        Step {
            title: "the consumer's queue".into(),
            argv: s(&[
                "az",
                "storage",
                "queue",
                "create",
                "--name",
                name,
                "--account-name",
                &account,
                "--auth-mode",
                "login",
            ]),
        },
        Step {
            title: "an Event Grid system topic on the storage account".into(),
            argv: s(&[
                "az",
                "eventgrid",
                "system-topic",
                "create",
                "--name",
                name,
                "--resource-group",
                &rg,
                "--source",
                &account_id,
                "--topic-type",
                "microsoft.storage.storageaccounts",
                "--location",
                &location,
                "--subscription",
                &sub,
            ]),
        },
    ];
    for (what, prefix) in prefixes {
        let mut argv = s(&[
            "az",
            "eventgrid",
            "system-topic",
            "event-subscription",
            "create",
            "--name",
            &format!("{name}-{what}"),
            "--system-topic-name",
            name,
            "--resource-group",
            &rg,
            "--subscription",
            &sub,
            "--endpoint-type",
            "storagequeue",
            "--endpoint",
            &format!("{account_id}/queueServices/default/queues/{name}"),
            "--included-event-types",
            "Microsoft.Storage.BlobCreated",
            "Microsoft.Storage.BlobDeleted",
            "--subject-begins-with",
        ]);
        argv.push(format!(
            "/blobServices/default/containers/{container}/blobs/{prefix}"
        ));
        steps.push(Step {
            title: format!("deliver changes under {prefix} ({what}) to the queue"),
            argv,
        });
    }
    notes.push("a storage account has one system topic: if it already has one, skip the system-topic step and use its name for --system-topic-name".into());
    notes.push(format!(
        "authorize the consumer with a SAS token in --azure-queue's URL (`az storage queue generate-sas --name {name} --account-name {account} --permissions rp --expiry <date> --auth-mode login --as-user`), or with a credential provider that returns an Entra token for https://storage.azure.com/ (role: Storage Queue Data Message Processor)"
    ));
    (
        steps,
        SourceSpec::Azure(format!("https://{account}.queue.core.windows.net/{name}")),
    )
}

fn r2(
    bucket: &Option<String>,
    prefixes: &[(&str, String)],
    name: &str,
    o: &Opts,
    endpoint: Option<&str>,
    ph: &mut Ph,
    notes: &mut Vec<String>,
) -> (Vec<Step>, SourceSpec) {
    let bucket = ph.get(bucket, "BUCKET", true);
    // https://<account>.r2.cloudflarestorage.com
    let from_endpoint = endpoint
        .and_then(|e| e.split_once("://").map(|(_, h)| h))
        .and_then(|h| h.split_once(".r2.cloudflarestorage.com"))
        .map(|(a, _)| a.to_string());
    let account = ph.get(&o.account.clone().or(from_endpoint), "ACCOUNT_ID", false);
    let mut steps = vec![
        Step {
            title: "the consumer's queue".into(),
            argv: s(&["wrangler", "queues", "create", name]),
        },
        Step {
            title: "let it be pulled over HTTP".into(),
            argv: s(&["wrangler", "queues", "consumer", "http", "add", name]),
        },
    ];
    for (what, prefix) in prefixes {
        steps.push(Step {
            title: format!("notify the queue of changes under {prefix} ({what})"),
            argv: s(&[
                "wrangler",
                "r2",
                "bucket",
                "notification",
                "create",
                &bucket,
                "--event-type",
                "object-create",
                "--event-type",
                "object-delete",
                "--prefix",
                prefix,
                "--queue",
                name,
            ]),
        });
    }
    notes.push("the consumer needs an API token with Queues read and write: export it as CLOUDFLARE_API_TOKEN (or have a credential provider return it)".into());
    (steps, SourceSpec::R2(format!("{account}/{name}")))
}

/// Run the plan's commands with `run`, in order, stopping at the first that fails. Nothing
/// runs unless `yes` or `confirm` says so, and nothing runs while a placeholder is unresolved.
pub fn apply(
    plan: &Plan,
    yes: bool,
    confirm: &mut dyn FnMut() -> Result<bool>,
    run: &mut dyn FnMut(&[String]) -> Result<()>,
) -> Result<()> {
    if !plan.unresolved.is_empty() {
        bail!(
            "not applying: fill in {} (see the options of `pail events setup --help`)",
            plan.unresolved.join(", ")
        );
    }
    if !yes && !confirm()? {
        bail!("not applying: not confirmed");
    }
    for (i, step) in plan.steps.iter().enumerate() {
        run(&step.argv).with_context(|| {
            format!(
                "step {} of {} failed ({}); the steps before it were applied",
                i + 1,
                plan.steps.len(),
                render(&step.argv)
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(consumer: &str) -> Opts {
        Opts {
            consumer: consumer.into(),
            ..Opts::default()
        }
    }

    fn cmds(p: &Plan) -> Vec<String> {
        p.steps.iter().map(|s| render(&s.argv)).collect()
    }

    #[test]
    fn providers_are_detected_from_the_store() {
        assert_eq!(detect("gs://b/r", None), Some(Provider::Gcs));
        assert_eq!(detect("s3://b/r", None), Some(Provider::S3));
        assert_eq!(
            detect("s3://b/r", Some("https://abc.r2.cloudflarestorage.com")),
            Some(Provider::R2)
        );
        assert_eq!(
            detect("https://a.blob.core.windows.net/c/r", None),
            Some(Provider::Azure)
        );
        assert_eq!(detect("file:///x", None), None);
        assert!(plan("file:///x/repo", None, &opts("ci")).is_err());
    }

    #[test]
    fn gcs_wires_refs_and_actions_into_a_subscription() {
        let p = plan("gs://acme/team/app", None, &opts("ci")).unwrap();
        assert_eq!(
            cmds(&p),
            [
                "gcloud pubsub topics create pail-ci",
                "gcloud storage buckets notifications create gs://acme --topic=pail-ci --object-prefix=team/app/refs/ --event-types=OBJECT_FINALIZE,OBJECT_DELETE",
                "gcloud storage buckets notifications create gs://acme --topic=pail-ci --object-prefix=team/app.actions/ --event-types=OBJECT_FINALIZE,OBJECT_DELETE",
                "gcloud pubsub subscriptions create pail-ci --topic=pail-ci",
            ]
        );
        assert!(p.unresolved.is_empty(), "the project is not in any command");
        assert_eq!(
            p.source.to_string(),
            "gcs:projects/PROJECT/subscriptions/pail-ci"
        );
        let mut o = opts("ci");
        o.project = Some("acme-prod".into());
        let p = plan("gs://acme/team/app", None, &o).unwrap();
        assert!(cmds(&p)[0].ends_with("--project=acme-prod"));
        assert_eq!(
            p.source.to_string(),
            "gcs:projects/acme-prod/subscriptions/pail-ci"
        );
        // a store with no path has no actions area
        let p = plan("gs://acme", None, &opts("ci")).unwrap();
        assert_eq!(p.steps.len(), 3);
        assert!(cmds(&p)[1].contains("--object-prefix=refs/"));
    }

    #[test]
    fn s3_wires_a_queue_with_a_policy_and_notification_rules() {
        let mut o = opts("ci");
        o.region = Some("eu-west-1".into());
        o.account = Some("111122223333".into());
        let p = plan("s3://acme/team/app", None, &o).unwrap();
        assert!(p.unresolved.is_empty());
        let c = cmds(&p);
        assert_eq!(
            c[0],
            "aws sqs create-queue --queue-name pail-ci --region eu-west-1"
        );
        assert!(c[1].contains("set-queue-attributes"));
        assert!(
            c[1].contains("--queue-url https://sqs.eu-west-1.amazonaws.com/111122223333/pail-ci")
        );
        let attrs: serde_json::Value =
            serde_json::from_str(&p.steps[1].argv[p.steps[1].argv.len() - 1]).unwrap();
        let policy: serde_json::Value =
            serde_json::from_str(attrs["Policy"].as_str().unwrap()).unwrap();
        assert_eq!(
            policy["Statement"][0]["Resource"],
            "arn:aws:sqs:eu-west-1:111122223333:pail-ci"
        );
        assert_eq!(
            policy["Statement"][0]["Condition"]["ArnLike"]["aws:SourceArn"],
            "arn:aws:s3:::acme"
        );
        let cfg: serde_json::Value =
            serde_json::from_str(&p.steps[2].argv[p.steps[2].argv.len() - 1]).unwrap();
        let rules = cfg["QueueConfigurations"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(
            rules[0]["Filter"]["Key"]["FilterRules"][0]["Value"],
            "team/app/refs/"
        );
        assert_eq!(
            rules[1]["Filter"]["Key"]["FilterRules"][0]["Value"],
            "team/app.actions/"
        );
        assert_eq!(
            p.source.to_string(),
            "sqs:https://sqs.eu-west-1.amazonaws.com/111122223333/pail-ci"
        );
        // without region and account the plan prints but cannot be applied
        let p = plan("s3://acme/team/app", None, &opts("ci")).unwrap();
        assert_eq!(p.unresolved, ["REGION", "ACCOUNT_ID"]);
    }

    #[test]
    fn azure_wires_event_grid_into_a_storage_queue() {
        let mut o = opts("ci");
        o.account = Some("acct".into());
        o.resource_group = Some("rg".into());
        o.project = Some("sub-1".into());
        o.region = Some("westeurope".into());
        let p = plan("https://acct.blob.core.windows.net/acme/team/app", None, &o).unwrap();
        assert!(p.unresolved.is_empty(), "{:?}", p.unresolved);
        let c = cmds(&p);
        assert!(c[0].starts_with("az storage queue create --name pail-ci --account-name acct"));
        assert!(c[1].starts_with("az eventgrid system-topic create --name pail-ci"));
        assert!(c[2].contains("--endpoint-type storagequeue"));
        assert!(c[2].contains("Microsoft.Storage.BlobCreated Microsoft.Storage.BlobDeleted"));
        assert!(c[2].ends_with(
            "--subject-begins-with /blobServices/default/containers/acme/blobs/team/app/refs/"
        ));
        assert!(c[3].ends_with("/blobs/team/app.actions/"));
        assert_eq!(
            p.source.to_string(),
            "azure:https://acct.queue.core.windows.net/pail-ci"
        );
    }

    #[test]
    fn r2_wires_a_queue_with_an_http_consumer() {
        let p = plan(
            "s3://acme/team/app",
            Some("https://0123abcd.r2.cloudflarestorage.com"),
            &opts("ci"),
        )
        .unwrap();
        assert_eq!(p.provider, Provider::R2);
        assert_eq!(
            cmds(&p),
            [
                "wrangler queues create pail-ci",
                "wrangler queues consumer http add pail-ci",
                "wrangler r2 bucket notification create acme --event-type object-create --event-type object-delete --prefix team/app/refs/ --queue pail-ci",
                "wrangler r2 bucket notification create acme --event-type object-create --event-type object-delete --prefix team/app.actions/ --queue pail-ci",
            ]
        );
        assert_eq!(p.source.to_string(), "r2:0123abcd/pail-ci");
        assert!(p.unresolved.is_empty());
    }

    #[test]
    fn consumer_names_are_checked_and_words_quoted() {
        for bad in ["", "Ci", "a b", "-x", "a/b", &"x".repeat(41)] {
            assert!(plan("gs://b/r", None, &opts(bad)).is_err(), "{bad}");
        }
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("--k=v,w/x"), "--k=v,w/x");
    }

    #[test]
    fn apply_needs_a_confirmation_and_no_placeholders_and_stops_at_a_failure() {
        let p = plan("gs://acme/team/app", None, &opts("ci")).unwrap();
        let mut ran: Vec<String> = vec![];
        // declined: nothing runs
        let r = apply(&p, false, &mut || Ok(false), &mut |a| {
            ran.push(render(a));
            Ok(())
        });
        assert!(r.is_err() && ran.is_empty());
        // --yes: no question, everything runs in order
        apply(&p, true, &mut || panic!("asked"), &mut |a| {
            ran.push(render(a));
            Ok(())
        })
        .unwrap();
        assert_eq!(ran, cmds(&p));
        // a failure stops it
        ran.clear();
        let r = apply(&p, true, &mut || Ok(true), &mut |a| {
            ran.push(render(a));
            if ran.len() == 2 {
                bail!("boom")
            } else {
                Ok(())
            }
        });
        assert!(r.is_err());
        assert_eq!(ran.len(), 2);
        // a placeholder in a command: refused even with --yes
        let p = plan("s3://acme/team/app", None, &opts("ci")).unwrap();
        ran.clear();
        let r = apply(&p, true, &mut || Ok(true), &mut |a| {
            ran.push(render(a));
            Ok(())
        });
        assert!(r.unwrap_err().to_string().contains("REGION"));
        assert!(ran.is_empty());
    }
}
