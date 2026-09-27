//! The `imageless.release.v1` manifest contract and node-owned issuer, cache,
//! and catalog policy (the optional cache-only release profile).
//!
//! The manifest parser/validator lives here so a resolver can consume releases
//! without any dependency on whatever CI produces them: any tool that can copy
//! a Nix closure to a cache and emit canonical manifest JSON conforms.

use crate::materialize::{ErrorCategory, ResolutionError};
use crate::minisign;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub const RELEASE_SCHEMA: &str = "imageless.release.v1";
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContractErrorKind {
    InvalidReference,
    InvalidManifest,
    DigestMismatch,
    IdentityMismatch,
    ManifestTooLarge,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContractError {
    pub(crate) kind: ContractErrorKind,
    pub(crate) diagnostic: &'static str,
}

impl ContractError {
    fn new(kind: ContractErrorKind, diagnostic: &'static str) -> Self {
        Self { kind, diagnostic }
    }
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.diagnostic)
    }
}

impl std::error::Error for ContractError {}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ReleaseReference {
    pub issuer: String,
    pub name: String,
    pub sha256: String,
}

impl ReleaseReference {
    pub fn parse(value: &str) -> Result<Self, ResolutionError> {
        Self::parse_contract(value).map_err(|error| {
            ResolutionError::new(ErrorCategory::InvalidRequest, error.diagnostic, false)
        })
    }

    pub(crate) fn parse_contract(value: &str) -> Result<Self, ContractError> {
        if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
            return Err(invalid_reference());
        }
        let (identity, digest) = value
            .rsplit_once("@sha256:")
            .ok_or_else(invalid_reference)?;
        let (issuer, name) = identity.split_once('/').ok_or_else(invalid_reference)?;
        if !valid_identifier(issuer) || !valid_release_name(name) {
            return Err(invalid_reference());
        }
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid_reference());
        }
        Ok(Self {
            issuer: issuer.to_string(),
            name: name.to_string(),
            sha256: digest.to_string(),
        })
    }

    pub fn identity(&self) -> String {
        format!("{}/{}@sha256:{}", self.issuer, self.name, self.sha256)
    }
}

fn invalid_reference() -> ContractError {
    ContractError::new(
        ContractErrorKind::InvalidReference,
        "release must be issuer/name@sha256:<64 lowercase hexadecimal digits>",
    )
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub(crate) fn valid_release_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !matches!(value.as_bytes().first(), Some(b'-' | b'.' | b'/'))
        && !matches!(value.as_bytes().last(), Some(b'-' | b'.' | b'/'))
        && value.split('/').all(valid_identifier)
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Selectors {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub containers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_containers: Vec<String>,
}

impl Selectors {
    pub fn permits(&self, name: Option<&str>) -> bool {
        if name.is_some_and(|name| self.skip_containers.iter().any(|item| item == name)) {
            return false;
        }
        self.containers.is_empty()
            || name.is_some_and(|name| self.containers.iter().any(|item| item == name))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ReleaseManifest {
    pub schema: String,
    pub issuer: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub selectors: Selectors,
    pub targets: BTreeMap<String, ReleaseTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sbom: Option<EvidenceReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<EvidenceReference>,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    value == &T::default()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct EvidenceReference {
    pub uri: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ReleaseTarget {
    pub rootfs: String,
    pub cache: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<StoreMount>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ProcessMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment: Vec<EnvironmentEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct EnvironmentEntry {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub allow_workload_override: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct StoreMount {
    pub source: String,
    pub destination: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ResolvedRelease {
    pub identity: String,
    pub rootfs: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<StoreMount>,
}

impl ResolvedRelease {
    pub fn store_paths(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.rootfs.as_str())
            .chain(self.mounts.iter().map(|mount| mount.source.as_str()))
    }
}

pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn canonical_bytes(manifest: &ReleaseManifest) -> Result<Vec<u8>, ResolutionError> {
    canonical_bytes_contract(manifest).map_err(map_contract_error)
}

fn canonical_bytes_contract(manifest: &ReleaseManifest) -> Result<Vec<u8>, ContractError> {
    let value = serde_json::to_value(manifest).map_err(|_| invalid_manifest())?;
    serde_json::to_vec(&value).map_err(|_| invalid_manifest())
}

pub(crate) fn parse_full_manifest(
    bytes: &[u8],
    reference: &ReleaseReference,
) -> Result<ReleaseManifest, ResolutionError> {
    parse_full_manifest_contract(bytes, reference).map_err(map_contract_error)
}

fn parse_full_manifest_contract(
    bytes: &[u8],
    reference: &ReleaseReference,
) -> Result<ReleaseManifest, ContractError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ContractError::new(
            ContractErrorKind::ManifestTooLarge,
            "release manifest exceeds 64 KiB",
        ));
    }
    if digest(bytes) != reference.sha256 {
        return Err(ContractError::new(
            ContractErrorKind::DigestMismatch,
            "release manifest digest does not match the requested release",
        ));
    }
    let manifest: ReleaseManifest =
        serde_json::from_slice(bytes).map_err(|_| invalid_manifest())?;
    if canonical_bytes_contract(&manifest)? != bytes {
        return Err(ContractError::new(
            ContractErrorKind::InvalidManifest,
            "release manifest is not in canonical JSON form",
        ));
    }
    validate_manifest(&manifest, Some(reference))?;
    Ok(manifest)
}

pub(crate) fn validate_manifest(
    manifest: &ReleaseManifest,
    reference: Option<&ReleaseReference>,
) -> Result<(), ContractError> {
    if manifest.schema != RELEASE_SCHEMA
        || !valid_identifier(&manifest.issuer)
        || !valid_release_name(&manifest.name)
        || manifest.targets.is_empty()
    {
        return Err(ContractError::new(
            ContractErrorKind::IdentityMismatch,
            "release manifest identity or schema is invalid",
        ));
    }
    if reference.is_some_and(|reference| {
        manifest.issuer != reference.issuer || manifest.name != reference.name
    }) {
        return Err(ContractError::new(
            ContractErrorKind::IdentityMismatch,
            "release manifest identity or schema does not match the request",
        ));
    }
    validate_selectors(&manifest.selectors)?;
    for (system, target) in &manifest.targets {
        if system.is_empty()
            || system.len() > 128
            || system.chars().any(char::is_whitespace)
            || system.chars().any(char::is_control)
        {
            return Err(invalid_manifest_with("target system is invalid"));
        }
        validate_store_path(&target.rootfs)?;
        if !valid_identifier(&target.cache) {
            return Err(invalid_manifest_with("cache identity is invalid"));
        }
        validate_process(target.process.as_ref())?;
        let mut destinations = HashSet::new();
        for mount in &target.mounts {
            validate_store_path(&mount.source)?;
            validate_destination(&mount.destination)?;
            if !destinations.insert(&mount.destination) {
                return Err(invalid_manifest_with("mount destinations must be unique"));
            }
        }
    }
    for evidence in [manifest.sbom.as_ref(), manifest.provenance.as_ref()]
        .into_iter()
        .flatten()
    {
        if evidence.uri.is_empty()
            || evidence.uri.len() > 4096
            || evidence.uri.chars().any(char::is_control)
            || evidence.sha256.len() != 64
            || !evidence
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid_manifest_with("evidence reference is invalid"));
        }
    }
    Ok(())
}

fn validate_store_path(value: &str) -> Result<(), ContractError> {
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(invalid_manifest_with(
            "store path is not a canonical Nix store path",
        ));
    }
    let Some(basename) = value.strip_prefix("/nix/store/") else {
        return Err(invalid_manifest_with(
            "store path is not a canonical Nix store path",
        ));
    };
    if basename.contains('/') {
        return Err(invalid_manifest_with(
            "store path is not a canonical Nix store path",
        ));
    }
    let Some((hash, suffix)) = basename.split_at_checked(32) else {
        return Err(invalid_manifest_with(
            "store path is not a canonical Nix store path",
        ));
    };
    const NIX_BASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    let Some(suffix) = suffix.strip_prefix('-') else {
        return Err(invalid_manifest_with(
            "store path is not a canonical Nix store path",
        ));
    };
    if !hash.bytes().all(|byte| NIX_BASE32.contains(&byte))
        || suffix.is_empty()
        || !suffix.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.' | b'_' | b'?' | b'=')
        })
    {
        return Err(invalid_manifest_with(
            "store path is not a canonical Nix store path",
        ));
    }
    Ok(())
}

fn validate_selectors(selectors: &Selectors) -> Result<(), ContractError> {
    for name in selectors
        .containers
        .iter()
        .chain(&selectors.skip_containers)
    {
        if !valid_identifier(name) {
            return Err(invalid_manifest_with(
                "container selector is not a Kubernetes DNS label",
            ));
        }
    }
    Ok(())
}

fn validate_process(process: Option<&ProcessMetadata>) -> Result<(), ContractError> {
    let Some(process) = process else {
        return Ok(());
    };
    if process
        .entrypoint
        .as_ref()
        .is_some_and(|parts| parts.is_empty())
    {
        return Err(invalid_manifest_with(
            "entrypoint must not be empty when supplied",
        ));
    }
    for value in process
        .entrypoint
        .iter()
        .flatten()
        .chain(process.default_args.iter().flatten())
    {
        if value.is_empty() || value.contains('\0') {
            return Err(invalid_manifest_with(
                "process arguments must be non-empty and contain no NUL",
            ));
        }
    }
    if let Some(cwd) = &process.working_directory {
        validate_destination(cwd)?;
    }
    let mut names = HashSet::new();
    for entry in &process.environment {
        if entry.name.is_empty()
            || entry.name.contains('=')
            || entry.name.contains('\0')
            || entry.value.contains('\0')
            || !names.insert(&entry.name)
        {
            return Err(invalid_manifest_with(
                "environment contains an invalid or duplicate name/value",
            ));
        }
    }
    Ok(())
}

fn validate_destination(value: &str) -> Result<(), ContractError> {
    let path = Path::new(value);
    if !path.is_absolute()
        || value.len() > 4096
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || value.contains('\0')
    {
        return Err(invalid_manifest_with(
            "path must be absolute and normalized",
        ));
    }
    Ok(())
}

fn invalid_manifest() -> ContractError {
    invalid_manifest_with("release manifest is invalid")
}

fn invalid_manifest_with(diagnostic: &'static str) -> ContractError {
    ContractError::new(ContractErrorKind::InvalidManifest, diagnostic)
}

fn map_contract_error(error: ContractError) -> ResolutionError {
    let category = match error.kind {
        ContractErrorKind::DigestMismatch => ErrorCategory::DigestMismatch,
        ContractErrorKind::IdentityMismatch => ErrorCategory::PolicyDenied,
        ContractErrorKind::ManifestTooLarge => ErrorCategory::ManifestFetch,
        ContractErrorKind::InvalidReference | ContractErrorKind::InvalidManifest => {
            ErrorCategory::InvalidRequest
        }
    };
    ResolutionError::new(category, error.diagnostic, false)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ResolverPolicy {
    pub system: String,
    // When true the node resolves digest-addressed releases only (substitute a
    // CI-built closure); it never evaluates a flake. Set false to allow node-side
    // evaluation of a flake ref. Defaults to true so a node evaluates nothing
    // unless the operator opts in.
    #[serde(default = "default_cache_only")]
    pub cache_only: bool,
    #[serde(default)]
    pub eval_allowed_uri_prefixes: Vec<String>,
    #[serde(default)]
    pub issuers: HashMap<String, IssuerPolicy>,
    // Node-side evaluation runs in a private mount namespace that exposes only
    // the store, system trees, and the staged source (SPEC §2.3), so no flake
    // input — direct or transitive — can read the node's filesystem. Set true
    // only on a node that cannot create mount namespaces; the default, like
    // every other field here, is the safe one.
    #[serde(default)]
    pub unconfined_evaluation: bool,
    // An in-image flake with inputs must ship a complete flake.lock. Set true
    // to let the node lock such a seed's inputs at evaluation time instead —
    // non-reproducible, and meant for development nodes.
    #[serde(default)]
    pub allow_unlocked_inputs: bool,
}

fn default_cache_only() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct IssuerPolicy {
    pub source: ManifestSource,
    #[serde(default)]
    pub allowed_releases: Vec<String>,
    #[serde(default)]
    pub caches: HashMap<String, CachePolicy>,
    // Minisign public keys (the base64 line `minisign -P` takes) whose
    // signatures this node accepts on the issuer's manifests (SPEC §6.1).
    // Listing several is how a key is rotated.
    #[serde(default)]
    pub signing_keys: Vec<String>,
    // Accept this issuer's manifests without a signature: anyone who can
    // write the catalog then chooses what runs. Exclusive with signing_keys,
    // and never the default.
    #[serde(default)]
    pub allow_unsigned: bool,
    // Manifest digests this node refuses even when validly signed: a release
    // withdrawn after it was published, which a compromised catalog could
    // otherwise re-point a channel at.
    #[serde(default)]
    pub revoked_manifests: Vec<String>,
}

impl IssuerPolicy {
    pub fn permits_release(&self, name: &str) -> bool {
        self.allowed_releases.iter().any(|pattern| {
            pattern == "*"
                || pattern == name
                || pattern
                    .strip_suffix('*')
                    .is_some_and(|prefix| name.starts_with(prefix))
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ManifestSource {
    Local { directory: PathBuf },
    Https { base_url: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CachePolicy {
    pub substituter: String,
    #[serde(default)]
    pub public_keys: Vec<String>,
}

pub(crate) fn validate_policy(
    policy: &ResolverPolicy,
    daemon_uid: u32,
) -> Result<(), ResolutionError> {
    if policy.system.is_empty()
        || policy.system.len() > 128
        || policy.system.chars().any(char::is_whitespace)
        || policy.system.chars().any(char::is_control)
    {
        return Err(invalid_policy("system is invalid"));
    }
    for prefix in &policy.eval_allowed_uri_prefixes {
        if prefix.is_empty()
            || prefix.len() > 4096
            || prefix.chars().any(char::is_whitespace)
            || prefix.chars().any(char::is_control)
        {
            return Err(invalid_policy("evaluation URI prefix is invalid"));
        }
    }
    for (identity, issuer) in &policy.issuers {
        if !valid_identifier(identity) {
            return Err(invalid_policy("issuer identity is invalid"));
        }
        match &issuer.source {
            ManifestSource::Local { directory } => {
                if !directory.is_absolute() {
                    return Err(invalid_policy("local issuer catalog must be absolute"));
                }
                let canonical = std::fs::canonicalize(directory)
                    .map_err(|_| invalid_policy("local issuer catalog is inaccessible"))?;
                let metadata = std::fs::metadata(&canonical)
                    .map_err(|_| invalid_policy("local issuer catalog is inaccessible"))?;
                if canonical != *directory
                    || !metadata.is_dir()
                    || metadata.uid() != daemon_uid
                    || metadata.mode() & 0o022 != 0
                {
                    return Err(invalid_policy(
                        "local issuer catalog must be canonical, daemon-owned, and not group/world writable",
                    ));
                }
            }
            ManifestSource::Https { base_url } => {
                if !base_url.starts_with("https://")
                    || base_url.contains(['?', '#'])
                    || base_url.chars().any(char::is_whitespace)
                {
                    return Err(invalid_policy("issuer catalog URL is invalid"));
                }
            }
        }
        if issuer.allowed_releases.is_empty() {
            return Err(invalid_policy("issuer has no allowed releases"));
        }
        for pattern in &issuer.allowed_releases {
            let valid = pattern == "*"
                || valid_release_name(pattern)
                || pattern
                    .strip_suffix('*')
                    .is_some_and(|prefix| !prefix.is_empty() && valid_release_prefix(prefix));
            if !valid {
                return Err(invalid_policy("allowed release pattern is invalid"));
            }
        }
        if issuer.caches.is_empty() {
            return Err(invalid_policy("issuer has no authorized caches"));
        }
        match (issuer.signing_keys.is_empty(), issuer.allow_unsigned) {
            (true, false) => {
                return Err(invalid_policy(
                    "issuer has no manifest signing keys; list signing_keys, or set allow_unsigned to accept unsigned manifests",
                ))
            }
            (false, true) => {
                return Err(invalid_policy(
                    "issuer sets both signing_keys and allow_unsigned",
                ))
            }
            _ => {}
        }
        let mut key_ids = HashSet::new();
        for key in &issuer.signing_keys {
            let key = minisign::PublicKey::parse(key)
                .ok_or_else(|| invalid_policy("issuer signing key is not a minisign public key"))?;
            if !key_ids.insert(key.key_id()) {
                return Err(invalid_policy(
                    "issuer lists two signing keys with one key id",
                ));
            }
        }
        if !issuer
            .revoked_manifests
            .iter()
            .all(|digest| valid_digest(digest))
        {
            return Err(invalid_policy(
                "revoked manifest is not a 64-digit lowercase hex digest",
            ));
        }
        for (identity, cache) in &issuer.caches {
            if !valid_identifier(identity)
                || !(cache.substituter.starts_with("https://")
                    || cache.substituter.starts_with("file:///"))
                || cache.substituter.chars().any(char::is_whitespace)
                || cache.substituter.chars().any(char::is_control)
                || cache.public_keys.iter().any(|key| {
                    key.is_empty() || key.chars().any(char::is_whitespace) || !key.contains(':')
                })
            {
                return Err(invalid_policy("authorized cache policy is invalid"));
            }
        }
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_release_prefix(value: &str) -> bool {
    value.trim_end_matches('/').split('/').all(valid_identifier)
}

fn invalid_policy(diagnostic: &'static str) -> ResolutionError {
    ResolutionError::new(ErrorCategory::PolicyDenied, diagnostic, false)
}

pub(crate) fn fetch_manifest(
    source: &ManifestSource,
    digest: &str,
    timeout: Duration,
) -> Result<Vec<u8>, ResolutionError> {
    fetch_catalog_file(
        source,
        &format!("{digest}.json"),
        MAX_MANIFEST_BYTES,
        timeout,
        fetch_error,
        fetch_too_large,
    )
}

/// Refuses a digest node policy revoked for its issuer, before anything is
/// fetched: a revoked release fails the same way whether or not its catalog
/// is reachable.
pub(crate) fn refuse_revoked(
    issuer: &IssuerPolicy,
    reference: &ReleaseReference,
) -> Result<(), ResolutionError> {
    if issuer.revoked_manifests.contains(&reference.sha256) {
        return Err(ResolutionError::new(
            ErrorCategory::PolicyDenied,
            "release manifest is revoked by node policy",
            false,
        ));
    }
    Ok(())
}

/// Refuses a manifest the issuer's node policy does not authenticate: unless
/// the issuer is `allow_unsigned`, one without a sidecar signature by one of
/// the issuer's keys (SPEC §6.1). The digest is checked first, so a catalog
/// serving the wrong bytes is reported as that rather than as a bad
/// signature.
pub(crate) fn authenticate_manifest(
    issuer: &IssuerPolicy,
    reference: &ReleaseReference,
    bytes: &[u8],
    timeout: Duration,
) -> Result<(), ResolutionError> {
    if digest(bytes) != reference.sha256 {
        return Err(ResolutionError::new(
            ErrorCategory::DigestMismatch,
            "release manifest digest does not match the requested release",
            false,
        ));
    }
    if issuer.allow_unsigned {
        return Ok(());
    }
    let sidecar = fetch_catalog_file(
        &issuer.source,
        &format!("{}.json.minisig", reference.sha256),
        minisign::MAX_SIGNATURE_BYTES,
        timeout,
        signature_fetch_error,
        signature_too_large,
    )?;
    let keys: Vec<minisign::PublicKey> = issuer
        .signing_keys
        .iter()
        .filter_map(|key| minisign::PublicKey::parse(key))
        .collect();
    minisign::verify(&sidecar, bytes, &keys).map_err(|error| {
        let diagnostic = match error {
            minisign::SignatureError::Malformed => {
                "release manifest signature is not a minisign signature"
            }
            minisign::SignatureError::UnknownKey => {
                "release manifest is not signed by a key this node trusts for its issuer"
            }
            minisign::SignatureError::Invalid => "release manifest signature does not verify",
        };
        ResolutionError::new(ErrorCategory::PolicyDenied, diagnostic, false)
    })
}

/// Reads `sha256/<name>` from an issuer catalog, at most `limit` bytes.
fn fetch_catalog_file(
    source: &ManifestSource,
    name: &str,
    limit: usize,
    timeout: Duration,
    fetch_error: fn() -> ResolutionError,
    too_large: fn() -> ResolutionError,
) -> Result<Vec<u8>, ResolutionError> {
    match source {
        ManifestSource::Local { directory } => {
            let path = directory.join("sha256").join(name);
            let canonical_directory =
                std::fs::canonicalize(directory).map_err(|_| fetch_error())?;
            let canonical_path = std::fs::canonicalize(&path).map_err(|_| fetch_error())?;
            if !canonical_path.starts_with(&canonical_directory) {
                return Err(ResolutionError::new(
                    ErrorCategory::PolicyDenied,
                    "manifest path escaped its issuer catalog",
                    false,
                ));
            }
            let bytes = std::fs::read(canonical_path).map_err(|_| fetch_error())?;
            if bytes.len() > limit {
                return Err(too_large());
            }
            Ok(bytes)
        }
        ManifestSource::Https { base_url } => {
            if !base_url.starts_with("https://") || base_url.contains(['?', '#']) {
                return Err(ResolutionError::new(
                    ErrorCategory::PolicyDenied,
                    "issuer manifest URL must use HTTPS and contain no query or fragment",
                    false,
                ));
            }
            let url = format!("{}/sha256/{name}", base_url.trim_end_matches('/'));
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(timeout))
                .max_redirects(0)
                .build()
                .into();
            let mut response = agent.get(&url).call().map_err(|_| fetch_error())?;
            let bytes = response
                .body_mut()
                .with_config()
                .limit((limit + 1) as u64)
                .read_to_vec()
                .map_err(|_| fetch_error())?;
            if bytes.len() > limit {
                return Err(too_large());
            }
            Ok(bytes)
        }
    }
}

fn fetch_error() -> ResolutionError {
    ResolutionError::new(
        ErrorCategory::ManifestFetch,
        "release manifest could not be fetched from the node-authorized issuer catalog",
        true,
    )
}

fn signature_fetch_error() -> ResolutionError {
    ResolutionError::new(
        ErrorCategory::ManifestFetch,
        "release manifest signature could not be fetched from the node-authorized issuer catalog",
        true,
    )
}

fn signature_too_large() -> ResolutionError {
    ResolutionError::new(
        ErrorCategory::ManifestFetch,
        "release manifest signature exceeds 4 KiB",
        false,
    )
}

fn fetch_too_large() -> ResolutionError {
    ResolutionError::new(
        ErrorCategory::ManifestFetch,
        "release manifest exceeds 64 KiB",
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> ReleaseManifest {
        ReleaseManifest {
            schema: RELEASE_SCHEMA.to_string(),
            issuer: "test".to_string(),
            name: "agent".to_string(),
            selectors: Selectors::default(),
            targets: BTreeMap::from([(
                "x86_64-linux".to_string(),
                ReleaseTarget {
                    rootfs: "/nix/store/00000000000000000000000000000000-rootfs".to_string(),
                    cache: "default".to_string(),
                    process: Some(ProcessMetadata {
                        entrypoint: Some(vec!["/bin/agent".to_string()]),
                        default_args: Some(vec!["serve".to_string()]),
                        working_directory: Some("/workspace".to_string()),
                        environment: vec![EnvironmentEntry {
                            name: "HOME".to_string(),
                            value: "/home/agent".to_string(),
                            allow_workload_override: false,
                        }],
                    }),
                    mounts: Vec::new(),
                },
            )]),
            sbom: None,
            provenance: None,
        }
    }

    fn reference(bytes: &[u8]) -> ReleaseReference {
        ReleaseReference::parse_contract(&format!("test/agent@sha256:{}", digest(bytes))).unwrap()
    }

    fn policy_with_issuer(issuer: serde_json::Value) -> ResolverPolicy {
        let mut base = serde_json::json!({
            "source": { "kind": "https", "base_url": "https://releases.example" },
            "allowed_releases": ["*"],
            "caches": { "main": { "substituter": "https://cache.example" } },
        });
        base.as_object_mut()
            .unwrap()
            .extend(issuer.as_object().unwrap().clone());
        serde_json::from_value(serde_json::json!({
            "system": "x86_64-linux",
            "issuers": { "acme": base },
        }))
        .unwrap()
    }

    #[test]
    fn issuers_must_choose_between_signing_keys_and_unsigned() {
        let secret = ed25519_dalek::SigningKey::from_bytes(&[5; 32]);
        let key = crate::minisign::tests::public_key_line(&secret, *b"releases");
        let rotated = crate::minisign::tests::public_key_line(
            &ed25519_dalek::SigningKey::from_bytes(&[6; 32]),
            *b"rotated1",
        );
        let diagnostic = |issuer| {
            validate_policy(&policy_with_issuer(issuer), 0)
                .err()
                .map(|error| error.diagnostic)
        };

        assert_eq!(
            diagnostic(serde_json::json!({ "signing_keys": [key] })),
            None
        );
        assert_eq!(
            diagnostic(serde_json::json!({ "signing_keys": [key, rotated] })),
            None
        );
        assert_eq!(
            diagnostic(serde_json::json!({ "allow_unsigned": true })),
            None
        );
        assert!(diagnostic(serde_json::json!({}))
            .unwrap()
            .contains("no manifest signing keys"));
        assert!(
            diagnostic(serde_json::json!({ "signing_keys": [key], "allow_unsigned": true }))
                .unwrap()
                .contains("both")
        );
        assert!(diagnostic(serde_json::json!({ "signing_keys": ["RWQ"] }))
            .unwrap()
            .contains("not a minisign public key"));
        assert!(
            diagnostic(serde_json::json!({ "signing_keys": [key, key] }))
                .unwrap()
                .contains("one key id")
        );
        let digest = "a".repeat(64);
        assert_eq!(
            diagnostic(serde_json::json!({ "signing_keys": [key], "revoked_manifests": [digest] })),
            None
        );
        for bad in [
            "A".repeat(64),
            format!("sha256:{}", "a".repeat(64)),
            "a".repeat(63),
        ] {
            assert!(diagnostic(
                serde_json::json!({ "signing_keys": [key], "revoked_manifests": [bad] })
            )
            .unwrap()
            .contains("revoked manifest"),);
        }
    }

    #[test]
    fn canonical_manifest_round_trips_and_binds_identity() {
        let manifest = manifest();
        let bytes = canonical_bytes_contract(&manifest).unwrap();
        assert_eq!(
            parse_full_manifest_contract(&bytes, &reference(&bytes)).unwrap(),
            manifest
        );

        let pretty = serde_json::to_vec_pretty(&manifest).unwrap();
        assert_eq!(
            parse_full_manifest_contract(&pretty, &reference(&pretty))
                .unwrap_err()
                .kind,
            ContractErrorKind::InvalidManifest
        );
        assert_eq!(
            parse_full_manifest_contract(b"{}", &reference(&bytes))
                .unwrap_err()
                .kind,
            ContractErrorKind::DigestMismatch
        );
    }

    #[test]
    fn references_and_manifest_metadata_are_strictly_validated() {
        assert!(
            ReleaseReference::parse_contract(&format!("test/agent@sha256:{}", "A".repeat(64)))
                .is_err()
        );

        let mut invalid = manifest();
        invalid.targets.get_mut("x86_64-linux").unwrap().process = Some(ProcessMetadata {
            entrypoint: Some(Vec::new()),
            default_args: None,
            working_directory: None,
            environment: Vec::new(),
        });
        assert_eq!(
            validate_manifest(&invalid, None).unwrap_err().kind,
            ContractErrorKind::InvalidManifest
        );

        let mut wrong_identity = manifest();
        wrong_identity.issuer = "other".to_string();
        let bytes = canonical_bytes_contract(&wrong_identity).unwrap();
        assert_eq!(
            parse_full_manifest_contract(&bytes, &reference(&bytes))
                .unwrap_err()
                .kind,
            ContractErrorKind::IdentityMismatch
        );
    }
}
