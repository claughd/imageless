//! End-to-end tests driving the resolver daemon through the `imageless-runc`
//! interposer, with a fake Nix client standing in for the store.
//!
//! `CARGO_BIN_EXE_*` only resolves binaries of the crate under test, so these
//! tests live in the resolver crate (which owns two of the three binaries) and
//! locate `imageless-runc` as a sibling artifact in the target directory. Run
//! them with a workspace-level `cargo test` so all three binaries are built.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const STORE_PATH: &str = "/nix/store/00000000000000000000000000000000-rootfs";
const STORE_MOUNT_PATH: &str = "/nix/store/11111111111111111111111111111111-tools";

struct TestRelease {
    reference: String,
    policy: PathBuf,
}

impl TestRelease {
    fn new(dir: &Path) -> Self {
        Self::with_selectors(dir, serde_json::json!({}))
    }

    fn with_selectors(dir: &Path, selectors: serde_json::Value) -> Self {
        // The manifest and policy families are non-exhaustive outside the
        // library, so the fixtures are built the way real inputs arrive: as
        // JSON through serde.
        let manifest: imageless::ReleaseManifest = serde_json::from_value(serde_json::json!({
            "schema": imageless::RELEASE_SCHEMA,
            "issuer": "test",
            "name": "rootfs",
            "selectors": selectors,
            "targets": {
                "x86_64-linux": {
                    "rootfs": STORE_PATH,
                    "cache": "local",
                    "process": {
                        "entrypoint": ["/bin/agent"],
                        "default_args": ["serve"],
                        "working_directory": "/workspace",
                        "environment": [
                            {
                                "name": "LOCKED",
                                "value": "release",
                                "allow_workload_override": false,
                            },
                            {
                                "name": "TERM",
                                "value": "release",
                                "allow_workload_override": true,
                            },
                        ],
                    },
                    "mounts": [
                        { "source": STORE_MOUNT_PATH, "destination": "/opt/tools" },
                    ],
                },
            },
        }))
        .unwrap();
        let bytes = imageless::canonical_manifest_bytes(&manifest).unwrap();
        let digest = imageless::manifest_digest(&bytes);
        let catalog = dir.join("catalog");
        std::fs::create_dir(&catalog).unwrap();
        let digest_directory = catalog.join("sha256");
        std::fs::create_dir(&digest_directory).unwrap();
        std::fs::write(digest_directory.join(format!("{digest}.json")), bytes).unwrap();

        let policy = serde_json::json!({
            "system": "x86_64-linux",
            "cache_only": true,
            "eval_allowed_uri_prefixes": [],
            "issuers": {
                "test": {
                    "source": {
                        "kind": "local",
                        "directory": std::fs::canonicalize(catalog).unwrap(),
                    },
                    "allowed_releases": ["rootfs"],
                    "caches": {
                        "local": {
                            "substituter": "file:///nix/store",
                            "public_keys": [],
                        },
                    },
                },
            },
        });
        let policy_path = dir.join("policy.json");
        std::fs::write(&policy_path, serde_json::to_vec(&policy).unwrap()).unwrap();
        std::fs::set_permissions(
            &policy_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        Self {
            reference: format!("test/rootfs@sha256:{digest}"),
            policy: policy_path,
        }
    }

    fn annotations(&self) -> serde_json::Value {
        serde_json::json!({ "imageless.run/release-v1": self.reference })
    }

    fn development_policy(dir: &Path) -> PathBuf {
        let policy = serde_json::json!({
            "system": "x86_64-linux",
            "cache_only": false,
            "eval_allowed_uri_prefixes": ["path:"],
            "issuers": {},
            // The fake Nix reports what it observed into this test's temp
            // directory, which a confined evaluator cannot write. Confinement
            // is exercised by the library's own test and the acceptance gates.
            "unconfined_evaluation": true,
        });
        let path = dir.join("development-policy.json");
        std::fs::write(&path, serde_json::to_vec(&policy).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        path
    }
}

fn temp_dir(label: &str) -> PathBuf {
    // The resolver binds `resolver.sock` in here, and sun_path holds 108 bytes.
    // Outside the Nix sandbox (whose TMPDIR is a short /build) the temp dir is
    // itself ~40 bytes, so the name stays compact: pid + counter is unique
    // within a run, and a stale directory from a recycled pid is cleared.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "il-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn executable(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

fn fake_nix(path: &Path, body: &str) {
    executable(
        path,
        &format!(
            r#"
root=''
requested=''
next_root=0
for arg in "$@"; do
  if [ "$next_root" = 1 ]; then root="$arg"; next_root=0; fi
  if [ "$arg" = "--add-root" ] || [ "$arg" = "--out-link" ]; then next_root=1; fi
  case "$arg" in /nix/store/*) requested="$arg" ;; esac
done
{body}
rm -f "$root"
store_path="${{FAKE_STORE_PATH:-${{requested:-{STORE_PATH}}}}}"
ln -s "$store_path" "$root"
printf '%s\n' "$store_path"
"#
        ),
    );
}

fn write_config(bundle: &Path, annotations: serde_json::Value) -> PathBuf {
    let config = bundle.join("config.json");
    let mut document = serde_json::to_value(oci_spec::runtime::Spec::default()).unwrap();
    let object = document.as_object_mut().unwrap();
    object.insert(
        "root".into(),
        serde_json::json!({ "path": "rootfs", "readonly": true }),
    );
    object.insert(
        "process".into(),
        serde_json::json!({
            "args": ["placeholder", "workload-arg"],
            "cwd": "/original",
            "env": ["LOCKED=workload", "TERM=workload", "KEEP=workload"]
        }),
    );
    object.insert("annotations".into(), annotations);
    std::fs::write(&config, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    std::fs::create_dir_all(bundle.join("rootfs")).unwrap();
    config
}

struct ResolverProcess {
    child: Child,
    socket: PathBuf,
}

impl ResolverProcess {
    fn start(dir: &Path, nix: &Path, policy: &Path) -> Self {
        Self::start_with_env(dir, nix, policy, &[])
    }

    fn start_with_env(
        dir: &Path,
        nix: &Path,
        policy: &Path,
        environment: &[(&str, &Path)],
    ) -> Self {
        Self::start_inner(dir, nix, policy, environment, None)
    }

    fn start_development(
        dir: &Path,
        nix: &Path,
        policy: &Path,
        user: &str,
        environment: &[(&str, &Path)],
    ) -> Self {
        Self::start_inner(dir, nix, policy, environment, Some(user))
    }

    fn start_inner(
        dir: &Path,
        nix: &Path,
        policy: &Path,
        environment: &[(&str, &Path)],
        development_user: Option<&str>,
    ) -> Self {
        let socket = dir.join("resolver.sock");
        let mut command = Command::new(env!("CARGO_BIN_EXE_imageless-resolver"));
        command
            .args([
                "--socket-path",
                socket.to_str().unwrap(),
                "--max-realizations",
                "2",
                "--realization-timeout-seconds",
                "30",
                "--policy-file",
                policy.to_str().unwrap(),
            ])
            .env("IMAGELESS_NIX", nix)
            .env("IMAGELESS_NIX_STORE", nix)
            .stderr(Stdio::inherit());
        if let Some(user) = development_user {
            command.args([
                "--development-worker",
                env!("CARGO_BIN_EXE_imageless-dev-resolver"),
                "--development-worker-user",
                user,
            ]);
        }
        for (key, value) in environment {
            command.env(key, value);
        }
        let child = command.spawn().unwrap();
        let started = Instant::now();
        while !socket.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "resolver socket did not appear"
            );
            thread::sleep(Duration::from_millis(10));
        }
        Self { child, socket }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ResolverProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// `CARGO_BIN_EXE_*` cannot name another crate's binary; find `imageless-runc`
/// as a sibling of this test executable in the target directory, building it
/// once with the same cargo if it is not already there.
fn runc_binary() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let binary = path.join("imageless-runc");
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "imageless-runc"])
            .status()
            .expect("cargo is available to build imageless-runc");
        assert!(status.success(), "building imageless-runc failed");
    });
    assert!(
        binary.is_file(),
        "imageless-runc was not built at {}",
        binary.display()
    );
    binary
}

fn runc(bundle: &Path, delegate: &Path, socket: &Path) -> Command {
    let mut command = Command::new(runc_binary());
    command
        .env("IMAGELESS_RUNC", delegate)
        .env("IMAGELESS_RESOLVER_SOCKET", socket)
        .env("IMAGELESS_REALIZATION_TIMEOUT_SECONDS", "3")
        .args(["--root", "/run/runc-test", "create", "--bundle"])
        .arg(bundle)
        .args(["--pid-file", "/tmp/imageless-test.pid", "integration-test"]);
    command
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "imageless-runc failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn root_path(config: &Path) -> String {
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
    document["root"]["path"].as_str().unwrap().to_string()
}

fn config_document(config: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap()
}

#[test]
fn development_source_uses_the_sanitized_unprivileged_worker() {
    let dir = temp_dir("development-worker");
    let policy = TestRelease::development_policy(&dir);
    let bundle = dir.join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    let config = write_config(
        &bundle,
        serde_json::json!({
            "run.imageless.source": "/source",
            "run.imageless.output": "rootfs"
        }),
    );
    std::fs::create_dir_all(bundle.join("rootfs/source")).unwrap();

    let worker_uid = dir.join("worker-uid");
    let nix = dir.join("nix");
    fake_nix(
        &nix,
        &format!(
            r#"
# The evaluation is the `build` call; root registration is `--realise`.
# Both name a root now: the build holds an --out-link while it runs.
case " $* " in *" build "*)
  test -z "${{LEAK_SECRET+x}}"
  while read -r key real effective saved filesystem; do
    if [ "$key" = "Uid:" ]; then
      printf '%s\n' "$effective" > "{}"
      break
    fi
  done < /proc/self/status
  printf '%s\n' "{}"
  exit 0
  ;;
esac
"#,
            worker_uid.display(),
            STORE_PATH
        ),
    );
    let delegate = dir.join("delegate");
    executable(&delegate, "exit 0");
    let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
    let leak = dir.join("must-not-reach-worker");
    let resolver = ResolverProcess::start_development(
        &dir,
        &nix,
        &policy,
        user.trim(),
        &[("LEAK_SECRET", &leak)],
    );
    assert_success(&runc(&bundle, &delegate, &resolver.socket).output().unwrap());
    assert_eq!(root_path(&config), STORE_PATH);
    assert_eq!(
        std::fs::read_to_string(worker_uid).unwrap().trim(),
        unsafe { libc::geteuid() }.to_string()
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn runc_client_expands_create_and_preserves_the_oci_command_line() {
    let dir = temp_dir("runc-create");
    let release = TestRelease::new(&dir);
    let bundle = dir.join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    let config = write_config(&bundle, release.annotations());
    let nix = dir.join("nix");
    fake_nix(&nix, "true");
    let resolver = ResolverProcess::start(&dir, &nix, &release.policy);
    let delegate = dir.join("delegate");
    let argv_log = dir.join("argv");
    let config_log = dir.join("delegated-config.json");
    executable(
        &delegate,
        "printf '%s\\n' \"$@\" > \"$ARGV_LOG\"\ncp \"$BUNDLE/config.json\" \"$CONFIG_LOG\"",
    );
    let output = runc(&bundle, &delegate, &resolver.socket)
        .env("ARGV_LOG", &argv_log)
        .env("CONFIG_LOG", &config_log)
        .env("BUNDLE", &bundle)
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(root_path(&config), STORE_PATH);
    assert_eq!(root_path(&config_log), STORE_PATH);
    assert_eq!(
        std::fs::read_to_string(&argv_log).unwrap(),
        format!(
            "--root\n/run/runc-test\ncreate\n--bundle\n{}\n--pid-file\n/tmp/imageless-test.pid\nintegration-test\n",
            bundle.display()
        )
    );
    assert!(bundle.join(imageless::GC_ROOT_NAME).is_symlink());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn runc_client_passthrough_is_resolver_independent_and_failure_cleans_roots() {
    let dir = temp_dir("runc-passthrough-cleanup");
    let delegate = dir.join("delegate");
    executable(&delegate, "exit 0");
    let missing = dir.join("missing.sock");

    let plain = dir.join("plain");
    std::fs::create_dir(&plain).unwrap();
    let config = write_config(&plain, serde_json::json!({}));
    assert_success(&runc(&plain, &delegate, &missing).output().unwrap());
    assert_eq!(root_path(&config), "rootfs");

    let release = TestRelease::new(&dir);
    let selected = dir.join("selected");
    std::fs::create_dir(&selected).unwrap();
    write_config(&selected, release.annotations());
    let nix = dir.join("nix");
    fake_nix(&nix, "true");
    let resolver = ResolverProcess::start(&dir, &nix, &release.policy);
    executable(&delegate, "exit 41");
    let output = runc(&selected, &delegate, &resolver.socket)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(41));
    assert!(!selected.join(imageless::GC_ROOT_NAME).exists());
    assert!(!selected.join(imageless::GC_ROOTS_DIR_NAME).exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn annotated_create_applies_release_metadata_roots_and_telemetry() {
    let dir = temp_dir("annotated-create");
    let release = TestRelease::new(&dir);
    let bundle = dir.join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    let config = write_config(&bundle, release.annotations());
    let nix = dir.join("nix");
    let delegate = dir.join("delegate");
    let telemetry = dir.join("timings.jsonl");
    fake_nix(&nix, "true");
    executable(&delegate, "exit 0");
    let resolver = ResolverProcess::start(&dir, &nix, &release.policy);
    let output = runc(&bundle, &delegate, &resolver.socket)
        .env("IMAGELESS_TELEMETRY_PATH", &telemetry)
        .output()
        .unwrap();
    assert_success(&output);

    assert_eq!(root_path(&config), STORE_PATH);
    let document = config_document(&config);
    assert_eq!(
        document["process"]["args"],
        serde_json::json!(["/bin/agent", "serve"])
    );
    assert_eq!(document["process"]["cwd"], "/workspace");
    assert_eq!(
        document["process"]["env"],
        serde_json::json!(["LOCKED=release", "TERM=workload", "KEEP=workload"])
    );
    let release_mount = document["mounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|mount| mount["destination"] == "/opt/tools")
        .expect("release mount was appended to the OCI spec");
    assert_eq!(release_mount["source"], STORE_MOUNT_PATH);
    let store_projection = document["mounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|mount| mount["destination"] == imageless::NIX_STORE_PATH)
        .expect("node Nix store projection was appended to the OCI spec");
    assert_eq!(store_projection["source"], imageless::NIX_STORE_PATH);
    assert_eq!(
        store_projection["options"],
        serde_json::json!(["rbind", "ro", "nosuid", "nodev"])
    );
    assert_eq!(
        std::fs::read_link(bundle.join(imageless::GC_ROOT_NAME)).unwrap(),
        Path::new(STORE_PATH)
    );
    assert_eq!(
        std::fs::read_link(bundle.join(imageless::GC_ROOTS_DIR_NAME).join("1")).unwrap(),
        Path::new(STORE_MOUNT_PATH)
    );
    let events = std::fs::read_to_string(telemetry)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 9);
    assert_eq!(
        events
            .iter()
            .map(|event| event["stage"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            // The original four, unchanged in name, order and span.
            "selection",
            "policy_verification",
            "substitution",
            "rewrite",
            // Carved out of two of the above rather than added beside them.
            "manifest_fetch",
            "staging",
            "evaluation",
            "root_registration",
            "delegate_startup"
        ]
    );
    let stage = |name: &str| {
        events
            .iter()
            .find(|event| event["stage"] == name)
            .map(|event| event["duration_us"].as_u64().unwrap())
            .unwrap()
    };
    // The defect this split exists to fix: the manifest fetch is inside the
    // policy span, so a slow catalog used to read as a slow policy check.
    assert!(
        stage("manifest_fetch") <= stage("policy_verification"),
        "the manifest fetch is carved out of policy verification, not added to it"
    );
    // This release is materialized, not staged: staging is for embedded
    // development sources only.
    assert_eq!(stage("staging"), 0);
    assert!(stage("evaluation") <= stage("substitution"));
    assert!(events.iter().all(|event| {
        event["schema"] == "imageless.timing.v1"
            && event["release"] == release.reference
            && event["outcome"] == "success"
            && event["duration_us"].is_u64()
    }));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_failed_create_records_an_error_event_where_it_previously_recorded_nothing() {
    // The sink held successes only, because every failure exited the shim
    // before the export. A node that never starts a pod is the one an operator
    // most needs a record from.
    let dir = temp_dir("failed-telemetry");
    let release = TestRelease::new(&dir);
    let bundle = dir.join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    // An issuer node policy does not allow: refused during selection, so the
    // failure lands before any release identity exists to name the event for.
    write_config(
        &bundle,
        serde_json::json!({
            "imageless.run/release-v1": release.reference.replacen("test/", "other/", 1)
        }),
    );
    let nix = dir.join("nix");
    fake_nix(&nix, "true");
    let delegate = dir.join("delegate");
    let called = dir.join("called");
    executable(&delegate, "touch \"$CALLED\"");
    let telemetry = dir.join("timings.jsonl");
    let resolver = ResolverProcess::start(&dir, &nix, &release.policy);
    let output = runc(&bundle, &delegate, &resolver.socket)
        .env("IMAGELESS_TELEMETRY_PATH", &telemetry)
        .env("CALLED", &called)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!called.exists(), "a refused create must not reach runc");

    let events = std::fs::read_to_string(telemetry)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["schema"], "imageless.timing.v1");
    assert_eq!(events[0]["outcome"], "error");
    assert_eq!(events[0]["stage"], "preparation");
    // No release was selected, so naming one would be a claim the run cannot
    // support.
    assert_eq!(events[0]["release"], "unresolved");
    // The duration is the half that distinguishes a fast refusal from a create
    // that burned its whole deadline.
    assert!(events[0]["duration_us"].is_u64());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Telemetry is node-owned; a pod that will not start says `CreateContainerError`
/// and the reason lives somewhere its owner cannot read. Unit tests cover the
/// record's shape and its curation — this one proves the shim's argv parsing and
/// the library's writer actually meet, over the real binary, with the flags
/// containerd passes.
#[test]
fn a_failed_create_relays_its_reason_into_the_runtime_log() {
    let dir = temp_dir("failed-relay");
    let release = TestRelease::new(&dir);
    let bundle = dir.join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    write_config(
        &bundle,
        serde_json::json!({
            "imageless.run/release-v1": release.reference.replacen("test/", "other/", 1)
        }),
    );
    let nix = dir.join("nix");
    fake_nix(&nix, "true");
    let delegate = dir.join("delegate");
    let called = dir.join("called");
    executable(&delegate, "touch \"$CALLED\"");
    let log = dir.join("runtime.log");
    let resolver = ResolverProcess::start(&dir, &nix, &release.policy);

    let output = Command::new(runc_binary())
        .env("IMAGELESS_RUNC", &delegate)
        .env("IMAGELESS_RESOLVER_SOCKET", &resolver.socket)
        .env("CALLED", &called)
        .args(["--root", "/run/runc-test", "--log"])
        .arg(&log)
        .args(["--log-format", "json", "create", "--bundle"])
        .arg(&bundle)
        .arg("integration-test")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!called.exists(), "a refused create must not reach runc");

    let records = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["level"], "error");
    let message = records[0]["msg"].as_str().unwrap();
    // containerd renders this into a pod event, so it has to stay one line and
    // it has to say which component refused before it says why.
    assert!(message.starts_with("imageless: "), "{message}");
    assert!(!message.contains('\n'), "{message}");

    // The category is the same token the telemetry sink records, so an operator
    // correlating a pod event with a node timing record matches strings by eye.
    let category = message
        .trim_start_matches("imageless: ")
        .split_once(": ")
        .expect("a category precedes the diagnostic")
        .0
        .to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("{}:", to_camel(&category))),
        "log said {category}, stderr said {stderr}"
    );

    // Nothing in the relayed line is node configuration the tenant cannot act
    // on — that half of a diagnostic stops at the curation boundary.
    assert!(!message.contains("/etc/imageless"), "{message}");
    std::fs::remove_dir_all(dir).unwrap();
}

/// `policy_denied` as `ResolutionError`'s `Debug` spells it, for the one
/// assertion that has to hold both spellings against each other.
fn to_camel(snake: &str) -> String {
    snake
        .split('_')
        .map(|word| {
            let mut characters = word.chars();
            match characters.next() {
                Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

#[test]
fn release_digest_policy_and_architecture_errors_are_categorized() {
    for (label, configure, expected) in [
        ("digest", "digest", "DigestMismatch"),
        ("architecture", "architecture", "ArchitectureMismatch"),
        ("policy", "policy", "PolicyDenied"),
    ] {
        let dir = temp_dir(label);
        let release = TestRelease::new(&dir);
        if configure == "digest" {
            let digest = release.reference.rsplit_once("@sha256:").unwrap().1;
            std::fs::write(
                dir.join("catalog/sha256").join(format!("{digest}.json")),
                b"{}",
            )
            .unwrap();
        } else if configure == "architecture" {
            let mut policy: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&release.policy).unwrap()).unwrap();
            policy["system"] = serde_json::Value::String("aarch64-linux".to_string());
            std::fs::write(&release.policy, serde_json::to_vec(&policy).unwrap()).unwrap();
        }

        let bundle = dir.join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        let annotations = if configure == "policy" {
            serde_json::json!({
                "imageless.run/release-v1": release.reference.replacen("test/", "other/", 1)
            })
        } else {
            release.annotations()
        };
        write_config(&bundle, annotations);
        let nix = dir.join("nix");
        fake_nix(&nix, "true");
        let resolver = ResolverProcess::start(&dir, &nix, &release.policy);
        let delegate = dir.join("delegate");
        let called = dir.join("called");
        executable(&delegate, "touch \"$CALLED\"");
        let output = runc(&bundle, &delegate, &resolver.socket)
            .env("CALLED", &called)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "expected {expected}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!called.exists());
        assert!(!bundle.join(imageless::GC_ROOT_NAME).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn missing_resolver_fails_selected_workload_but_not_sandbox_or_passthrough() {
    let dir = temp_dir("fail-closed");
    let release = TestRelease::new(&dir);
    let delegate = dir.join("delegate");
    executable(&delegate, "exit 0");
    let missing = dir.join("missing.sock");

    let selected = dir.join("selected");
    std::fs::create_dir(&selected).unwrap();
    write_config(&selected, release.annotations());
    let output = runc(&selected, &delegate, &missing).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("resolver is unavailable"));

    for (name, annotations) in [
        ("plain", serde_json::json!({})),
        (
            "sandbox",
            serde_json::json!({
                "imageless.run/release-v1": release.reference,
                "io.kubernetes.cri.container-type": "sandbox"
            }),
        ),
        (
            "sidecar",
            serde_json::json!({
                "imageless.run/release-v1": release.reference,
                "imageless.run/containers-v1": "agent",
                "io.kubernetes.cri.container-name": "sidecar"
            }),
        ),
    ] {
        let bundle = dir.join(name);
        std::fs::create_dir(&bundle).unwrap();
        let config = write_config(&bundle, annotations);
        assert_success(&runc(&bundle, &delegate, &missing).output().unwrap());
        assert_eq!(root_path(&config), "rootfs");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn delegate_or_rewrite_failure_removes_new_root() {
    let dir = temp_dir("cleanup-failure");
    let release = TestRelease::new(&dir);
    let nix = dir.join("nix");
    fake_nix(&nix, "true");
    let resolver = ResolverProcess::start(&dir, &nix, &release.policy);
    let failing_delegate = dir.join("delegate-fail");
    executable(&failing_delegate, "exit 42");

    let delegated = dir.join("delegated");
    std::fs::create_dir(&delegated).unwrap();
    let config = write_config(&delegated, release.annotations());
    let engine_config = std::fs::read(&config).unwrap();
    let output = runc(&delegated, &failing_delegate, &resolver.socket)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42));
    assert!(!delegated.join(imageless::GC_ROOT_NAME).exists());
    assert!(!delegated.join(imageless::GC_ROOTS_DIR_NAME).exists());
    // The engine's config is back, byte for byte, so a create retried on the
    // same bundle plans against the image, not against our rewrite.
    assert_eq!(std::fs::read(&config).unwrap(), engine_config);
    let succeeding_delegate = dir.join("delegate-ok");
    executable(&succeeding_delegate, "exit 0");
    let retried = runc(&delegated, &succeeding_delegate, &resolver.socket)
        .output()
        .unwrap();
    assert_success(&retried);
    assert_eq!(root_path(&config), STORE_PATH);

    let rewrite = dir.join("rewrite");
    std::fs::create_dir(&rewrite).unwrap();
    let document = serde_json::json!({
        "ociVersion": "1.2.0",
        "annotations": { "imageless.run/release-v1": release.reference }
    });
    std::fs::write(
        rewrite.join("config.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    let output = runc(&rewrite, &failing_delegate, &resolver.socket)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!rewrite.join(imageless::GC_ROOT_NAME).exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn root_collision_is_categorized_and_never_calls_delegate() {
    let dir = temp_dir("collision");
    let release = TestRelease::new(&dir);
    let bundle = dir.join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    write_config(&bundle, release.annotations());
    std::fs::write(bundle.join(imageless::GC_ROOT_NAME), "reserved").unwrap();
    let nix = dir.join("nix");
    fake_nix(&nix, "true");
    let resolver = ResolverProcess::start(&dir, &nix, &release.policy);
    let delegate = dir.join("delegate");
    let called = dir.join("called");
    executable(&delegate, "touch \"$CALLED\"");
    let output = runc(&bundle, &delegate, &resolver.socket)
        .env("CALLED", &called)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("RootCollision"));
    assert!(!called.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn nix_store_mount_collision_fails_closed_and_removes_roots() {
    let dir = temp_dir("store-projection-collision");
    let release = TestRelease::new(&dir);
    let bundle = dir.join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    let config = write_config(&bundle, release.annotations());
    let mut document = config_document(&config);
    document["mounts"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "destination": "/nix/store",
            "source": "/workload-controlled",
            "type": "bind"
        }));
    std::fs::write(&config, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    let nix = dir.join("nix");
    fake_nix(&nix, "true");
    let resolver = ResolverProcess::start(&dir, &nix, &release.policy);
    let delegate = dir.join("delegate");
    let called = dir.join("called");
    executable(&delegate, "touch \"$CALLED\"");
    let output = runc(&bundle, &delegate, &resolver.socket)
        .env("CALLED", &called)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("SpecConflict"));
    assert!(!called.exists());
    assert!(!bundle.join(imageless::GC_ROOT_NAME).exists());
    assert!(!bundle.join(imageless::GC_ROOTS_DIR_NAME).exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn killing_resolver_kills_materializer_and_client_fails_closed() {
    let dir = temp_dir("resolver-death");
    let release = TestRelease::new(&dir);
    let bundle = dir.join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    write_config(&bundle, release.annotations());
    let pid_file = dir.join("materializer.pid");
    let nix = dir.join("nix");
    executable(&nix, "printf '%s' \"$$\" > \"$PID_FILE\"; sleep 30");
    let mut resolver =
        ResolverProcess::start_with_env(&dir, &nix, &release.policy, &[("PID_FILE", &pid_file)]);
    let delegate = dir.join("delegate");
    executable(&delegate, "exit 0");
    let mut client = runc(&bundle, &delegate, &resolver.socket)
        .env("IMAGELESS_REALIZATION_TIMEOUT_SECONDS", "30")
        .env("PID_FILE", &pid_file)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !pid_file.exists() {
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(10));
    }
    let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    resolver.kill();

    let started = Instant::now();
    let status = loop {
        if let Some(status) = client.try_wait().unwrap() {
            break status;
        }
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
    let started = Instant::now();
    loop {
        let exists = unsafe { libc::kill(pid, 0) } == 0;
        let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .is_some_and(|stat| {
                stat.rsplit_once(") ")
                    .is_some_and(|(_, fields)| fields.starts_with("Z "))
            });
        if !exists || zombie {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(10));
    }
    std::fs::remove_dir_all(dir).unwrap();
}
