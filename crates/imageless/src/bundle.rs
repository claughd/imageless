//! Atomic OCI bundle rewrite and the client-side resolve-and-apply seam.

use crate::client::{effective_uid, request_resolution_detailed};
use crate::gc::remove_bundle_gc_roots;
use crate::layer::{RootLayer, DEFAULT_ROOT_LAYER_DIRECTORY, ROOT_LAYER_DIRECTORY_ENV};
use crate::materialize::{
    elapsed_us, ErrorCategory, ResolutionError, ResolutionSuccess, ResolveRequest,
};
use crate::mounts::{apply_node_store_projection, apply_store_mounts, store_projection_for};
use crate::release::{ProcessMetadata, ResolvedRelease};
use crate::resolver::{resolve_in_process, PolicySource};
use crate::spec::expansion_request;
use crate::{to_io, StoreProjection};
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How a caller reaches materialization: through the node resolver daemon's
/// UNIX socket, or in-process under a node policy. In-process materialization
/// runs as the calling user and provides no cross-process single-flight; the
/// daemon remains the multi-tenant deployment shape. The in-process policy may
/// come from a file or inline from the daemon environment — see [`PolicySource`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaterializerConfig {
    Socket(PathBuf),
    InProcess(PolicySource),
}

impl MaterializerConfig {
    /// Environment-based selection for adapter binaries, in precedence order:
    /// a non-empty `IMAGELESS_RESOLVER_SOCKET` selects the daemon socket; else,
    /// when built with the `inline-policy` feature, a non-empty
    /// `IMAGELESS_POLICY_JSON` supplies the policy inline (no file to own,
    /// trusted because the daemon operator set it); else materialization reads
    /// the policy file named by `IMAGELESS_POLICY` (default
    /// `/etc/imageless/policy.json`). Without the feature the inline path is not
    /// compiled in, so a production binary can only ever load a policy file.
    pub fn from_environment() -> Self {
        if let Ok(socket) = std::env::var("IMAGELESS_RESOLVER_SOCKET") {
            if !socket.is_empty() {
                return Self::Socket(PathBuf::from(socket));
            }
        }
        #[cfg(feature = "inline-policy")]
        if let Some(json) = std::env::var("IMAGELESS_POLICY_JSON")
            .ok()
            .filter(|value| !value.is_empty())
        {
            return Self::InProcess(PolicySource::Inline(json));
        }
        Self::InProcess(PolicySource::File(
            std::env::var_os("IMAGELESS_POLICY")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        ))
    }

    fn resolve(&self, request: &ResolveRequest) -> Result<ResolutionSuccess, ResolutionError> {
        match self {
            Self::Socket(socket_path) => request_resolution_detailed(socket_path, request),
            Self::InProcess(source) => resolve_in_process(source, request),
        }
    }
}

/// One `create`-time preparation request: where the bundle lives and how to
/// resolve it. Construct with [`PrepareBundle::new`] (the contract defaults)
/// and override fields as deployment configuration requires. Non-exhaustive so
/// later revisions can add knobs without breaking embedders.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PrepareBundle {
    pub config_path: PathBuf,
    pub bundle_path: PathBuf,
    /// Flake output attribute used when the image names none.
    pub default_output: String,
    /// Materialization deadline in seconds (SPEC §4.3, 1–3600).
    pub timeout_seconds: u64,
    pub materializer: MaterializerConfig,
    /// The runtime log the calling OCI runtime was given (`runc --log`), when
    /// it was also given `--log-format json`. containerd reads this file back
    /// when the runtime fails, which is the one channel by which a create
    /// failure becomes an event on the pod instead of a line the workload's
    /// owner can never see. `None` disables the relay, and is the default:
    /// nothing is written unless a caller opts in with a path it parsed itself.
    pub runtime_log: Option<PathBuf>,
    /// Where the writable mountpoint layer over the materialized root is
    /// staged (SPEC §4.5); `None` hands the store path to the runtime as the
    /// root, which is only sound where the store is mounted read-only and the
    /// root already holds every mountpoint the container needs.
    pub root_layers: Option<PathBuf>,
}

/// `IMAGELESS_ROOT_LAYERS`: unset for the default staging directory, `none`
/// to hand the runtime the store path itself, or another absolute directory.
pub fn root_layers_from_environment() -> Option<PathBuf> {
    match std::env::var_os(ROOT_LAYER_DIRECTORY_ENV) {
        None => Some(PathBuf::from(DEFAULT_ROOT_LAYER_DIRECTORY)),
        Some(value) if value.is_empty() => Some(PathBuf::from(DEFAULT_ROOT_LAYER_DIRECTORY)),
        Some(value) if value == "none" => None,
        Some(value) => Some(PathBuf::from(value)),
    }
}

impl PrepareBundle {
    /// Contract defaults: output `rootfs`, a 300 s deadline, and the
    /// environment-selected materializer.
    pub fn new(config_path: impl Into<PathBuf>, bundle_path: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            bundle_path: bundle_path.into(),
            default_output: "rootfs".to_string(),
            timeout_seconds: 300,
            materializer: MaterializerConfig::from_environment(),
            runtime_log: None,
            root_layers: root_layers_from_environment(),
        }
    }
}

/// Bound on one relayed record. A pod event is read by a person in a terminal,
/// and containerd carries the runtime's message into an event whose own limits
/// are not ours to discover the hard way; a materialization diagnostic already
/// arrives bounded (the Nix excerpt is capped upstream), so this is a backstop
/// rather than the usual case.
const MAX_RELAY_BYTES: usize = 2048;

/// A logrus record in the shape `runc` writes and containerd's `go-runc` parses.
/// `time` is deliberately absent: the parser treats it as optional, containerd
/// stamps its own event, and a second clock in the line is one more thing that
/// can disagree with the first.
#[derive(Serialize)]
struct RuntimeLogRecord<'a> {
    level: &'static str,
    msg: &'a str,
}

/// Trim a diagnostic to the half its reader can act on, and flatten it onto one
/// line. See [`crate::resolver::OPERATOR_HINT`] for where the cut falls and why.
fn tenant_diagnostic(diagnostic: &str) -> String {
    let curated = match diagnostic.find(crate::resolver::OPERATOR_HINT) {
        Some(cut) => diagnostic[..cut].trim_end(),
        None => diagnostic,
    };
    // A pod event is one line in `kubectl describe`; a captured Nix tail is
    // many. Collapsing runs of whitespace keeps a multi-line excerpt readable
    // where it lands rather than where it was produced.
    //
    // Control bytes are dropped rather than escaped. JSON would carry them
    // safely enough, but containerd decodes this back out and a person reads
    // the result in a terminal — a build log excerpt is not a place to let ANSI
    // escapes through. `nix.rs` already strips them from its excerpts; a
    // diagnostic can be built anywhere, and this is the seam that leaves.
    let flattened: String = curated
        .chars()
        .map(|character| {
            if character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .filter(|character| !character.is_control())
        .collect();
    flattened.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Relay a terminal create failure into the runtime log, best-effort.
///
/// Writing is never allowed to change the outcome of a create that has already
/// failed, so every error here is dropped: the caller is on its way to exiting
/// non-zero regardless, and a telemetry-shaped side channel that can turn a
/// clear failure into a confusing one is worse than no side channel.
fn relay_failure(log: &Path, category: &ErrorCategory, diagnostic: &str) {
    // The path comes from the runtime's own argv rather than from the image, so
    // this guards against a malformed invocation, not against a tenant. The
    // `O_NOFOLLOW` below is the part that matters: it refuses to be redirected
    // through a symlink into a file the relay was never pointed at.
    if !log.is_absolute() {
        return;
    }
    let curated = tenant_diagnostic(diagnostic);
    let mut message = format!("imageless: {}: {curated}", category.as_str());
    if message.len() > MAX_RELAY_BYTES {
        let mut cut = MAX_RELAY_BYTES - 3;
        while !message.is_char_boundary(cut) {
            cut -= 1;
        }
        message.truncate(cut);
        message.push_str("...");
    }
    let Ok(mut record) = serde_json::to_vec(&RuntimeLogRecord {
        level: "error",
        msg: &message,
    }) else {
        return;
    };
    record.push(b'\n');
    let opened = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(log);
    if let Ok(mut file) = opened {
        let _ = file.write_all(&record);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleTimings {
    pub selection_us: u64,
    pub policy_verification_us: u64,
    pub substitution_us: u64,
    pub rewrite_us: u64,
    /// Carved out of `policy_verification_us` by the materializer: fetching the
    /// release manifest, which is a network round trip on an HTTPS issuer.
    pub manifest_fetch_us: u64,
    /// Carved out of `substitution_us`: copying an embedded development source
    /// out of the image's rootfs. Zero for a release.
    pub staging_us: u64,
    /// Carved out of `substitution_us`: the Nix process itself.
    pub evaluation_us: u64,
    /// Carved out of `substitution_us`: registering the GC root. For a caller
    /// that joined another's in-flight materialization, this is the whole of
    /// its own Nix cost.
    pub root_registration_us: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedResolution {
    pub resolution: ResolvedRelease,
    pub timings: BundleTimings,
    /// The mountpoint layer `root.path` now names. Once the runtime has
    /// returned, the caller hands it to the container with
    /// [`RootLayer::commit`], which releases it at once if no container was
    /// made (SPEC §4.5).
    pub root_layer: Option<RootLayer>,
}

/// Prepare an OCI bundle at `create`: select or pass through, materialize
/// through the configured materializer, and atomically apply the returned
/// image-like metadata. `Ok(None)` is passthrough — not an imageless bundle,
/// nothing touched. Both a standalone adapter and an embedding OCI runtime
/// call this seam; a socket-backed caller never gains authority to execute
/// Nix.
pub fn prepare_bundle(prepare: &PrepareBundle) -> io::Result<Option<AppliedResolution>> {
    let selection_started = Instant::now();
    let request = match expansion_request(
        &prepare.config_path,
        &prepare.bundle_path,
        &prepare.default_output,
        prepare.timeout_seconds,
    ) {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(None),
        Err(error) => {
            // `plan` is the only source of `InvalidInput` here, and it can only
            // fail once the annotations have already identified the bundle as
            // imageless: a `run.imageless.*` source alongside an
            // `imageless.run/*` release (SPEC §3), an unparsable reference, an
            // output attribute that is not a name. That is the most fixable
            // failure a tenant can hit — it is in the pod spec they wrote —
            // and the one they were least able to see.
            //
            // Every other kind here is the node's own: a config.json that will
            // not parse, a rootfs that will not stat. Those name node paths,
            // the tenant cannot act on them, and reaching this arm does not
            // even establish that the bundle was imageless.
            if error.kind() == io::ErrorKind::InvalidInput {
                if let Some(log) = &prepare.runtime_log {
                    relay_failure(log, &ErrorCategory::SpecConflict, &error.to_string());
                }
            }
            return Err(error);
        }
    };
    let selection_us = elapsed_us(selection_started);
    // Everything below has passed the passthrough gate above, so the relay can
    // only ever touch a bundle this runtime was asked to expand. A bundle that
    // is not imageless returned already, having had nothing written on its
    // behalf — SPEC §4.1 makes passthrough a strict no-op, and a stray log line
    // would be an observable one.
    let success = prepare.materializer.resolve(&request).map_err(|error| {
        if let Some(log) = &prepare.runtime_log {
            relay_failure(log, &error.category, &error.diagnostic);
        }
        io::Error::other(format!("resolution failed: {error}"))
    })?;
    let rewrite_started = Instant::now();
    let root_layer = match &prepare.root_layers {
        Some(staging) if !staging.is_absolute() => {
            let _ = remove_bundle_gc_roots(&prepare.bundle_path);
            return Err(io::Error::other(format!(
                "{ROOT_LAYER_DIRECTORY_ENV} must be an absolute directory or `none`"
            )));
        }
        Some(staging) => match RootLayer::mount(Path::new(&success.resolution.rootfs), staging) {
            Ok(layer) => Some(layer),
            Err(error) => {
                let _ = remove_bundle_gc_roots(&prepare.bundle_path);
                if let Some(log) = &prepare.runtime_log {
                    relay_failure(
                        log,
                        &ErrorCategory::Internal,
                        "the node could not assemble the container root over its store path",
                    );
                }
                return Err(io::Error::other(format!(
                    "{:?}: could not mount the root layer under {}: {error}",
                    ErrorCategory::Internal,
                    staging.display()
                )));
            }
        },
        None => None,
    };
    let root_path = root_layer.as_ref().map_or_else(
        || success.resolution.rootfs.clone(),
        |layer| layer.root().to_string_lossy().into_owned(),
    );
    let applied = store_projection_for(&success.resolution).and_then(|projection| {
        apply_resolution_rooted(
            &prepare.config_path,
            &success.resolution,
            &projection,
            &root_path,
        )
    });
    if let Err(error) = applied {
        if let Some(layer) = root_layer {
            layer.detach();
        }
        let _ = remove_bundle_gc_roots(&prepare.bundle_path);
        let diagnostic = error.to_string();
        if let Some(log) = &prepare.runtime_log {
            relay_failure(log, &ErrorCategory::SpecConflict, &diagnostic);
        }
        return Err(io::Error::new(
            error.kind(),
            format!("{:?}: {diagnostic}", ErrorCategory::SpecConflict),
        ));
    }
    Ok(Some(AppliedResolution {
        resolution: success.resolution,
        timings: BundleTimings {
            selection_us,
            policy_verification_us: success.timings.policy_verification_us,
            substitution_us: success.timings.substitution_us,
            rewrite_us: elapsed_us(rewrite_started),
            manifest_fetch_us: success.timings.manifest_fetch_us,
            staging_us: success.timings.staging_us,
            evaluation_us: success.timings.evaluation_us,
            root_registration_us: success.timings.root_registration_us,
        },
        root_layer,
    }))
}

#[derive(Serialize)]
struct TimingEvent<'a> {
    schema: &'static str,
    release: &'a str,
    stage: &'a str,
    duration_us: u64,
    timestamp_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'a str>,
}

/// Append timing events to a node-owned JSON-lines sink without touching the
/// adapter's inherited stdio streams. The configured path is deployment policy;
/// callers should treat telemetry failure as non-fatal to workload startup.
pub fn export_timing_events(
    path: &Path,
    release: &str,
    stages: &[(&str, u64)],
    outcome: Option<&str>,
) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "telemetry path must be absolute",
        ));
    }
    let timestamp_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    for (stage, duration_us) in stages {
        serde_json::to_writer(
            &mut bytes,
            &TimingEvent {
                schema: "imageless.timing.v1",
                release,
                stage,
                duration_us: *duration_us,
                timestamp_unix_ms,
                outcome,
            },
        )
        .map_err(to_io)?;
        bytes.push(b'\n');
    }

    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != effective_uid() || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "telemetry sink must be a daemon-owned regular file that is not group/world writable",
        ));
    }
    file.write_all(&bytes)
}

pub fn apply_resolution(config_path: &Path, resolution: &ResolvedRelease) -> io::Result<()> {
    let projection = store_projection_for(resolution)?;
    apply_resolution_with_projection(config_path, resolution, &projection)
}

/// Bundle rewrite with an explicit store-projection backend. [`apply_resolution`]
/// selects the backend from node configuration (`IMAGELESS_STORE_PROJECTION`);
/// tests drive the backend directly.
pub fn apply_resolution_with_projection(
    config_path: &Path,
    resolution: &ResolvedRelease,
    projection: &StoreProjection,
) -> io::Result<()> {
    apply_resolution_rooted(config_path, resolution, projection, &resolution.rootfs)
}

/// The rewrite with `root.path` set to `root_path`: the store path itself, or
/// the mountpoint layer over it.
fn apply_resolution_rooted(
    config_path: &Path,
    resolution: &ResolvedRelease,
    projection: &StoreProjection,
    root_path: &str,
) -> io::Result<()> {
    let text = std::fs::read_to_string(config_path)?;
    let permissions = std::fs::metadata(config_path)?.permissions();
    let mut document: serde_json::Value = serde_json::from_str(&text).map_err(to_io)?;
    let root = document
        .get_mut("root")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| io::Error::other("config.json has no `root` object to rewrite"))?;
    root.insert(
        "path".into(),
        serde_json::Value::String(root_path.to_string()),
    );
    // The new root is a store path shared with every other container and the
    // node itself (SPEC §4.4), or an overlay whose only lower layer is one: a
    // writable root would let one workload rewrite what all of them run,
    // wherever the node's /nix/store is not itself mounted read-only.
    root.insert("readonly".into(), serde_json::Value::Bool(true));

    if let Some(metadata) = &resolution.process {
        apply_process_metadata(&mut document, metadata)?;
    }
    apply_store_mounts(&mut document, &resolution.mounts)?;
    apply_node_store_projection(&mut document, projection)?;

    write_config_atomically(config_path, permissions, &document)
}

/// Compatibility helper for callers that only replace a rootfs. Production
/// release expansion should use [`apply_resolution`] so image metadata is not
/// silently discarded.
pub fn rewrite_root_path(config_path: &Path, store_path: &str) -> io::Result<()> {
    apply_resolution(
        config_path,
        &ResolvedRelease {
            identity: "rootfs-only".to_string(),
            rootfs: store_path.to_string(),
            process: None,
            mounts: Vec::new(),
        },
    )
}

fn apply_process_metadata(
    document: &mut serde_json::Value,
    metadata: &ProcessMetadata,
) -> io::Result<()> {
    let process = document
        .get_mut("process")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| io::Error::other("config.json has no `process` object to rewrite"))?;

    if metadata.entrypoint.is_some() || metadata.default_args.is_some() {
        let existing = process
            .get("args")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .map(|value| {
                        value.as_str().map(str::to_owned).ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "config.json process.args contains a non-string",
                            )
                        })
                    })
                    .collect::<io::Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        let mut args = match &metadata.entrypoint {
            Some(entrypoint) => entrypoint.clone(),
            None => existing.first().cloned().into_iter().collect(),
        };
        if let Some(default_args) = &metadata.default_args {
            args.extend(default_args.clone());
        }
        if args.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "release process metadata would produce an empty process.args",
            ));
        }
        process.insert(
            "args".to_string(),
            serde_json::Value::Array(args.into_iter().map(serde_json::Value::String).collect()),
        );
    }

    if let Some(cwd) = &metadata.working_directory {
        process.insert("cwd".to_string(), serde_json::Value::String(cwd.clone()));
    }

    if !metadata.environment.is_empty() {
        let environment = process
            .entry("env")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "config.json process.env is not an array",
                )
            })?;
        let mut positions = HashMap::new();
        for (index, value) in environment.iter().enumerate() {
            let value = value.as_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "config.json process.env contains a non-string",
                )
            })?;
            if let Some((name, _)) = value.split_once('=') {
                positions.insert(name.to_string(), index);
            }
        }
        for entry in &metadata.environment {
            if let Some(index) = positions.get(&entry.name).copied() {
                if !entry.allow_workload_override {
                    environment[index] =
                        serde_json::Value::String(format!("{}={}", entry.name, entry.value));
                }
            } else {
                positions.insert(entry.name.clone(), environment.len());
                environment.push(serde_json::Value::String(format!(
                    "{}={}",
                    entry.name, entry.value
                )));
            }
        }
    }
    Ok(())
}

fn write_config_atomically(
    config_path: &Path,
    permissions: std::fs::Permissions,
    document: &serde_json::Value,
) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&document).map_err(to_io)?;
    let (temporary, mut file) = create_sibling_temp(config_path)?;
    let result = (|| {
        file.set_permissions(permissions)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, config_path)?;
        let parent = config_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn create_sibling_temp(config_path: &Path) -> io::Result<(PathBuf, File)> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let parent = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    for _ in 0..128 {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{name}.imageless-{}-{nonce}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate config.json temporary file",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::normalized_oci_destination;
    use crate::release::{EnvironmentEntry, StoreMount};
    use crate::testutil::{temporary, STORE};
    use crate::NIX_STORE_PATH;
    use std::os::unix::fs::PermissionsExt;

    fn relayed_lines(log: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| {
                serde_json::from_str(line).expect("containerd parses this file line by line")
            })
            .collect()
    }

    /// The point of the relay: a create that failed for a reason the workload's
    /// owner can act on says so where they can read it, in the shape
    /// containerd's log parser expects.
    #[test]
    fn a_relayed_failure_is_one_json_error_record() {
        let directory = temporary("relay-shape");
        let log = directory.join("runtime.log");
        relay_failure(
            &log,
            &ErrorCategory::ArchitectureMismatch,
            "release targets aarch64-linux; this node is x86_64-linux",
        );

        let lines = relayed_lines(&log);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["level"], "error");
        assert_eq!(
            lines[0]["msg"],
            "imageless: architecture_mismatch: release targets aarch64-linux; this node is x86_64-linux"
        );
        // containerd stamps its own event; a second clock is one more thing
        // that can disagree with the first.
        assert!(lines[0].get("time").is_none());
    }

    /// The curation dylan asked for. A tenant reading `kubectl describe pod`
    /// learns why their pod will not start; they do not learn which file on
    /// the node an operator would have to write to change that.
    #[test]
    fn a_policy_refusal_relays_its_reason_without_the_nodes_configuration_path() {
        let directory = temporary("relay-curation");
        let log = directory.join("runtime.log");
        let refusal = crate::resolver::default_policy_hint(
            ResolutionError::new(
                ErrorCategory::EvaluationDisabled,
                "development sources are not permitted",
                false,
            ),
            Path::new("/etc/imageless/policy.json"),
        );
        // The operator-facing text is genuinely there before curation, or this
        // test would pass against a diagnostic that never carried a path.
        assert!(refusal.diagnostic.contains("/etc/imageless/policy.json"));

        relay_failure(&log, &refusal.category, &refusal.diagnostic);

        let message = relayed_lines(&log)[0]["msg"].as_str().unwrap().to_string();
        assert_eq!(
            message,
            "imageless: evaluation_disabled: development sources are not permitted"
        );
        assert!(!message.contains("/etc/imageless"));
        assert!(!message.contains("node policy does not permit"));
    }

    /// A materialization diagnostic can carry a captured Nix tail, which is
    /// many lines and can be adversarial. A pod event is one line, and it is
    /// not a place to discover containerd's own limits.
    #[test]
    fn a_hostile_diagnostic_relays_as_one_bounded_line() {
        let directory = temporary("relay-bounds");
        let log = directory.join("runtime.log");
        let hostile = format!(
            "nix said:\n{}\n\u{7}\u{1b}[31m",
            "A".repeat(MAX_RELAY_BYTES * 4)
        );
        relay_failure(&log, &ErrorCategory::Materialization, &hostile);

        let lines = relayed_lines(&log);
        assert_eq!(lines.len(), 1, "the record must not become several");
        let message = lines[0]["msg"].as_str().unwrap();
        assert!(message.len() <= MAX_RELAY_BYTES, "{}", message.len());
        assert!(!message.contains('\n'));
        assert!(message.ends_with("..."));
        // containerd decodes this back out and a person reads it in a terminal.
        assert!(!message.chars().any(char::is_control), "{message}");
    }

    /// The runtime hands over this path; `O_NOFOLLOW` is what keeps a stale or
    /// malformed invocation from writing through a symlink into a file the
    /// relay was never pointed at.
    #[test]
    fn the_relay_refuses_a_symlinked_log_and_an_unanchored_one() {
        let directory = temporary("relay-refusals");
        let target = directory.join("elsewhere");
        std::fs::write(&target, b"untouched").unwrap();
        let link = directory.join("runtime.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        relay_failure(&link, &ErrorCategory::Internal, "a diagnostic");
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");

        // A relative path is a malformed invocation, not a location to guess at
        // from whatever directory the runtime happened to start in.
        relay_failure(
            Path::new("runtime.log"),
            &ErrorCategory::Internal,
            "a diagnostic",
        );
        assert!(!Path::new("runtime.log").exists());
    }

    /// The failure a tenant is most able to fix, because it is in the pod spec
    /// they wrote: SPEC §3 makes the two annotation families mutually
    /// exclusive. It fails in `expansion_request`, upstream of the materializer
    /// the other relay sites sit behind, so it needs its own arm — and until
    /// this one existed it was the loudest thing the tenant could not see.
    #[test]
    fn contradictory_annotations_relay_before_a_materializer_is_ever_reached() {
        let directory = temporary("relay-spec-conflict");
        let bundle = directory.join("bundle");
        std::fs::create_dir_all(bundle.join("rootfs")).unwrap();
        let config = bundle.join("config.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "ociVersion": "1.0.2",
                "root": { "path": "rootfs" },
                "annotations": {
                    "imageless.run/release-v1": "example/agent@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                    "run.imageless.source": "/source"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let log = directory.join("runtime.log");

        let mut prepare = PrepareBundle::new(&config, &bundle);
        // Reaching a materializer at all would mean the conflict went unnoticed.
        prepare.materializer =
            MaterializerConfig::Socket(PathBuf::from("/nonexistent/imageless.sock"));
        prepare.runtime_log = Some(log.clone());

        let error = prepare_bundle(&prepare).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let message = relayed_lines(&log)[0]["msg"].as_str().unwrap().to_string();
        assert!(
            message.starts_with("imageless: spec_conflict: "),
            "{message}"
        );
    }

    /// The other half of that arm: a `config.json` that will not parse is the
    /// node's problem, names node paths, and does not even establish that the
    /// bundle was imageless. It must not reach a tenant-visible channel.
    #[test]
    fn an_unreadable_config_is_not_relayed_to_the_tenant() {
        let directory = temporary("relay-unreadable");
        let bundle = directory.join("bundle");
        std::fs::create_dir_all(bundle.join("rootfs")).unwrap();
        let config = bundle.join("config.json");
        std::fs::write(&config, b"{ this is not json").unwrap();
        let log = directory.join("runtime.log");

        let mut prepare = PrepareBundle::new(&config, &bundle);
        prepare.runtime_log = Some(log.clone());

        let error = prepare_bundle(&prepare).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            !log.exists(),
            "a node-side parse failure is not tenant news"
        );
    }

    /// SPEC §4.1 makes passthrough a strict no-op, and a log line is
    /// observable. A bundle that is not imageless returns before the relay can
    /// reach it — asserted here against the seam rather than the helper.
    #[test]
    fn a_passthrough_bundle_leaves_the_runtime_log_untouched() {
        let directory = temporary("relay-passthrough");
        let bundle = directory.join("bundle");
        std::fs::create_dir_all(bundle.join("rootfs")).unwrap();
        let config = bundle.join("config.json");
        std::fs::write(
            &config,
            br#"{"ociVersion":"1.0.2","root":{"path":"rootfs"},"annotations":{}}"#,
        )
        .unwrap();
        let log = directory.join("runtime.log");

        let mut prepare = PrepareBundle::new(&config, &bundle);
        // A materializer that cannot possibly succeed: reaching it at all would
        // be the bug this test exists to catch.
        prepare.materializer =
            MaterializerConfig::Socket(PathBuf::from("/nonexistent/imageless.sock"));
        prepare.runtime_log = Some(log.clone());

        assert!(prepare_bundle(&prepare).unwrap().is_none());
        assert!(!log.exists(), "passthrough must write nothing at all");
    }

    #[test]
    fn materializer_selection_follows_the_environment() {
        std::env::set_var("IMAGELESS_RESOLVER_SOCKET", "/run/imageless/test.sock");
        std::env::set_var("IMAGELESS_POLICY", "/etc/imageless/other-policy.json");
        assert_eq!(
            MaterializerConfig::from_environment(),
            MaterializerConfig::Socket(PathBuf::from("/run/imageless/test.sock"))
        );

        // An empty socket variable means daemonless, not "socket at ''".
        std::env::set_var("IMAGELESS_RESOLVER_SOCKET", "");
        assert_eq!(
            MaterializerConfig::from_environment(),
            MaterializerConfig::InProcess(PolicySource::File(Some(PathBuf::from(
                "/etc/imageless/other-policy.json"
            ))))
        );

        // Inline policy JSON takes precedence over a policy file path: no file
        // to own, trusted because it rides in the daemon environment. Only
        // compiled in with the `inline-policy` feature.
        #[cfg(feature = "inline-policy")]
        {
            std::env::set_var("IMAGELESS_POLICY_JSON", "{\"system\":\"x86_64-linux\"}");
            assert_eq!(
                MaterializerConfig::from_environment(),
                MaterializerConfig::InProcess(PolicySource::Inline(
                    "{\"system\":\"x86_64-linux\"}".to_string()
                ))
            );

            // A daemon socket still outranks inline policy.
            std::env::set_var("IMAGELESS_RESOLVER_SOCKET", "/run/imageless/test.sock");
            assert_eq!(
                MaterializerConfig::from_environment(),
                MaterializerConfig::Socket(PathBuf::from("/run/imageless/test.sock"))
            );
            std::env::remove_var("IMAGELESS_POLICY_JSON");
            std::env::remove_var("IMAGELESS_RESOLVER_SOCKET");
        }

        std::env::remove_var("IMAGELESS_RESOLVER_SOCKET");
        std::env::remove_var("IMAGELESS_POLICY");
        assert_eq!(
            MaterializerConfig::from_environment(),
            MaterializerConfig::InProcess(PolicySource::File(None))
        );
    }

    #[test]
    fn passthrough_bundles_never_consult_resolver_or_policy() {
        let dir = temporary("passthrough");
        let config = dir.join("config.json");
        std::fs::write(
            &config,
            r#"{"ociVersion":"1.2.0","root":{"path":"rootfs"},"process":{"args":["sh"]}}"#,
        )
        .unwrap();
        let before = std::fs::read(&config).unwrap();
        // A bundle with no imageless annotations and no embedded flake must
        // pass through even when the configured policy file cannot exist.
        let mut prepare = PrepareBundle::new(&config, &dir);
        prepare.timeout_seconds = 5;
        prepare.materializer = MaterializerConfig::InProcess(PolicySource::File(Some(
            PathBuf::from("/nonexistent/imageless-policy.json"),
        )));
        let result = prepare_bundle(&prepare).unwrap();
        assert!(result.is_none());
        assert_eq!(std::fs::read(&config).unwrap(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rewrite_forces_the_shared_store_root_read_only() {
        let dir = temporary("readonly-root");
        let config = dir.join("config.json");
        for root in [
            serde_json::json!({ "path": "rootfs" }),
            serde_json::json!({ "path": "rootfs", "readonly": false }),
        ] {
            std::fs::write(
                &config,
                serde_json::to_vec(&serde_json::json!({ "root": root })).unwrap(),
            )
            .unwrap();
            rewrite_root_path(&config, STORE).unwrap();
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
            assert_eq!(value["root"]["path"], STORE);
            assert_eq!(value["root"]["readonly"], true);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rewrite_preserves_fields_and_mode() {
        let dir = temporary("rewrite");
        let config = dir.join("config.json");
        std::fs::write(
            &config,
            r#"{"root":{"path":"rootfs","readonly":true},"unknown":{"keep":3}}"#,
        )
        .unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o640)).unwrap();
        rewrite_root_path(&config, STORE).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(value["root"]["path"], STORE);
        assert_eq!(value["root"]["readonly"], true);
        assert_eq!(value["unknown"]["keep"], 3);
        assert_eq!(
            std::fs::metadata(config).unwrap().permissions().mode() & 0o777,
            0o640
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn release_metadata_replaces_only_declared_image_fields() {
        assert!(normalized_oci_destination("/"));
        assert!(normalized_oci_destination("/opt/data"));
        for invalid in [
            "opt/data",
            "/opt//data",
            "/opt/./data",
            "/opt/../data",
            "/opt/data/",
        ] {
            assert!(!normalized_oci_destination(invalid));
        }

        let dir = temporary("release-metadata");
        let config = dir.join("config.json");
        let original = serde_json::json!({
            "ociVersion": "1.2.0",
            "root": { "path": "rootfs", "readonly": true },
            "process": {
                "args": ["placeholder", "workload-arg"],
                "cwd": "/original",
                "env": ["LOCKED=workload", "OPEN=workload", "KEEP=workload"]
            },
            "mounts": [{
                "destination": "/data",
                "source": "tmpfs",
                "type": "tmpfs"
            }],
            "linux": {
                "namespaces": [{ "type": "network" }],
                "resources": { "memory": { "limit": 1048576 } }
            }
        });
        std::fs::write(&config, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
        let resolution = ResolvedRelease {
            identity: "test/agent@sha256:00".to_string(),
            rootfs: STORE.to_string(),
            process: Some(ProcessMetadata {
                entrypoint: Some(vec!["/bin/agent".to_string()]),
                default_args: Some(vec!["serve".to_string()]),
                working_directory: Some("/workspace".to_string()),
                environment: vec![
                    EnvironmentEntry {
                        name: "LOCKED".to_string(),
                        value: "release".to_string(),
                        allow_workload_override: false,
                    },
                    EnvironmentEntry {
                        name: "OPEN".to_string(),
                        value: "release".to_string(),
                        allow_workload_override: true,
                    },
                    EnvironmentEntry {
                        name: "ADDED".to_string(),
                        value: "release".to_string(),
                        allow_workload_override: false,
                    },
                ],
            }),
            mounts: vec![StoreMount {
                source: "/nix/store/11111111111111111111111111111111-tools".to_string(),
                destination: "/opt/tools".to_string(),
            }],
        };
        apply_resolution(&config, &resolution).unwrap();
        let applied: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(applied["root"]["path"], STORE);
        assert_eq!(
            applied["process"]["args"],
            serde_json::json!(["/bin/agent", "serve"])
        );
        assert_eq!(applied["process"]["cwd"], "/workspace");
        assert_eq!(
            applied["process"]["env"],
            serde_json::json!([
                "LOCKED=release",
                "OPEN=workload",
                "KEEP=workload",
                "ADDED=release"
            ])
        );
        assert_eq!(applied["mounts"][0], original["mounts"][0]);
        assert_eq!(applied["mounts"][1]["destination"], "/opt/tools");
        assert_eq!(
            applied["mounts"][1]["options"],
            serde_json::json!(["rbind", "ro", "nosuid", "nodev"])
        );
        assert_eq!(
            applied["mounts"][2],
            serde_json::json!({
                "destination": NIX_STORE_PATH,
                "options": ["rbind", "ro", "nosuid", "nodev"],
                "source": NIX_STORE_PATH,
                "type": "bind"
            })
        );
        assert_eq!(applied["linux"], original["linux"]);

        std::fs::write(&config, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
        let before = std::fs::read(&config).unwrap();
        let collision = ResolvedRelease {
            mounts: vec![StoreMount {
                source: "/nix/store/22222222222222222222222222222222-data".to_string(),
                destination: "/data".to_string(),
            }],
            ..resolution.clone()
        };
        let error = apply_resolution(&config, &collision).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&config).unwrap(), before);

        for destination in ["/", "/nix", "/nix/store", "/nix/store/dependency"] {
            let mut conflicting = original.clone();
            conflicting["mounts"] = serde_json::json!([{
                "destination": destination,
                "source": "/host/source",
                "type": "bind"
            }]);
            std::fs::write(&config, serde_json::to_vec_pretty(&conflicting).unwrap()).unwrap();
            let before = std::fs::read(&config).unwrap();
            let error = apply_resolution(&config, &resolution).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("Nix store projection"));
            assert_eq!(std::fs::read(&config).unwrap(), before);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
