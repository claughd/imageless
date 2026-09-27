//! Bundle selection: annotations, embedded-source discovery, and validation.

use crate::materialize::{ContractError, Materialize, ResolvePurpose, ResolveRequest};
use crate::release::ReleaseReference;
use crate::{
    CONTAINERS_ANNOTATION, CRI_CONTAINER_NAME_ANNOTATION, CRI_CONTAINER_TYPE_ANNOTATION,
    EMBEDDED_FLAKE_PATH, MAX_ANNOTATION_VALUE_BYTES, MAX_SELECTOR_BYTES, OUTPUT_ANNOTATION,
    PROTOCOL_VERSION, RELEASE_ANNOTATION, RELEASE_CONTAINERS_ANNOTATION,
    RELEASE_SKIP_CONTAINERS_ANNOTATION, SKIP_CONTAINERS_ANNOTATION, SOURCE_ANNOTATION,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

pub fn plan(
    annotations: &HashMap<String, String>,
    rootfs: &Path,
    default_output: &str,
) -> Result<Option<Materialize>, ContractError> {
    if annotations
        .get(CRI_CONTAINER_TYPE_ANNOTATION)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("sandbox"))
    {
        return Ok(None);
    }
    let release = annotations.get(RELEASE_ANNOTATION);
    let (containers_field, skip_field) = if release.is_some() {
        (
            RELEASE_CONTAINERS_ANNOTATION,
            RELEASE_SKIP_CONTAINERS_ANNOTATION,
        )
    } else {
        (CONTAINERS_ANNOTATION, SKIP_CONTAINERS_ANNOTATION)
    };
    if !container_selected(annotations, containers_field, skip_field)? {
        return Ok(None);
    }
    if let Some(reference) = release {
        let reference = ReleaseReference::parse(reference)
            .map_err(|error| ContractError::new(RELEASE_ANNOTATION, error.diagnostic))?;
        if annotations.contains_key(SOURCE_ANNOTATION) {
            return Err(ContractError::new(
                RELEASE_ANNOTATION,
                "must not be combined with development source annotations",
            ));
        }
        return Ok(Some(Materialize::Release(reference)));
    }
    let Some(source) = annotations.get(SOURCE_ANNOTATION) else {
        return Ok(None);
    };
    validate_source(source)?;
    let (field, output) = annotations
        .get(OUTPUT_ANNOTATION)
        .map(|value| (OUTPUT_ANNOTATION, value.as_str()))
        .unwrap_or(("IMAGELESS_DEFAULT_OUTPUT", default_output));
    validate_output(field, output)?;

    let flake = if let Some(relative) = source.strip_prefix('/') {
        format!("path:{}", rootfs.join(relative).display())
    } else {
        source.clone()
    };
    Ok(Some(Materialize::Flake(format!("{flake}#{output}"))))
}

/// Discover an embedded source in the bundle rootfs.
///
/// The flake is the metadata: a regular `etc/imageless/flake.nix` selects the
/// container with source `/etc/imageless` and the runtime's default output
/// (`rootfs` unless configured otherwise). An image with a nonstandard output
/// aliases it to `rootfs` inside its own flake; deployer-side overrides are
/// annotations only (handled by the caller, which skips this discovery when
/// selection annotations are present).
pub(crate) fn embedded_source_annotations(
    rootfs: &Path,
) -> io::Result<Option<HashMap<String, String>>> {
    // A symlinked ancestor (`etc`, `etc/imageless`) would be resolved by the
    // HOST kernel against the node's root, so the file it reaches is not in
    // the image at all. Such an image carries no embedded flake at this path:
    // pass it through rather than fail closed, since symlinking `/etc` is an
    // ordinary thing for a non-imageless image to do.
    let parent = Path::new(EMBEDDED_FLAKE_PATH)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or_default();
    if first_symlinked_component(rootfs, parent)?.is_some() {
        return Ok(None);
    }
    let path = rootfs.join(EMBEDDED_FLAKE_PATH);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "embedded flake must be a regular file, not a symlink",
        ));
    }
    Ok(Some(HashMap::from([(
        SOURCE_ANNOTATION.to_string(),
        "/etc/imageless".to_string(),
    )])))
}

fn container_selected(
    annotations: &HashMap<String, String>,
    containers_field: &'static str,
    skip_field: &'static str,
) -> Result<bool, ContractError> {
    if annotations
        .get(CRI_CONTAINER_TYPE_ANNOTATION)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("sandbox"))
    {
        return Ok(false);
    }
    let name = annotations
        .get(CRI_CONTAINER_NAME_ANNOTATION)
        .map(String::as_str);
    if let Some(selector) = annotations.get(skip_field) {
        if name.is_some_and(|name| {
            selector_names(skip_field, selector).is_ok_and(|names| names.contains(&name))
        }) {
            return Ok(false);
        }
        // Preserve validation even if there is no matching CRI name.
        selector_names(skip_field, selector)?;
    }
    match annotations.get(containers_field) {
        Some(selector) => {
            let names = selector_names(containers_field, selector)?;
            Ok(name.is_some_and(|name| names.contains(&name)))
        }
        None => Ok(true),
    }
}

fn selector_names<'a>(
    field: &'static str,
    selector: &'a str,
) -> Result<Vec<&'a str>, ContractError> {
    validate_scalar(field, selector, MAX_SELECTOR_BYTES)?;
    let names: Vec<_> = selector.split(',').map(str::trim).collect();
    for name in &names {
        if name.is_empty() {
            return Err(ContractError::new(
                field,
                "contains an empty container name",
            ));
        }
        if name.len() > 63
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || name.starts_with('-')
            || name.ends_with('-')
        {
            return Err(ContractError::new(
                field,
                format!("container name {name:?} is not a Kubernetes DNS label"),
            ));
        }
    }
    Ok(names)
}

fn validate_scalar(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ContractError> {
    if value.is_empty() {
        return Err(ContractError::new(field, "must not be empty"));
    }
    if value.len() > max_bytes {
        return Err(ContractError::new(
            field,
            format!("exceeds the {max_bytes}-byte limit"),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(ContractError::new(field, "contains a control character"));
    }
    Ok(())
}

pub fn validate_store_path(field: &'static str, value: &str) -> Result<(), ContractError> {
    validate_scalar(field, value, MAX_ANNOTATION_VALUE_BYTES)?;
    let Some(basename) = value.strip_prefix("/nix/store/") else {
        return Err(ContractError::new(
            field,
            "must be a direct child of /nix/store",
        ));
    };
    if basename.contains('/') {
        return Err(ContractError::new(
            field,
            "must be a direct child of /nix/store",
        ));
    }
    let Some((hash, name)) = basename.split_at_checked(32) else {
        return Err(ContractError::new(field, "has a truncated Nix store hash"));
    };
    const NIX_BASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    if !hash.bytes().all(|byte| NIX_BASE32.contains(&byte)) {
        return Err(ContractError::new(field, "has an invalid Nix store hash"));
    }
    let Some(name) = name.strip_prefix('-') else {
        return Err(ContractError::new(field, "must include a store name"));
    };
    if name.is_empty()
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.' | b'_' | b'?' | b'=')
        })
    {
        return Err(ContractError::new(field, "has an invalid Nix store name"));
    }
    Ok(())
}

/// Validate a `run.imageless.source` annotation value (SPEC §3): an absolute,
/// canonical in-image path, or an external flake reference carrying an
/// explicit remote scheme — node-local schemes (`path:`, `file:`, any
/// `*+file:` transport) and registry names (bare words, `flake:`) are
/// rejected. Public so client-side tooling rejects exactly what the node
/// rejects, with the node's own error text.
pub fn validate_source(source: &str) -> Result<(), ContractError> {
    validate_scalar(SOURCE_ANNOTATION, source, MAX_ANNOTATION_VALUE_BYTES)?;
    if source.chars().any(char::is_whitespace) {
        return Err(ContractError::new(
            SOURCE_ANNOTATION,
            "must not contain whitespace",
        ));
    }
    if source.contains('#') {
        return Err(ContractError::new(
            SOURCE_ANNOTATION,
            "must not contain a flake output fragment",
        ));
    }
    if let Some(relative) = source.strip_prefix('/') {
        if !relative.is_empty()
            && relative
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
        {
            return Err(ContractError::new(
                SOURCE_ANNOTATION,
                "in-image paths must be canonical and must not contain `.` or `..` components",
            ));
        }
        return Ok(());
    }
    // Anything else is an external flake reference (SPEC §3). The only way a
    // pod may name node-local content is the in-image `/` form above: the
    // `path:` prefix that embedded-flake policies allow-list belongs to the
    // runtime's own rewrite of staged in-image sources, so a node-local scheme
    // arriving through an annotation would read arbitrary node files into the
    // staging copy. Bare registry words (and their `flake:` spelling) are
    // rejected with the same rule — node-side registry state is not a
    // deployment identity.
    let scheme = match source.split_once(':') {
        Some((scheme, _)) if !scheme.contains('/') => scheme.to_ascii_lowercase(),
        _ => {
            return Err(ContractError::new(
                SOURCE_ANNOTATION,
                "external references must carry an explicit scheme; registry names are not resolved",
            ));
        }
    };
    if scheme == "flake" {
        return Err(ContractError::new(
            SOURCE_ANNOTATION,
            "external references must carry an explicit scheme; registry names are not resolved",
        ));
    }
    if scheme == "path" || scheme.split('+').next_back() == Some("file") {
        return Err(ContractError::new(
            SOURCE_ANNOTATION,
            "node-local schemes are not allowed in annotations; use an absolute in-image path",
        ));
    }
    Ok(())
}

pub(crate) fn validate_output(field: &'static str, output: &str) -> Result<(), ContractError> {
    validate_scalar(field, output, 256)?;
    if !output
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.' | b'_'))
    {
        return Err(ContractError::new(
            field,
            "may contain only ASCII letters, digits, `.`, `_`, `+`, and `-`",
        ));
    }
    Ok(())
}

pub fn expansion_request(
    config_path: &Path,
    bundle_path: &Path,
    default_output: &str,
    timeout_seconds: u64,
) -> io::Result<Option<ResolveRequest>> {
    #[derive(Deserialize)]
    struct SelectionSpec {
        #[serde(default)]
        annotations: HashMap<String, String>,
        root: Option<SelectionRoot>,
    }

    #[derive(Deserialize)]
    struct SelectionRoot {
        path: PathBuf,
    }

    let spec: SelectionSpec = serde_json::from_slice(&std::fs::read(config_path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut annotations = spec.annotations;
    let root = spec
        .root
        .map(|root| root.path)
        .unwrap_or_else(|| PathBuf::from("rootfs"));
    let rootfs = if root.is_absolute() {
        root
    } else {
        bundle_path.join(root)
    };
    if !annotations.contains_key(RELEASE_ANNOTATION) && !annotations.contains_key(SOURCE_ANNOTATION)
    {
        if let Some(embedded) = embedded_source_annotations(&rootfs)? {
            annotations.extend(embedded);
        }
    }
    let materialize = plan(&annotations, &rootfs, default_output)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if let (Some(_), Some(relative)) = (
        &materialize,
        annotations
            .get(SOURCE_ANNOTATION)
            .and_then(|source| source.strip_prefix('/')),
    ) {
        if let Some(symlink) = first_symlinked_component(&rootfs, relative)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                ContractError::new(
                    SOURCE_ANNOTATION,
                    format!("in-image path traverses the symlink `/{symlink}`"),
                ),
            ));
        }
    }
    Ok(materialize.map(|materialize| ResolveRequest {
        version: PROTOCOL_VERSION,
        purpose: ResolvePurpose::Runtime,
        materialize,
        bundle_path: bundle_path.to_path_buf(),
        timeout_ms: timeout_seconds.saturating_mul(1000),
        container_name: annotations.get(CRI_CONTAINER_NAME_ANNOTATION).cloned(),
    }))
}

/// The first component of in-image `relative` that is a symlink, if any.
///
/// In-image paths are read from the host before the container exists, so the
/// host kernel resolves them: an image symlink `/a -> /` turns `/a/etc/nixos`
/// into the node's own `/etc/nixos` (SPEC §2.3). Only the rootfs itself is
/// trusted — the engine chose it — so every component below it is checked,
/// the last one included. Nothing runs in the rootfs yet, so it cannot change
/// between this walk and staging. A missing component ends the walk: whatever
/// reads the path next reports it absent.
fn first_symlinked_component(rootfs: &Path, relative: &str) -> io::Result<Option<String>> {
    let mut path = rootfs.to_path_buf();
    for component in relative.split('/').filter(|part| !part.is_empty()) {
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let inside = path.strip_prefix(rootfs).unwrap_or(&path);
                return Ok(Some(inside.display().to_string()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{annotations, temporary, RELEASE_REF, STORE};

    #[test]
    fn annotation_selection_is_fail_closed_and_normalized() {
        assert_eq!(
            plan(&annotations(&[]), Path::new("/b/rootfs"), "rootfs").unwrap(),
            None
        );
        assert_eq!(
            plan(
                &annotations(&[
                    (RELEASE_ANNOTATION, RELEASE_REF),
                    (CRI_CONTAINER_TYPE_ANNOTATION, "sandbox"),
                ]),
                Path::new("/b/rootfs"),
                "rootfs",
            )
            .unwrap(),
            None
        );
        assert_eq!(
            plan(
                &annotations(&[
                    (SOURCE_ANNOTATION, "/etc/flake"),
                    (OUTPUT_ANNOTATION, "agent"),
                ]),
                Path::new("/bundle/rootfs"),
                "rootfs",
            )
            .unwrap(),
            Some(Materialize::Flake(
                "path:/bundle/rootfs/etc/flake#agent".into()
            ))
        );
        assert!(plan(
            &annotations(&[(SOURCE_ANNOTATION, "/etc/../flake")]),
            Path::new("rootfs"),
            "rootfs"
        )
        .is_err());
        assert!(plan(
            &annotations(&[
                (RELEASE_ANNOTATION, RELEASE_REF),
                (SOURCE_ANNOTATION, "/etc/flake"),
            ]),
            Path::new("/bundle/rootfs"),
            "rootfs",
        )
        .is_err());
    }

    #[test]
    fn external_references_require_remote_schemes() {
        for source in [
            "github:example/agent?rev=0123456789abcdef0123456789abcdef01234567",
            "git+https://example.com/agent.git?rev=0123456789abcdef0123456789abcdef01234567",
            "https://example.com/agent.tar.gz",
        ] {
            assert_eq!(
                plan(
                    &annotations(&[(SOURCE_ANNOTATION, source)]),
                    Path::new("/bundle/rootfs"),
                    "rootfs",
                )
                .unwrap(),
                Some(Materialize::Flake(format!("{source}#rootfs"))),
                "expected {source} to pass through as an external reference"
            );
        }
        // Node-local schemes would read node files through the staging copy;
        // registry names would resolve against node-side registry state.
        for source in [
            "path:/etc/imageless",
            "PATH:/etc/imageless",
            "file:///etc/passwd",
            "git+file:///var/lib/secrets",
            "tarball+file:///root/archive.tar",
            "nixpkgs",
            "flake:nixpkgs",
            "./agent",
            "etc/imageless:agent",
        ] {
            assert!(
                plan(
                    &annotations(&[(SOURCE_ANNOTATION, source)]),
                    Path::new("/bundle/rootfs"),
                    "rootfs",
                )
                .is_err(),
                "expected {source} to be rejected"
            );
        }
    }

    #[test]
    fn unrecognized_annotations_are_ignored() {
        // Only run.imageless.* and imageless.run/* select; anything else is an
        // ordinary annotation with no special handling.
        for (field, value) in [
            ("com.example.team", "platform"),
            ("example.run/release-v1", RELEASE_REF),
            ("run.example.source", "/etc/imageless"),
            ("run.imageless.closure", STORE),
        ] {
            assert_eq!(
                plan(
                    &annotations(&[(field, value)]),
                    Path::new("/bundle/rootfs"),
                    "rootfs",
                )
                .unwrap(),
                None,
                "expected {field} to be ignored"
            );
        }
    }

    #[test]
    fn selectors_skip_sandbox_and_unselected_sidecars() {
        let selected = annotations(&[
            (RELEASE_ANNOTATION, RELEASE_REF),
            (CRI_CONTAINER_NAME_ANNOTATION, "agent"),
            (RELEASE_CONTAINERS_ANNOTATION, "init,agent"),
        ]);
        assert!(plan(&selected, Path::new("rootfs"), "rootfs")
            .unwrap()
            .is_some());
        let sidecar = annotations(&[
            (RELEASE_ANNOTATION, RELEASE_REF),
            (CRI_CONTAINER_NAME_ANNOTATION, "sidecar"),
            (RELEASE_CONTAINERS_ANNOTATION, "init,agent"),
        ]);
        assert_eq!(plan(&sidecar, Path::new("rootfs"), "rootfs").unwrap(), None);
    }

    #[test]
    fn zero_config_embedded_flake_is_discovered() {
        let bundle = temporary("zero-config");
        let rootfs = bundle.join("rootfs");
        let flake = rootfs.join(EMBEDDED_FLAKE_PATH);
        std::fs::create_dir_all(flake.parent().unwrap()).unwrap();
        std::fs::write(&flake, "{ outputs = _: { }; }").unwrap();
        std::fs::write(
            bundle.join("config.json"),
            br#"{"root":{"path":"rootfs"},"annotations":{}}"#,
        )
        .unwrap();

        let request = expansion_request(&bundle.join("config.json"), &bundle, "rootfs", 30)
            .unwrap()
            .unwrap();
        assert_eq!(
            request.materialize,
            Materialize::Flake(format!("path:{}/etc/imageless#rootfs", rootfs.display()))
        );

        // The runtime's configured default output applies to the discovered
        // flake; the image aliases a nonstandard output inside its own flake.
        let request = expansion_request(&bundle.join("config.json"), &bundle, "custom", 30)
            .unwrap()
            .unwrap();
        assert_eq!(
            request.materialize,
            Materialize::Flake(format!("path:{}/etc/imageless#custom", rootfs.display()))
        );

        // Other files under etc/imageless are ordinary rootfs content with no
        // bearing on discovery.
        std::fs::write(rootfs.join("etc/imageless/source.json"), "{}").unwrap();
        let request = expansion_request(&bundle.join("config.json"), &bundle, "rootfs", 30)
            .unwrap()
            .unwrap();
        assert_eq!(
            request.materialize,
            Materialize::Flake(format!("path:{}/etc/imageless#rootfs", rootfs.display()))
        );

        // A symlinked flake.nix fails closed instead of selecting defaults.
        std::fs::remove_file(&flake).unwrap();
        std::os::unix::fs::symlink("../elsewhere/flake.nix", &flake).unwrap();
        assert!(expansion_request(&bundle.join("config.json"), &bundle, "rootfs", 30).is_err());
        std::fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn explicit_annotations_override_embedded_discovery() {
        let bundle = temporary("annotations-over-embedded");
        let rootfs = bundle.join("rootfs");
        let flake = rootfs.join(EMBEDDED_FLAKE_PATH);
        std::fs::create_dir_all(flake.parent().unwrap()).unwrap();
        std::fs::write(&flake, "{ outputs = _: { }; }").unwrap();
        std::fs::write(
            bundle.join("config.json"),
            format!(
                r#"{{"root":{{"path":"rootfs"}},"annotations":{{"{SOURCE_ANNOTATION}":"/etc/elsewhere"}}}}"#
            ),
        )
        .unwrap();

        let request = expansion_request(&bundle.join("config.json"), &bundle, "rootfs", 30)
            .unwrap()
            .unwrap();
        assert_eq!(
            request.materialize,
            Materialize::Flake(format!("path:{}/etc/elsewhere#rootfs", rootfs.display()))
        );
        std::fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn in_image_sources_never_traverse_a_symlink_onto_the_host() {
        let bundle = temporary("symlinked-source");
        let rootfs = bundle.join("rootfs");
        let config = bundle.join("config.json");
        let with_source = |source: &str| {
            std::fs::write(
                &config,
                format!(
                    r#"{{"root":{{"path":"rootfs"}},"annotations":{{"{SOURCE_ANNOTATION}":"{source}"}}}}"#
                ),
            )
            .unwrap();
            expansion_request(&config, &bundle, "rootfs", 30)
        };
        std::fs::create_dir_all(rootfs.join("srv/app")).unwrap();
        std::os::unix::fs::symlink("/", rootfs.join("hostroot")).unwrap();
        std::os::unix::fs::symlink("srv/app", rootfs.join("app")).unwrap();

        // The reproduced exploit: `/hostroot -> /` would stage the node's own
        // directory. Refused, and the diagnostic names the offending link.
        let error = with_source("/hostroot/etc").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("`/hostroot`"), "{error}");
        // A symlink in last position, even a relative in-image one, is refused
        // here too rather than left to staging.
        assert!(with_source("/app").is_err());
        // A real directory path is unaffected, and a missing one is left for
        // staging to report.
        assert_eq!(
            with_source("/srv/app").unwrap().unwrap().materialize,
            Materialize::Flake(format!("path:{}/srv/app#rootfs", rootfs.display()))
        );
        assert!(with_source("/srv/missing/deeper").unwrap().is_some());

        // Zero-config discovery through a symlinked ancestor would stat a HOST
        // file; such an image has no embedded flake and passes through.
        std::fs::write(&config, br#"{"root":{"path":"rootfs"},"annotations":{}}"#).unwrap();
        let host_etc = bundle.join("host-etc");
        std::fs::create_dir_all(host_etc.join("imageless")).unwrap();
        std::fs::write(host_etc.join("imageless/flake.nix"), "{ }").unwrap();
        std::os::unix::fs::symlink(&host_etc, rootfs.join("etc")).unwrap();
        assert!(expansion_request(&config, &bundle, "rootfs", 30)
            .unwrap()
            .is_none());
        std::fs::remove_file(rootfs.join("etc")).unwrap();
        std::fs::create_dir(rootfs.join("etc")).unwrap();
        std::os::unix::fs::symlink(host_etc.join("imageless"), rootfs.join("etc/imageless"))
            .unwrap();
        assert!(expansion_request(&config, &bundle, "rootfs", 30)
            .unwrap()
            .is_none());
        std::fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn bundles_without_embedded_sources_or_annotations_pass_through() {
        let bundle = temporary("passthrough");
        std::fs::create_dir_all(bundle.join("rootfs/etc")).unwrap();
        std::fs::write(
            bundle.join("config.json"),
            br#"{"root":{"path":"rootfs"},"annotations":{}}"#,
        )
        .unwrap();
        assert!(
            expansion_request(&bundle.join("config.json"), &bundle, "rootfs", 30)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(bundle).unwrap();
    }
}
