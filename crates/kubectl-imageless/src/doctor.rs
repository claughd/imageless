//! Diagnose whether a cluster is prepared to run imageless pods.
//!
//! Everything here is a *configuration* verdict. The node-local half of the
//! seam — containerd's runtime handler, the shim binary, the policy file — has
//! no API representation, so a fully green report still cannot prove a pod will
//! start. The report says so in its own summary line; a diagnostic that implied
//! otherwise would be worse than none.
//!
//! Each cluster check is a pure function over JSON already fetched, so the
//! interesting cases are fixture tests rather than subprocess tests.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use imageless::ResolverPolicy;
use serde_json::{json, Value};

use crate::kubectl::{Failure, Kubectl};
use crate::{flakeref, registry};

#[cfg_attr(test, derive(Debug))]
pub(crate) struct Options {
    pub runtime_class: String,
    pub connection: Vec<String>,
    pub json: bool,
    pub strict: bool,
    pub policy: Option<PathBuf>,
    pub source: Option<String>,
    pub repo: Option<String>,
    pub plain_http: bool,
}

#[derive(Clone, Copy, PartialEq)]
#[cfg_attr(test, derive(Debug))]
enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Skip => "skip",
        }
    }
}

struct Check {
    id: &'static str,
    status: Status,
    summary: String,
    detail: Vec<String>,
    remedy: Vec<String>,
}

impl Check {
    fn new(id: &'static str, status: Status, summary: impl Into<String>) -> Check {
        Check {
            id,
            status,
            summary: summary.into(),
            detail: Vec::new(),
            remedy: Vec::new(),
        }
    }

    fn detail(mut self, line: impl Into<String>) -> Check {
        self.detail.push(line.into());
        self
    }

    fn remedy(mut self, line: impl Into<String>) -> Check {
        self.remedy.push(line.into());
        self
    }
}

#[derive(Default)]
struct Report {
    checks: Vec<Check>,
    /// The cluster could not be looked at, which is a different answer from
    /// "the cluster is broken" and gets its own exit code.
    probe_failed: bool,
}

impl Report {
    fn push(&mut self, check: Check) {
        self.checks.push(check);
    }

    fn count(&self, status: Status) -> usize {
        self.checks.iter().filter(|c| c.status == status).count()
    }
}

/// The node label a RuntimeClass gates scheduling on, read from the cluster.
type Selector = Vec<(String, String)>;

pub(crate) fn run(options: &Options) -> ExitCode {
    let kubectl = match Kubectl::new(options.connection.clone()) {
        Ok(kubectl) => kubectl,
        Err(error) => {
            eprintln!("kubectl-imageless: {error}");
            return ExitCode::from(2);
        }
    };
    let report = diagnose(&kubectl, options);
    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&render_json(&report, &options.runtime_class))
                .expect("report serializes")
        );
    } else {
        print!("{}", render_text(&report));
    }
    exit_code(&report, options.strict)
}

/// Never returns `Err`: a failure to look is itself a check.
fn diagnose(kubectl: &Kubectl, options: &Options) -> Report {
    let mut report = Report::default();

    // Read the context before probing — it is offline, so it still answers
    // when the probe cannot — but report it after, because "which cluster"
    // only matters once "can we reach one" is settled.
    let context = kubectl
        .json(&["config", "view", "--minify", "-o", "json"])
        .ok();
    let context = check_context(context.as_ref());

    let client = kubectl.json(&["version", "--client", "-o", "json"]);
    match client {
        Err(Failure::NotInstalled(message)) => {
            report.probe_failed = true;
            report.push(
                Check::new("kubectl", Status::Fail, message)
                    .remedy("install kubectl, or set IMAGELESS_KUBECTL to the binary to drive"),
            );
        }
        client => {
            let client_version = client.ok().and_then(|value| {
                value["clientVersion"]["gitVersion"]
                    .as_str()
                    .map(str::to_string)
            });
            match kubectl.probe(&["get", "--raw", "/version"]) {
                Ok(body) => {
                    let server = serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|value| value["gitVersion"].as_str().map(str::to_string));
                    report.push(check_kubectl(client_version.as_deref(), server.as_deref()));
                }
                Err(_) => {
                    report.probe_failed = true;
                    report.push(
                        Check::new(
                            "kubectl",
                            Status::Fail,
                            "kubectl could not reach or authenticate to the API server; its own \
                             diagnostic is printed above",
                        )
                        .remedy(
                            "make `kubectl get --raw /version` work first — doctor has no \
                             separate connection to test, and this is kubectl's own credential \
                             path (exec plugin, token file, client certificate), reused \
                             deliberately rather than reimplemented",
                        ),
                    );
                }
            }
        }
    }

    report.push(context);

    if report.probe_failed {
        for id in ["runtime-class", "scheduling", "nodes"] {
            report.push(Check::new(
                id,
                Status::Skip,
                "the cluster could not be probed",
            ));
        }
    } else {
        let class =
            kubectl.json_or_absent(&["get", "runtimeclass", &options.runtime_class, "-o", "json"]);
        report.push(match &class {
            Ok(class) => check_runtime_class(class.as_ref(), &options.runtime_class),
            Err(Failure::Forbidden(message)) => Check::new(
                "runtime-class",
                Status::Skip,
                format!(
                    "cannot read RuntimeClass `{}`: {message}",
                    options.runtime_class
                ),
            )
            .detail(
                "RuntimeClasses are cluster-scoped and developers frequently are not granted them",
            ),
            Err(failure) => Check::new(
                "runtime-class",
                Status::Fail,
                format!("could not read RuntimeClass: {}", describe(failure)),
            ),
        });
        let class = class.ok().flatten();
        let (scheduling, selector) = check_scheduling(class.as_ref(), &options.runtime_class);
        report.push(scheduling);
        report.push(match kubectl.json(&["get", "nodes", "-o", "json"]) {
            Ok(nodes) => {
                let items = nodes["items"].as_array().cloned().unwrap_or_default();
                check_nodes(&items, &selector)
            }
            Err(Failure::Forbidden(message)) => Check::new(
                "nodes",
                Status::Skip,
                format!("cannot list nodes: {message}"),
            ),
            Err(failure) => Check::new(
                "nodes",
                Status::Fail,
                format!("could not list nodes: {}", describe(&failure)),
            ),
        });
    }

    report.push(node_config_check());

    // Offline checks run even when the cluster could not be probed: debugging
    // a policy file should not require a cluster.
    report.push(match &options.policy {
        Some(path) => match std::fs::read(path) {
            Ok(bytes) => check_policy(path, &bytes),
            Err(error) => Check::new(
                "policy",
                Status::Fail,
                format!("{}: {error}", path.display()),
            ),
        },
        None => Check::new(
            "policy",
            Status::Skip,
            "pass --policy PATH to check a node policy file",
        ),
    });

    let policy = options
        .policy
        .as_ref()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<ResolverPolicy>(&bytes).ok());
    report.push(match &options.source {
        Some(source) => check_source(source, policy.as_ref()),
        None => Check::new(
            "source",
            Status::Skip,
            "pass --source REF to check an external flake reference",
        ),
    });

    report.push(match &options.repo {
        Some(repo) => check_registry(repo, options.plain_http),
        None => Check::new(
            "registry",
            Status::Skip,
            "pass --repo HOST/REPO to check registry reachability",
        ),
    });

    report
}

fn check_context(context: Option<&Value>) -> Check {
    let source = match std::env::var("KUBECONFIG") {
        Ok(paths) if !paths.is_empty() => format!("$KUBECONFIG={paths} (merged by kubectl)"),
        _ => "kubectl's default (~/.kube/config); KUBECONFIG is not set".to_string(),
    };
    let Some(context) = context else {
        // In-cluster there is no kubeconfig at all and `--minify` exits
        // non-zero; that is a correct configuration, not a warning.
        if std::env::var_os("KUBERNETES_SERVICE_HOST").is_some() {
            return Check::new(
                "context",
                Status::Pass,
                "no kubeconfig context; KUBERNETES_SERVICE_HOST is set, so kubectl is using \
                 this pod's service account",
            );
        }
        return Check::new(
            "context",
            Status::Warn,
            "kubectl has no current context; every check below reports on whatever kubectl \
             defaults to",
        )
        .detail(source);
    };
    let name = context["current-context"].as_str().unwrap_or("");
    let cluster = context["clusters"][0]["name"].as_str().unwrap_or("");
    let server = context["clusters"][0]["cluster"]["server"]
        .as_str()
        .unwrap_or("");
    let namespace = context["contexts"][0]["context"]["namespace"]
        .as_str()
        .unwrap_or("default");
    Check::new(
        "context",
        Status::Pass,
        format!("context `{name}` -> cluster `{cluster}` at {server}, namespace `{namespace}`"),
    )
    .detail(source)
}

fn check_kubectl(client: Option<&str>, server: Option<&str>) -> Check {
    let (Some(client), Some(server)) = (client, server) else {
        return Check::new(
            "kubectl",
            Status::Pass,
            "kubectl reached the API server".to_string(),
        );
    };
    let summary = format!("kubectl {client} reached the API server (server {server})");
    match skew(client, server) {
        Some(message) => Check::new("kubectl", Status::Warn, message),
        None => Check::new("kubectl", Status::Pass, summary),
    }
}

/// kubectl supports one minor version of skew in each direction.
fn skew(client: &str, server: &str) -> Option<String> {
    let (client_minor, server_minor) = (minor(client)?, minor(server)?);
    let distance = client_minor.abs_diff(server_minor);
    (distance > 1).then(|| {
        let direction = if client_minor < server_minor {
            "behind"
        } else {
            "ahead of"
        };
        format!(
            "kubectl {client} is {distance} minor versions {direction} the server ({server}); \
             kubectl supports one minor of skew — upgrade kubectl before trusting anything below"
        )
    })
}

/// Leading digits only: managed clusters report minors like `27+`.
fn minor(version: &str) -> Option<u32> {
    let digits: String = version
        .trim_start_matches('v')
        .split('.')
        .nth(1)?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

fn check_runtime_class(class: Option<&Value>, name: &str) -> Check {
    let Some(class) = class else {
        return Check::new(
            "runtime-class",
            Status::Fail,
            format!(
                "RuntimeClass `{name}` does not exist — a pod naming it in \
                     runtimeClassName never starts"
            ),
        )
        .remedy("kubectl apply -f examples/runtimeclass.yaml")
        .remedy("pass --runtime-class NAME if your cluster names it differently");
    };
    let handler = class["handler"].as_str().unwrap_or_default();
    if handler == name {
        Check::new(
            "runtime-class",
            Status::Pass,
            format!("RuntimeClass `{name}` exists; handler `{handler}`"),
        )
    } else {
        Check::new(
            "runtime-class",
            Status::Warn,
            format!(
                "RuntimeClass `{name}` selects handler `{handler}` — every node's containerd \
                 must define a runtime keyed `{handler}`, not `{name}`: the key is the \
                 handler, not the RuntimeClass name"
            ),
        )
    }
}

fn check_scheduling(class: Option<&Value>, name: &str) -> (Check, Selector) {
    let Some(class) = class else {
        return (
            Check::new("scheduling", Status::Skip, "no RuntimeClass to read"),
            Vec::new(),
        );
    };
    let selector: Selector = class["scheduling"]["nodeSelector"]
        .as_object()
        .map(|labels| {
            labels
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default();
    if selector.is_empty() {
        return (
            Check::new(
                "scheduling",
                Status::Warn,
                format!("RuntimeClass `{name}` has no scheduling.nodeSelector"),
            )
            .remedy(
                "pods can land on nodes that never had the shim installed, where they fail at \
                 container start instead of staying Pending; examples/runtimeclass.yaml gates \
                 on imageless.run/runtime=v2",
            ),
            selector,
        );
    }
    (
        Check::new(
            "scheduling",
            Status::Pass,
            format!(
                "RuntimeClass `{name}` gates scheduling on {}",
                render_selector(&selector)
            ),
        ),
        selector,
    )
}

fn check_nodes(nodes: &[Value], selector: &Selector) -> Check {
    if selector.is_empty() {
        return Check::new(
            "nodes",
            Status::Skip,
            "the RuntimeClass names no node label, so no node can be identified as prepared",
        );
    }
    let matching: Vec<&Value> = nodes
        .iter()
        .filter(|node| {
            selector.iter().all(|(key, value)| {
                node["metadata"]["labels"][key].as_str() == Some(value.as_str())
            })
        })
        .collect();
    if matching.is_empty() {
        return Check::new(
            "nodes",
            Status::Fail,
            format!(
                "no node carries {} — pods using this RuntimeClass stay Pending forever",
                render_selector(selector)
            ),
        )
        .detail(format!("{} nodes in the cluster", nodes.len()))
        .remedy(
            "label a prepared node, and only after the shim and containerd handler are \
             installed and healthy on it",
        );
    }
    let ready = matching.iter().filter(|node| is_ready(node)).count();
    let mut check = Check::new(
        "nodes",
        if ready == matching.len() {
            Status::Pass
        } else {
            Status::Warn
        },
        format!(
            "{ready} of {} nodes matching {} are Ready",
            matching.len(),
            render_selector(selector)
        ),
    );
    for node in matching {
        let name = node["metadata"]["name"].as_str().unwrap_or("?");
        let condition = if is_ready(node) { "Ready" } else { "NotReady" };
        let info = &node["status"]["nodeInfo"];
        check = check.detail(format!(
            "{name}  {condition}  {}  {}",
            info["architecture"].as_str().unwrap_or("?"),
            registry::printable(info["containerRuntimeVersion"].as_str().unwrap_or("?"))
        ));
    }
    check
}

fn is_ready(node: &Value) -> bool {
    node["status"]["conditions"]
        .as_array()
        .is_some_and(|conditions| {
            conditions
                .iter()
                .any(|c| c["type"] == "Ready" && c["status"] == "True")
        })
}

/// Always a skip, never a pass: this is the half of the seam with no API
/// representation, and the reason a green report is not proof.
fn node_config_check() -> Check {
    Check::new(
        "node-config",
        Status::Skip,
        "node-local containerd configuration has no API representation",
    )
    .remedy(
        "merge examples/containerd-config.toml (config version 3; the 1.x shape is in \
         examples/containerd-config-v2.toml) and restart containerd",
    )
    .remedy(
        "external-reference pods additionally need run.imageless.* in the handler's \
         pod_annotations and container_annotations",
    )
}

fn check_policy(path: &Path, bytes: &[u8]) -> Check {
    let disclaimer = "this is the file you named, not the node's — doctor has no way to read \
                      /etc/imageless/policy.json on a node";
    let policy: ResolverPolicy = match serde_json::from_slice(bytes) {
        Ok(policy) => policy,
        Err(error) => {
            return Check::new(
                "policy",
                Status::Fail,
                format!(
                    "`{}` is not a valid node policy: {error} — the node parses this file with \
                     deny_unknown_fields, so a typo is a fail-closed node, not a default",
                    path.display()
                ),
            )
            .detail(disclaimer)
        }
    };
    let issuers = format!("{} issuers authorized", policy.issuers.len());
    if policy.cache_only {
        // A hardened node is not a broken node.
        return Check::new(
            "policy",
            Status::Warn,
            format!(
                "`{}` sets cache_only: true, so this node resolves digest-addressed releases \
                 only and refuses every flake source, embedded or external",
                path.display()
            ),
        )
        .detail(issuers)
        .detail(disclaimer);
    }
    if policy.eval_allowed_uri_prefixes.is_empty() {
        return Check::new(
            "policy",
            Status::Fail,
            format!(
                "`{}` sets cache_only: false but eval_allowed_uri_prefixes is empty, so every \
                 external source is denied",
                path.display()
            ),
        )
        .detail(disclaimer);
    }
    let mut check = Check::new(
        "policy",
        Status::Pass,
        format!(
            "`{}` allows node-side evaluation; eval_allowed_uri_prefixes: {}",
            path.display(),
            policy.eval_allowed_uri_prefixes.join(", ")
        ),
    );
    for prefix in &policy.eval_allowed_uri_prefixes {
        // SPEC §3: prefixes are literal bytes, so an unterminated one also
        // authorizes a look-alike organization.
        if !prefix.ends_with('/') && !prefix.ends_with(':') {
            check.status = Status::Warn;
            check = check.remedy(format!(
                "eval_allowed_uri_prefixes entry `{prefix}` is matched as a literal byte \
                 prefix, so it also authorizes `{prefix}-evil/anything` — terminate it at a `/`"
            ));
        }
    }
    check.detail(issuers).detail(disclaimer)
}

fn check_source(source: &str, policy: Option<&ResolverPolicy>) -> Check {
    if let Err(error) = imageless::validate_source(source) {
        return Check::new("source", Status::Fail, error.to_string());
    }
    let mut check = Check::new(
        "source",
        Status::Pass,
        format!("`{source}` is admissible as run.imageless.source"),
    );
    if let Some(policy) = policy {
        // The node's own matcher: a literal byte prefix, checked after
        // cache_only.
        let authorized = policy
            .eval_allowed_uri_prefixes
            .iter()
            .find(|prefix| source.starts_with(prefix.as_str()));
        match authorized {
            Some(prefix) => {
                check = check.detail(format!(
                    "authorized by eval_allowed_uri_prefixes entry `{prefix}`"
                ));
            }
            None => {
                return Check::new(
                    "source",
                    Status::Fail,
                    format!(
                        "no eval_allowed_uri_prefixes entry is a prefix of `{source}` — the node \
                         would answer `development source URI is not authorized by node policy`"
                    ),
                )
                .remedy(format!(
                    "add `{}` (terminated at a `/`, so it cannot also authorize a look-alike \
                     organization)",
                    flakeref::policy_prefix(source)
                ));
            }
        }
    }
    match flakeref::pin(source) {
        Ok(Some(_)) => check,
        Ok(None) => {
            check.status = Status::Warn;
            check
                .detail(format!("`{source}` names no revision"))
                .remedy(
                    "the node deliberately does not police pin forms (SPEC §3), so it will \
                     materialize whatever that reference resolves to at run time — this warning \
                     is authoring-tool policy, not a node contract",
                )
        }
        Err(error) => Check::new("source", Status::Fail, error),
    }
}

fn check_registry(repo: &str, plain_http: bool) -> Check {
    match registry::Registry::connect(repo, plain_http) {
        Ok(_) => {
            let host = repo.split('/').next().unwrap_or(repo);
            // Reaching /v2/ proves reachability and, at most, read access: a
            // Bearer registry hands an anonymous client a pull token without
            // ever seeing a credential. Say which one was presented, and leave
            // push permission to the push that proves it.
            let credentials = match crate::auth::lookup(host) {
                Ok((_, source)) => source.describe(),
                Err(_) => "no usable stored credentials".to_string(),
            };
            let mut check = Check::new(
                "registry",
                Status::Pass,
                format!("`{host}` answered /v2/ ({credentials})"),
            )
            .detail("push permission is not checked here; the first push proves it");
            // `is_loopback` splits the port itself, bracketed IPv6 included;
            // cutting at the first `:` here turned `[::1]:5000` into `[`.
            if registry::is_loopback(host) {
                check.status = Status::Warn;
                check = check.remedy(
                    "the seed is pulled by the node, not by this client: a loopback host means \
                     the node's own loopback there — kind's local-registry recipe is what makes \
                     the two agree",
                );
            }
            check
        }
        Err(error) => Check::new("registry", Status::Fail, error),
    }
}

fn describe(failure: &Failure) -> String {
    match failure {
        Failure::NotInstalled(message) | Failure::Probe(message) | Failure::Forbidden(message) => {
            registry::printable(message)
        }
        Failure::Other { code, stderr } => match code {
            Some(code) => format!("exit {code}: {}", registry::printable(stderr)),
            None => registry::printable(stderr),
        },
    }
}

fn render_selector(selector: &Selector) -> String {
    selector
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn render_text(report: &Report) -> String {
    let mut out = format!("kubectl-imageless doctor {}\n\n", env!("CARGO_PKG_VERSION"));
    for check in &report.checks {
        out.push_str(&format!(
            "{}  {:<14} {}\n",
            check.status.label(),
            check.id,
            check.summary
        ));
        for line in &check.detail {
            out.push_str(&format!("      {line}\n"));
        }
        for line in &check.remedy {
            out.push_str(&format!("      fix  {line}\n"));
        }
    }
    out.push_str(&format!(
        "\n{} passed, {} warnings, {} skipped, {} failed — a green report is not proof the seam\n\
         works; see the node-config check.\n",
        report.count(Status::Pass),
        report.count(Status::Warn),
        report.count(Status::Skip),
        report.count(Status::Fail),
    ));
    out
}

fn render_json(report: &Report, runtime_class: &str) -> Value {
    json!({
        "schema": "imageless.doctor.v1",
        "runtime_class": runtime_class,
        "probe_failed": report.probe_failed,
        "summary": {
            "pass": report.count(Status::Pass),
            "warn": report.count(Status::Warn),
            "skip": report.count(Status::Skip),
            "fail": report.count(Status::Fail),
        },
        "checks": report.checks.iter().map(|check| json!({
            "id": check.id,
            "status": check.status.label(),
            "summary": check.summary,
            "detail": check.detail,
            "remedy": check.remedy,
        })).collect::<Vec<_>>(),
    })
}

/// `3` is worth its own code: doctor is meant to gate CI, and "your cluster is
/// broken" is a different answer from "I could not look".
fn exit_code(report: &Report, strict: bool) -> ExitCode {
    if report.probe_failed {
        return ExitCode::from(3);
    }
    if report.count(Status::Fail) > 0 || (strict && report.count(Status::Warn) > 0) {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(handler: &str, selector: Option<Value>) -> Value {
        let mut class = json!({"handler": handler, "metadata": {"name": "imageless"}});
        if let Some(selector) = selector {
            class["scheduling"] = json!({ "nodeSelector": selector });
        }
        class
    }

    fn node(name: &str, labels: Value, ready: bool) -> Value {
        json!({
            "metadata": {"name": name, "labels": labels},
            "status": {
                "conditions": [{"type": "Ready", "status": if ready {"True"} else {"False"}}],
                "nodeInfo": {"architecture": "amd64", "containerRuntimeVersion": "containerd://2.1.4"},
            },
        })
    }

    #[test]
    fn an_absent_runtime_class_is_fatal_and_names_the_example() {
        let check = check_runtime_class(None, "imageless");
        assert_eq!(check.status, Status::Fail);
        assert!(check.remedy.iter().any(|r| r.contains("runtimeclass.yaml")));
    }

    #[test]
    fn a_handler_that_differs_from_the_name_is_called_out() {
        let class = class("imageless-v2", None);
        let check = check_runtime_class(Some(&class), "imageless");
        assert_eq!(check.status, Status::Warn);
        // The containerd key is the handler, not the RuntimeClass name.
        assert!(
            check.summary.contains("keyed `imageless-v2`")
                || check.summary.contains("`imageless-v2`")
        );
    }

    #[test]
    fn the_selector_comes_from_the_cluster_not_a_constant() {
        // Nothing in the shim or library reads this label; hardcoding it would
        // make doctor wrong on every cluster that chose its own.
        let class = class("imageless", Some(json!({"example.com/imageless": "yes"})));
        let (check, selector) = check_scheduling(Some(&class), "imageless");
        assert_eq!(check.status, Status::Pass);
        assert_eq!(
            selector,
            vec![("example.com/imageless".to_string(), "yes".to_string())]
        );
    }

    #[test]
    fn a_runtime_class_without_scheduling_warns_about_silent_scheduling() {
        let class = class("imageless", None);
        let (check, selector) = check_scheduling(Some(&class), "imageless");
        assert_eq!(check.status, Status::Warn);
        assert!(selector.is_empty());
    }

    #[test]
    fn no_matching_node_is_fatal_because_pods_stay_pending() {
        let selector = vec![("imageless.run/runtime".to_string(), "v2".to_string())];
        let nodes = vec![node("plain", json!({}), true)];
        let check = check_nodes(&nodes, &selector);
        assert_eq!(check.status, Status::Fail);
        assert!(check.summary.contains("stay Pending"), "{}", check.summary);
    }

    #[test]
    fn matching_but_unready_nodes_warn_rather_than_pass() {
        let selector = vec![("imageless.run/runtime".to_string(), "v2".to_string())];
        let labels = json!({"imageless.run/runtime": "v2"});
        let nodes = vec![node("a", labels.clone(), true), node("b", labels, false)];
        let check = check_nodes(&nodes, &selector);
        assert_eq!(check.status, Status::Warn);
        assert!(check.summary.starts_with("1 of 2"), "{}", check.summary);
    }

    #[test]
    fn a_cache_only_policy_warns_but_is_not_a_failure() {
        let policy = br#"{"system":"x86_64-linux","cache_only":true}"#;
        let check = check_policy(Path::new("p.json"), policy);
        assert_eq!(check.status, Status::Warn);
        assert!(check.summary.contains("cache_only: true"));
    }

    #[test]
    fn evaluation_enabled_with_no_prefixes_denies_everything() {
        let policy = br#"{"system":"x86_64-linux","cache_only":false}"#;
        let check = check_policy(Path::new("p.json"), policy);
        assert_eq!(check.status, Status::Fail);
    }

    #[test]
    fn an_unterminated_prefix_is_flagged_as_matching_a_look_alike() {
        let policy = br#"{"system":"x86_64-linux","cache_only":false,
                          "eval_allowed_uri_prefixes":["github:myorg"]}"#;
        let check = check_policy(Path::new("p.json"), policy);
        assert_eq!(check.status, Status::Warn);
        assert!(check.remedy.iter().any(|r| r.contains("myorg-evil")));
    }

    #[test]
    fn a_typo_in_the_policy_is_a_fail_closed_node_not_a_default() {
        // The node parses with deny_unknown_fields.
        let policy = br#"{"system":"x86_64-linux","cacheOnly":true}"#;
        let check = check_policy(Path::new("p.json"), policy);
        assert_eq!(check.status, Status::Fail);
        assert!(check.summary.contains("deny_unknown_fields"));
    }

    #[test]
    fn a_source_is_checked_against_the_node_s_own_prefix_matcher() {
        let policy: ResolverPolicy = serde_json::from_slice(
            br#"{"system":"x86_64-linux","cache_only":false,
                 "eval_allowed_uri_prefixes":["github:acme/"]}"#,
        )
        .unwrap();
        let rev = "0123456789abcdef0123456789abcdef01234567";
        let allowed = check_source(&format!("github:acme/agent/{rev}"), Some(&policy));
        assert_eq!(allowed.status, Status::Pass);
        let denied = check_source(&format!("github:other/agent/{rev}"), Some(&policy));
        assert_eq!(denied.status, Status::Fail);
        assert!(denied
            .remedy
            .iter()
            .any(|r| r.contains("github:other/agent/")));
    }

    #[test]
    fn an_unpinned_source_warns_and_says_whose_rule_it_is() {
        let check = check_source("github:acme/agent", None);
        assert_eq!(check.status, Status::Warn);
        assert!(check
            .remedy
            .iter()
            .any(|r| r.contains("not a node contract")));
    }

    #[test]
    fn a_source_the_node_rejects_carries_the_node_s_text() {
        let check = check_source("path:/srv/flake", None);
        assert_eq!(check.status, Status::Fail);
        assert!(check.summary.contains("node-local schemes"));
    }

    #[test]
    fn skew_is_only_reported_beyond_one_minor() {
        assert!(skew("v1.33.1", "v1.33.1").is_none());
        assert!(skew("v1.32.0", "v1.33.1").is_none());
        assert!(skew("v1.24.9", "v1.33.1").is_some());
        // Managed control planes report minors like `27+`.
        assert_eq!(minor("v1.27+"), Some(27));
    }

    #[test]
    fn could_not_look_is_a_different_exit_code_from_broken() {
        let mut report = Report {
            probe_failed: true,
            ..Default::default()
        };
        report.push(Check::new("x", Status::Fail, ""));
        assert_eq!(
            format!("{:?}", exit_code(&report, false)),
            format!("{:?}", ExitCode::from(3))
        );

        let mut broken = Report::default();
        broken.push(Check::new("x", Status::Fail, ""));
        assert_eq!(
            format!("{:?}", exit_code(&broken, false)),
            format!("{:?}", ExitCode::FAILURE)
        );

        let mut warned = Report::default();
        warned.push(Check::new("x", Status::Warn, ""));
        assert_eq!(
            format!("{:?}", exit_code(&warned, false)),
            format!("{:?}", ExitCode::SUCCESS)
        );
        assert_eq!(
            format!("{:?}", exit_code(&warned, true)),
            format!("{:?}", ExitCode::FAILURE)
        );
    }

    #[test]
    fn the_summary_refuses_to_claim_the_seam_works() {
        let mut report = Report::default();
        report.push(Check::new("x", Status::Pass, "fine"));
        let text = render_text(&report);
        assert!(text.contains("not proof the seam"), "{text}");
    }

    #[test]
    fn the_json_report_is_one_document_with_stable_ids() {
        let mut report = Report::default();
        report.push(Check::new("runtime-class", Status::Pass, "fine"));
        let value = render_json(&report, "imageless");
        assert_eq!(value["schema"], "imageless.doctor.v1");
        assert_eq!(value["checks"][0]["id"], "runtime-class");
        assert_eq!(value["summary"]["pass"], 1);
    }
}
