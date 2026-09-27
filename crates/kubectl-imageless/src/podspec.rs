//! Pod manifest for a seed image, validated with the node's own contract.
//!
//! The annotations written here are passed through `imageless::plan` before
//! anything is printed, so a selection the node would reject is rejected at
//! authoring time with the node's own error text.

use imageless::{plan, Materialize, OUTPUT_ANNOTATION, RELEASE_ANNOTATION, SOURCE_ANNOTATION};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

/// The source the node's zero-config discovery synthesizes for an embedded
/// flake; written explicitly so the pod manifest is self-describing.
pub const EMBEDDED_SOURCE: &str = "/etc/imageless";

/// Which annotation family tells the node what to materialize.
///
/// The two are mutually exclusive by SPEC §3 — `plan` refuses a pod carrying
/// both — so they are one enum rather than two optional fields that could be
/// set together.
pub enum Deploy<'a> {
    /// `run.imageless.source`: `EMBEDDED_SOURCE` for a packed seed, or a flake
    /// reference the node evaluates. Carries no grammar of its own; `plan`
    /// decides whether it is admissible.
    Source(&'a str),
    /// `imageless.run/release-v1`: a digest-addressed release the node resolves
    /// against its issuer catalogs. Already pinned before it gets here — a
    /// channel never reaches a pod manifest.
    Release(&'a str),
}

pub struct PodSpec<'a> {
    pub name: &'a str,
    pub namespace: Option<&'a str>,
    /// Digest-pinned reference: `HOST/REPO@sha256:<manifest-digest>`.
    pub image: &'a str,
    pub runtime_class: &'a str,
    pub deploy: Deploy<'a>,
    pub output: Option<&'a str>,
    pub command: &'a [String],
    /// Absolute container paths, each backed by its own emptyDir.
    pub writable: &'a [String],
}

pub fn pod(spec: &PodSpec) -> Result<Value, String> {
    if spec.command.is_empty() {
        return Err(
            "a workload command is required (the materialized rootfs chooses its own layout, \
             so there is no entrypoint to fall back on); pass it after `--`"
                .to_string(),
        );
    }
    validate_name(spec.name)?;
    let mut annotations = HashMap::new();
    match spec.deploy {
        Deploy::Source(source) => {
            annotations.insert(SOURCE_ANNOTATION.to_string(), source.to_string());
            if let Some(output) = spec.output {
                annotations.insert(OUTPUT_ANNOTATION.to_string(), output.to_string());
            }
        }
        Deploy::Release(reference) => {
            annotations.insert(RELEASE_ANNOTATION.to_string(), reference.to_string());
            // A release manifest names its own rootfs and process metadata, so
            // there is no output to select. `plan` returns before it would read
            // this annotation, which means writing it anyway would produce a pod
            // whose manifest claims something the node silently ignores.
            if spec.output.is_some() {
                return Err(
                    "--output selects a flake output; a release manifest names its own rootfs"
                        .to_string(),
                );
            }
        }
    }
    // A plan of `None` means the node would decline to materialize this pod at
    // all — today unreachable, since nothing here emits a container selector,
    // but printing a pod the runtime silently ignores is the one failure this
    // command must never produce.
    let materialize = plan(&annotations, Path::new("/"), "rootfs")
        .map_err(|error| error.to_string())?
        .ok_or("the annotations select no container to materialize")?;
    debug_assert!(match spec.deploy {
        Deploy::Source(_) => matches!(materialize, Materialize::Flake(_)),
        Deploy::Release(_) => matches!(materialize, Materialize::Release(_)),
    });

    let mut metadata = json!({
        "name": spec.name,
        "labels": { "app.kubernetes.io/managed-by": "kubectl-imageless" },
        "annotations": annotations,
    });
    if let Some(namespace) = spec.namespace {
        metadata["namespace"] = json!(namespace);
    }
    let mut container = json!({
        "name": "workload",
        "image": spec.image,
        "imagePullPolicy": "IfNotPresent",
        "command": spec.command,
    });
    let mut pod_spec = json!({
        "runtimeClassName": spec.runtime_class,
        "restartPolicy": "Never",
    });
    if !spec.writable.is_empty() {
        let (volumes, mounts) = writable_volumes(spec.writable)?;
        pod_spec["volumes"] = json!(volumes);
        container["volumeMounts"] = json!(mounts);
    }
    pod_spec["containers"] = json!([container]);
    Ok(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": metadata,
        "spec": pod_spec,
    }))
}

/// One emptyDir per writable path. The node forces the materialized root
/// read-only (SPEC §4.4), so these are where a workload writes.
fn writable_volumes(paths: &[String]) -> Result<(Vec<Value>, Vec<Value>), String> {
    let mut volumes = Vec::new();
    let mut mounts = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let canonical = path.starts_with('/')
            && path.len() > 1
            && !path.ends_with('/')
            && path[1..]
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..");
        if !canonical {
            return Err(format!(
                "--writable {path}: must be an absolute, canonical path other than /"
            ));
        }
        if paths[..index].contains(path) {
            return Err(format!("--writable {path}: given twice"));
        }
        let name = format!("writable-{index}");
        volumes.push(json!({ "name": name, "emptyDir": {} }));
        mounts.push(json!({ "name": name, "mountPath": path }));
    }
    Ok((volumes, mounts))
}

/// RFC 1123 label: what the API server enforces for pod names.
pub fn validate_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{name}: pod names are lowercase alphanumerics and `-`, at most 63 characters"
        ))
    }
}

/// Derive a pod name from a directory name, or fall back to `imageless-run`.
pub fn derive_name(directory: &Path) -> String {
    // Canonicalize so `run .` names the pod after the actual directory —
    // `Path::new(".").file_name()` is `None`, not the current directory.
    let canonical = directory.canonicalize();
    let directory = canonical.as_deref().unwrap_or(directory);
    let raw = directory
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    sanitize_name(&raw)
}

/// Coerce arbitrary text into the RFC 1123 label [`validate_name`] accepts,
/// falling back to `imageless-run` when nothing usable survives. Shared with
/// `flakeref`, so a name derived from a flake reference and one derived from a
/// directory cannot disagree about what a legal pod name is.
pub fn sanitize_name(raw: &str) -> String {
    let raw = raw.to_lowercase();
    let mut name = String::new();
    for character in raw.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            name.push(character);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_matches('-');
    let name = &name[..name.len().min(63)];
    if name.is_empty() {
        "imageless-run".to_string()
    } else {
        name.trim_matches('-').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(command: &'a [String]) -> PodSpec<'a> {
        PodSpec {
            name: "demo",
            namespace: None,
            image: "registry.example/apps/demo@sha256:0000",
            runtime_class: "imageless",
            deploy: Deploy::Source(EMBEDDED_SOURCE),
            output: None,
            command,
            writable: &[],
        }
    }

    #[test]
    fn writable_paths_become_emptydir_mounts_and_bad_ones_are_refused() {
        let command = vec!["/bin/server".to_string()];
        let writable = vec!["/tmp".to_string(), "/var/cache/app".to_string()];
        let manifest = pod(&PodSpec {
            writable: &writable,
            ..spec(&command)
        })
        .unwrap();
        assert_eq!(
            manifest["spec"]["volumes"],
            json!([
                { "name": "writable-0", "emptyDir": {} },
                { "name": "writable-1", "emptyDir": {} },
            ])
        );
        assert_eq!(
            manifest["spec"]["containers"][0]["volumeMounts"],
            json!([
                { "name": "writable-0", "mountPath": "/tmp" },
                { "name": "writable-1", "mountPath": "/var/cache/app" },
            ])
        );
        // Without the flag the manifest carries no volumes at all.
        assert!(pod(&spec(&command)).unwrap()["spec"]
            .get("volumes")
            .is_none());
        for bad in [
            &["tmp"][..],
            &["/"],
            &["/tmp/"],
            &["/a/../b"],
            &["/tmp", "/tmp"],
        ] {
            let bad: Vec<String> = bad.iter().map(|path| path.to_string()).collect();
            assert!(pod(&PodSpec {
                writable: &bad,
                ..spec(&command)
            })
            .is_err());
        }
    }

    #[test]
    fn the_manifest_selects_the_embedded_source() {
        let command = vec!["/bin/server".to_string(), "--port=8080".to_string()];
        let pod = pod(&spec(&command)).unwrap();
        assert_eq!(
            pod["metadata"]["annotations"][SOURCE_ANNOTATION],
            EMBEDDED_SOURCE
        );
        assert_eq!(pod["spec"]["runtimeClassName"], "imageless");
        assert_eq!(pod["spec"]["restartPolicy"], "Never");
        assert_eq!(pod["spec"]["containers"][0]["command"][1], "--port=8080");
        assert!(pod["metadata"]["annotations"][OUTPUT_ANNOTATION].is_null());
    }

    #[test]
    fn a_missing_command_fails_at_authoring_time() {
        let error = pod(&spec(&[])).unwrap_err();
        assert!(error.contains("command is required"), "{error}");
    }

    #[test]
    fn an_invalid_output_fails_with_the_node_s_error_text() {
        let command = vec!["/bin/true".to_string()];
        let mut with_output = spec(&command);
        with_output.output = Some("has whitespace here");
        let error = pod(&with_output).unwrap_err();
        assert!(error.contains(OUTPUT_ANNOTATION), "{error}");
    }

    #[test]
    fn namespace_appears_only_when_given() {
        let command = vec!["/bin/true".to_string()];
        let mut with = spec(&command);
        with.namespace = Some("staging");
        assert_eq!(pod(&with).unwrap()["metadata"]["namespace"], "staging");
        assert!(pod(&spec(&command)).unwrap()["metadata"]["namespace"].is_null());
    }

    #[test]
    fn names_are_derived_and_validated() {
        assert_eq!(derive_name(Path::new("/work/My App_v2")), "my-app-v2");
        assert_eq!(derive_name(Path::new("/work/---")), "imageless-run");
        assert!(validate_name("ok-name").is_ok());
        assert!(validate_name("-leading").is_err());
        assert!(validate_name("UPPER").is_err());
        assert!(validate_name(&"x".repeat(64)).is_err());
    }

    #[test]
    fn dot_derives_from_the_actual_directory_not_the_fallback() {
        let current = std::env::current_dir().unwrap();
        assert_eq!(derive_name(Path::new(".")), derive_name(&current));
        assert_ne!(derive_name(Path::new(".")), "imageless-run");
    }
}
