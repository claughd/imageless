//! Node Nix-store projection into the delegated container.

use crate::release::ResolvedRelease;
use crate::release::StoreMount;
use crate::spec::validate_store_path;
use crate::NIX_STORE_PATH;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How the node's `/nix/store` is projected into the delegated container.
///
/// Both backends produce ordinary read-only OCI bind mounts and leave the
/// delegate unmodified, so neither changes the published release contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreProjection {
    /// Bind the whole node store read-only at `/nix/store`. The stock-runc
    /// compatibility backend: it provides integrity but not confidentiality,
    /// exposing every node store path regardless of the selected release.
    Node,
    /// Bind only the paths in the selected release closure, one read-only mount
    /// per store path. The hardened backend: the workload sees exactly its own
    /// closure and no unrelated node store paths.
    Closure(Vec<String>),
    /// Add no store mount: the runtime that consumes the bundle projects the
    /// store itself. For embedding runtimes whose sandbox builds its own view of
    /// `/nix/store` from `root.path` and refuses any bundle mount there (Cowboy's
    /// nucleus cage). Workload mounts at or under `/nix/store` are still refused,
    /// so the store view stays the runtime's alone.
    Runtime,
}

pub(crate) fn client_nix_store() -> PathBuf {
    std::env::var_os("IMAGELESS_NIX_STORE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(option_env!("IMAGELESS_NIX_STORE").unwrap_or("nix-store")))
}

/// Enumerate the transitive runtime closure of `store_paths` with
/// `nix-store --query --requisites`. The seeds are already realized locally, so
/// this is a local query, not a substitution. Every returned line must be a
/// canonical direct child of `/nix/store`; a misbehaving or compromised
/// `nix-store` that prints anything else fails closed, so it can never inject an
/// arbitrary bind source into the container spec.
pub fn enumerate_closure(nix_store: &Path, store_paths: &[&str]) -> io::Result<Vec<String>> {
    if store_paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut command = Command::new(nix_store);
    command.args([OsStr::new("--query"), OsStr::new("--requisites")]);
    for path in store_paths {
        command.arg(path);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "nix-store --query --requisites failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut paths = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        validate_store_path("closure requisite", path)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        paths.push(path.to_string());
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Select the store-projection backend for a client bundle rewrite.
/// `IMAGELESS_STORE_PROJECTION=closure` selects the hardened closure-scoped
/// backend and `runtime` leaves the store to the consuming runtime; any other
/// value (or unset) keeps the whole-node compatibility bind, which stays the
/// explicit fallback. Embedders that always project the store themselves set
/// [`crate::PrepareBundle::runtime_supplies_store`] instead of relying on the
/// node's environment.
pub fn store_projection_for(resolution: &ResolvedRelease) -> io::Result<StoreProjection> {
    match std::env::var("IMAGELESS_STORE_PROJECTION").ok().as_deref() {
        Some("runtime") => Ok(StoreProjection::Runtime),
        Some("closure") => {
            let seeds: Vec<&str> = resolution.store_paths().collect();
            let closure = enumerate_closure(&client_nix_store(), &seeds)?;
            Ok(StoreProjection::Closure(closure))
        }
        _ => Ok(StoreProjection::Node),
    }
}

pub(crate) fn apply_store_mounts(
    document: &mut serde_json::Value,
    release_mounts: &[StoreMount],
) -> io::Result<()> {
    if release_mounts.is_empty() {
        return Ok(());
    }
    let mounts = document
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "config.json is not an object"))?
        .entry("mounts")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "config.json mounts is not an array",
            )
        })?;
    let mut destinations = std::collections::HashSet::new();
    for mount in mounts.iter() {
        let destination = mount
            .get("destination")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "config.json mount has no string destination",
                )
            })?;
        destinations.insert(destination.to_string());
    }
    for mount in release_mounts {
        if !destinations.insert(mount.destination.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "release store mount collides with existing destination {}",
                    mount.destination
                ),
            ));
        }
        mounts.push(serde_json::json!({
            "destination": mount.destination,
            "options": ["rbind", "ro", "nosuid", "nodev"],
            "source": mount.source,
            "type": "bind"
        }));
    }
    Ok(())
}

/// Make absolute Nix store references in the selected rootfs resolve under a
/// stock OCI runtime. The node store is immutable and is exposed read-only;
/// execute permission is deliberately preserved because rootfs programs and
/// their dynamic loaders live there.
///
/// [`StoreProjection::Node`] binds the whole store (compatibility);
/// [`StoreProjection::Closure`] binds only the selected closure paths (hardened);
/// [`StoreProjection::Runtime`] adds nothing, for a runtime that projects the
/// store itself. The workload-mount conflict check is identical for all three.
pub(crate) fn apply_node_store_projection(
    document: &mut serde_json::Value,
    projection: &StoreProjection,
) -> io::Result<()> {
    // The rootfs store path is projected AS the container root (`root.path`), so
    // it must not also be bound as a store path inside itself: runc refuses a
    // mount whose target coincides with the container's own rootfs ("mountpoint
    // is on the top of rootfs"). The whole node closure legitimately contains
    // the rootfs path; drop just that one from the per-path binds.
    let container_root = document
        .get("root")
        .and_then(|root| root.get("path"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let mounts = document
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "config.json is not an object"))?
        .entry("mounts")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "config.json mounts is not an array",
            )
        })?;
    let store = Path::new(NIX_STORE_PATH);
    for mount in mounts.iter() {
        let destination = mount
            .get("destination")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "config.json mount has no string destination",
                )
            })?;
        if !normalized_oci_destination(destination) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "config.json mount destination is not an absolute normalized path",
            ));
        }
        let destination = Path::new(destination);
        if destination.starts_with(store) || store.starts_with(destination) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "mount destination {} conflicts with the node Nix store projection",
                    destination.display()
                ),
            ));
        }
    }
    match projection {
        StoreProjection::Runtime => {}
        StoreProjection::Node => {
            mounts.push(serde_json::json!({
                "destination": NIX_STORE_PATH,
                "options": ["rbind", "ro", "nosuid", "nodev"],
                "source": NIX_STORE_PATH,
                "type": "bind"
            }));
        }
        StoreProjection::Closure(paths) => {
            // The sparse rootfs's `/nix/store` is itself a read-only Nix store
            // path, so a stock runtime cannot `mkdir` the per-path mountpoints a
            // closure bind needs. Lay an empty writable tmpfs over `/nix/store`
            // first (the mountpoint dir already exists in the rootfs), giving the
            // runtime a writable surface to create each `/nix/store/<hash>`
            // mountpoint; the real paths are then read-only binds on top. The
            // tmpfs root is writable but empty and per-container ephemeral, and a
            // workload write there cannot shadow a path already bound read-only —
            // so confidentiality (only the closure is visible) and the integrity
            // of each store path are both preserved. Ordering matters: the tmpfs
            // must precede the binds so the runtime mounts it before creating the
            // mountpoints inside it.
            mounts.push(serde_json::json!({
                "destination": NIX_STORE_PATH,
                "options": ["nosuid", "nodev", "mode=0755"],
                "source": "tmpfs",
                "type": "tmpfs"
            }));
            for path in paths {
                if container_root.as_deref() == Some(path.as_str()) {
                    continue;
                }
                mounts.push(serde_json::json!({
                    "destination": path,
                    "options": ["rbind", "ro", "nosuid", "nodev"],
                    "source": path,
                    "type": "bind"
                }));
            }
        }
    }
    Ok(())
}

pub(crate) fn normalized_oci_destination(value: &str) -> bool {
    value.starts_with('/')
        && !value.contains('\0')
        && (value == "/"
            || (!value.ends_with('/')
                && value.split('/').skip(1).all(|component| {
                    !component.is_empty() && component != "." && component != ".."
                })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::apply_resolution_with_projection;
    use crate::testutil::{executable, temporary, without_text_file_busy, STORE};

    #[test]
    fn enumerate_closure_returns_sorted_unique_store_paths_and_fails_closed() {
        let dir = temporary("enumerate-closure");
        let nix = dir.join("fake-nix-store");
        executable(
            &nix,
            r#"for arg in "$@"; do
  case "$arg" in /nix/store/*) printf '%s\n' "$arg" ;; esac
done
printf '%s\n' "/nix/store/22222222222222222222222222222222-bash"
printf '%s\n' "/nix/store/11111111111111111111111111111111-libc"
printf '%s\n' "/nix/store/11111111111111111111111111111111-libc""#,
        );
        let closure = without_text_file_busy(|| enumerate_closure(&nix, &[STORE])).unwrap();
        assert_eq!(
            closure,
            vec![
                STORE.to_string(),
                "/nix/store/11111111111111111111111111111111-libc".to_string(),
                "/nix/store/22222222222222222222222222222222-bash".to_string(),
            ]
        );

        // A non-store requisite fails closed so a rogue nix-store cannot inject
        // an arbitrary bind source.
        let bad = dir.join("fake-nix-store-bad");
        executable(&bad, r#"printf '%s\n' "/etc/passwd""#);
        let error = without_text_file_busy(|| enumerate_closure(&bad, &[STORE])).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        // A nonzero exit is surfaced, not silently treated as an empty closure.
        let fail = dir.join("fake-nix-store-fail");
        executable(&fail, "echo boom >&2\nexit 3");
        assert!(without_text_file_busy(|| enumerate_closure(&fail, &[STORE])).is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn closure_projection_binds_only_the_selected_paths() {
        let dir = temporary("closure-projection");
        let config = dir.join("config.json");
        let original = serde_json::json!({
            "ociVersion": "1.2.0",
            "root": { "path": "rootfs" },
            "process": { "args": ["placeholder"] },
            "mounts": [{ "destination": "/data", "source": "tmpfs", "type": "tmpfs" }]
        });
        std::fs::write(&config, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
        let resolution = ResolvedRelease {
            identity: "test/agent@sha256:00".to_string(),
            rootfs: STORE.to_string(),
            process: None,
            mounts: Vec::new(),
        };
        let closure = vec![
            STORE.to_string(),
            "/nix/store/11111111111111111111111111111111-libc".to_string(),
        ];
        apply_resolution_with_projection(
            &config,
            &resolution,
            &StoreProjection::Closure(closure.clone()),
        )
        .unwrap();
        let applied: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        let mounts = applied["mounts"].as_array().unwrap();
        // The original workload mount is preserved.
        assert_eq!(mounts[0]["destination"], "/data");
        // An empty writable tmpfs scaffolds `/nix/store` so the runtime can
        // create the per-path mountpoints, and it precedes the binds.
        let store_tmpfs: Vec<usize> = mounts
            .iter()
            .enumerate()
            .filter(|(_, mount)| mount["destination"] == NIX_STORE_PATH && mount["type"] == "tmpfs")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(store_tmpfs.len(), 1);
        let tmpfs = &mounts[store_tmpfs[0]];
        assert_eq!(tmpfs["source"], "tmpfs");
        assert_eq!(
            tmpfs["options"],
            serde_json::json!(["nosuid", "nodev", "mode=0755"])
        );
        // One read-only bind per closure path, each ordered after the tmpfs, no
        // whole-store bind at `/nix/store`, and — crucially — the rootfs's own
        // store path (`STORE`, projected as the container root) is NOT bound
        // inside itself, so runc does not reject it as "on the top of rootfs".
        let store_binds: Vec<(usize, &str)> = mounts
            .iter()
            .enumerate()
            .filter(|(_, mount)| mount["type"] == "bind")
            .filter_map(|(index, mount)| {
                mount["destination"]
                    .as_str()
                    .filter(|destination| destination.starts_with(NIX_STORE_PATH))
                    .map(|destination| (index, destination))
            })
            .collect();
        assert_eq!(
            store_binds.iter().map(|(_, dst)| *dst).collect::<Vec<_>>(),
            vec!["/nix/store/11111111111111111111111111111111-libc"]
        );
        assert!(store_binds.iter().all(|(index, _)| *index > store_tmpfs[0]));
        assert!(store_binds
            .iter()
            .all(|(_, destination)| *destination != NIX_STORE_PATH && *destination != STORE));
        for (_, destination) in &store_binds {
            let mount = mounts
                .iter()
                .find(|mount| mount["destination"].as_str() == Some(destination))
                .unwrap();
            assert_eq!(mount["source"], mount["destination"]);
            assert_eq!(
                mount["options"],
                serde_json::json!(["rbind", "ro", "nosuid", "nodev"])
            );
        }

        // The workload-mount conflict check still fails closed under the
        // closure backend.
        let mut conflicting = original.clone();
        conflicting["mounts"] =
            serde_json::json!([{ "destination": "/nix/store", "source": "/host", "type": "bind" }]);
        std::fs::write(&config, serde_json::to_vec_pretty(&conflicting).unwrap()).unwrap();
        let error = apply_resolution_with_projection(
            &config,
            &resolution,
            &StoreProjection::Closure(closure),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn runtime_projection_adds_no_store_mount_and_still_refuses_workload_ones() {
        let dir = temporary("runtime-projection");
        let config = dir.join("config.json");
        let original = serde_json::json!({
            "ociVersion": "1.2.0",
            "root": { "path": "rootfs" },
            "process": { "args": ["placeholder"] },
            "mounts": [{ "destination": "/data", "source": "tmpfs", "type": "tmpfs" }]
        });
        std::fs::write(&config, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
        let resolution = ResolvedRelease {
            identity: "test/agent@sha256:00".to_string(),
            rootfs: STORE.to_string(),
            process: None,
            mounts: Vec::new(),
        };
        apply_resolution_with_projection(&config, &resolution, &StoreProjection::Runtime).unwrap();
        let applied: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        // The root is rewritten as with any backend, and the workload's own
        // mounts are untouched, but nothing is mounted at or under the store:
        // a runtime that builds its own store view (and refuses bundle mounts
        // there) gets a bundle it can accept.
        assert_eq!(applied["root"]["path"], STORE);
        assert_eq!(applied["root"]["readonly"], true);
        let mounts = applied["mounts"].as_array().unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0]["destination"], "/data");

        // The store view stays the runtime's alone: a workload mount there is
        // still refused.
        let mut conflicting = original.clone();
        conflicting["mounts"] = serde_json::json!([
            { "destination": "/nix/store/x", "source": "/host", "type": "bind" }
        ]);
        std::fs::write(&config, serde_json::to_vec_pretty(&conflicting).unwrap()).unwrap();
        let error =
            apply_resolution_with_projection(&config, &resolution, &StoreProjection::Runtime)
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        std::fs::remove_dir_all(dir).unwrap();
    }
}
