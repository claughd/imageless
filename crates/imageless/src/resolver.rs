//! The resolver daemon: policy loading, single-flight materialization, the
//! development-source staging pipeline, and the UNIX-socket server.

use crate::client::{effective_uid, peer_allowed, peer_uid, read_frame, write_frame};
use crate::confine::Confinement;
use crate::gc::{gc_root_path, prepare_gc_root, remove_bundle_gc_roots, validate_registered_root};
use crate::materialize::{
    elapsed_us, remaining, ClosurePathReport, ClosureReport, ErrorCategory, Materialize,
    ResolutionError, ResolutionSuccess, ResolutionTimings, ResolvePurpose, ResolveRequest,
    ResolveResponse,
};
use crate::nix::{parse_materialized_path, run_command, validate_realise_output};
use crate::release::{self, CachePolicy, ReleaseReference, ResolvedRelease, ResolverPolicy};
use crate::spec::{validate_output, validate_store_path};
use crate::{
    to_io, GC_ROOT_NAME, MAX_ANNOTATION_VALUE_BYTES, MAX_CONNECTIONS, MAX_FRAME_BYTES,
    MAX_STAGED_SOURCE_BYTES, MAX_STAGED_SOURCE_ENTRIES, PROTOCOL_VERSION,
};
use serde::Deserialize;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// The node policy file consulted by in-process materialization when
/// `IMAGELESS_POLICY` does not name another location.
pub const DEFAULT_POLICY_PATH: &str = "/etc/imageless/policy.json";

/// Where in-process materialization reads its node policy from. Both variants
/// carry the same trust as whoever supplies them: a file gated by the
/// [`load_resolver_policy`] ownership check, or inline JSON handed in through
/// the daemon environment. The file exists so the loader has an ownership
/// anchor; when the operator sets the daemon's environment instead, that
/// environment is an equivalent anchor and nothing needs to land on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicySource {
    /// A policy file: `Some(path)` loads that exact file and never defaults;
    /// `None` consults the node default path, which falls back to a
    /// fail-closed cache-only policy when the file is absent.
    File(Option<PathBuf>),
    /// Inline policy JSON supplied out-of-band (e.g. `IMAGELESS_POLICY_JSON`).
    /// There is no file to own, so the ownership check does not apply; whoever
    /// set the value is the trust anchor. Never defaults — malformed or
    /// oversized JSON fails closed.
    ///
    /// Gated behind the `inline-policy` cargo feature: a production build omits
    /// this variant entirely, so it cannot read policy from the environment.
    #[cfg(feature = "inline-policy")]
    Inline(String),
}

pub struct ResolverConfig {
    pub max_realizations: usize,
    pub max_timeout: Duration,
    pub nix: PathBuf,
    pub nix_store: PathBuf,
    pub policy: ResolverPolicy,
    pub development_worker: Option<DevelopmentWorkerConfig>,
    /// Without a development worker, evaluate development sources directly as
    /// the calling user instead of refusing. In-process materialization sets
    /// this; the daemon never does — it keeps requiring the unprivileged
    /// worker so evaluation cannot run with daemon privileges.
    pub evaluate_as_caller: bool,
}

#[derive(Clone, Debug)]
pub struct DevelopmentWorkerConfig {
    pub program: PathBuf,
    pub user: String,
}

impl ResolverConfig {
    pub fn from_environment(max_realizations: usize, timeout_seconds: u64) -> Self {
        Self {
            max_realizations,
            max_timeout: Duration::from_secs(timeout_seconds),
            nix: std::env::var_os("IMAGELESS_NIX")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(option_env!("IMAGELESS_NIX").unwrap_or("nix"))),
            nix_store: std::env::var_os("IMAGELESS_NIX_STORE")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(option_env!("IMAGELESS_NIX_STORE").unwrap_or("nix-store"))
                }),
            // Fail-closed placeholder policy. Every caller replaces it with a
            // loaded node policy (the daemon from `--policy-file`, in-process
            // materialization from the policy file or inline JSON), so there is
            // deliberately no environment switch that can loosen it here.
            policy: ResolverPolicy {
                system: std::env::var("IMAGELESS_SYSTEM")
                    .unwrap_or_else(|_| "x86_64-linux".to_string()),
                cache_only: true,
                eval_allowed_uri_prefixes: Vec::new(),
                issuers: HashMap::new(),
                unconfined_evaluation: false,
                allow_unlocked_inputs: false,
            },
            development_worker: None,
            evaluate_as_caller: false,
        }
    }
}

/// Materialize a request inside the calling process instead of a resolver
/// daemon: load the node policy (an explicit path must load; the default path
/// falls back to a fail-closed cache-only policy when absent) and drive a
/// process-local [`Resolver`] as the calling user. There is no cross-process
/// single-flight in this mode.
pub fn resolve_in_process(
    source: &PolicySource,
    request: &ResolveRequest,
) -> Result<ResolutionSuccess, ResolutionError> {
    resolve_in_process_at(source, Path::new(DEFAULT_POLICY_PATH), request)
}

fn resolve_in_process_at(
    source: &PolicySource,
    default_path: &Path,
    request: &ResolveRequest,
) -> Result<ResolutionSuccess, ResolutionError> {
    let (policy, defaulted) = in_process_policy(source, default_path)?;
    let mut config = ResolverConfig::from_environment(2, 3600);
    config.policy = policy;
    config.evaluate_as_caller = true;
    let result = Resolver::new(config).resolve_detailed(request.clone());
    if defaulted {
        result.map_err(|error| default_policy_hint(error, default_path))
    } else {
        result
    }
}

fn in_process_policy(
    source: &PolicySource,
    default_path: &Path,
) -> Result<(ResolverPolicy, bool), ResolutionError> {
    match source {
        // Inline policy: the daemon environment is the trust anchor, so there is
        // no file to own. It never defaults — invalid input fails closed.
        #[cfg(feature = "inline-policy")]
        PolicySource::Inline(json) => {
            if json.len() > 1024 * 1024 {
                return Err(ResolutionError::new(
                    ErrorCategory::PolicyDenied,
                    "inline resolver policy (IMAGELESS_POLICY_JSON) exceeds 1 MiB".to_string(),
                    false,
                ));
            }
            let policy = parse_policy_bytes(json.as_bytes()).map_err(|error| {
                ResolutionError::new(
                    ErrorCategory::PolicyDenied,
                    format!(
                        "inline resolver policy (IMAGELESS_POLICY_JSON) could not be loaded: {error}"
                    ),
                    false,
                )
            })?;
            Ok((policy, false))
        }
        PolicySource::File(policy_path) => {
            let (path, defaultable) = match policy_path {
                Some(path) => (path.as_path(), false),
                None => (default_path, true),
            };
            match load_resolver_policy(path) {
                Ok(policy) => Ok((policy, false)),
                Err(error) if defaultable && error.kind() == io::ErrorKind::NotFound => Ok((
                    ResolverPolicy {
                        system: std::env::var("IMAGELESS_SYSTEM")
                            .unwrap_or_else(|_| "x86_64-linux".to_string()),
                        cache_only: true,
                        eval_allowed_uri_prefixes: Vec::new(),
                        issuers: HashMap::new(),
                        unconfined_evaluation: false,
                        allow_unlocked_inputs: false,
                    },
                    true,
                )),
                Err(error) => Err(ResolutionError::new(
                    ErrorCategory::PolicyDenied,
                    format!(
                        "node resolver policy {} could not be loaded: {error}",
                        path.display()
                    ),
                    false,
                )),
            }
        }
    }
}

/// Where a diagnostic stops addressing whoever owns the workload and starts
/// addressing whoever configures the node.
///
/// Everything from this marker onward names a node-local path and a node-local
/// remedy: true, useful, and meaningless to a tenant who cannot act on it and
/// should not be handed the node's filesystem layout. [`default_policy_hint`]
/// is the only producer, and it appends beginning exactly here, so the relay
/// into the runtime log can cut at one shared constant rather than guess.
pub(crate) const OPERATOR_HINT: &str = "; node policy does not permit this";

pub(crate) fn default_policy_hint(error: ResolutionError, default_path: &Path) -> ResolutionError {
    match error.category {
        ErrorCategory::EvaluationDisabled | ErrorCategory::PolicyDenied => ResolutionError::new(
            error.category,
            format!(
                "{}{OPERATOR_HINT} — write {} to authorize releases or development sources",
                error.diagnostic,
                default_path.display()
            ),
            error.retryable,
        ),
        _ => error,
    }
}

pub fn load_resolver_policy(path: &Path) -> io::Result<ResolverPolicy> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "resolver policy must be a daemon-owned regular file that is not group/world writable",
        ));
    }
    if metadata.len() > 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resolver policy exceeds 1 MiB",
        ));
    }
    parse_policy_bytes(&std::fs::read(path)?)
}

/// Parse and validate policy JSON regardless of where the bytes came from (a
/// file that already passed the ownership check, or inline environment input).
/// The trust decision belongs to the caller; this only enforces shape.
fn parse_policy_bytes(bytes: &[u8]) -> io::Result<ResolverPolicy> {
    let policy: ResolverPolicy = serde_json::from_slice(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("resolver policy is invalid: {error}"),
        )
    })?;
    release::validate_policy(&policy, effective_uid()).map_err(to_io)?;
    Ok(policy)
}

#[derive(Clone)]
pub struct Resolver {
    inner: Arc<ResolverInner>,
}

struct ResolverInner {
    config: ResolverConfig,
    flights: Mutex<HashMap<String, Arc<Flight>>>,
    permits: Mutex<usize>,
    permit_ready: Condvar,
}

/// Sub-stage durations accumulated over one caller's resolution.
///
/// `Cell` rather than return values: these spans are taken at four different
/// depths of the materialize call tree, and handing tuples back up would put a
/// timing field in the signature of every function they pass through. One
/// caller owns one clock for the length of its own resolution and no other
/// thread touches it, so there is nothing here to synchronize — a follower on a
/// coalesced flight has its own clock and records only the root registration it
/// actually performed.
#[derive(Default)]
struct StageClock {
    manifest_fetch_us: Cell<u64>,
    staging_us: Cell<u64>,
    evaluation_us: Cell<u64>,
    root_registration_us: Cell<u64>,
}

impl StageClock {
    /// Run `call`, adding its duration to `field`. Additive because a single
    /// resolution can enter the same stage twice — a release with mounts
    /// realises more than one store path.
    fn time<T>(field: &Cell<u64>, call: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let value = call();
        field.set(field.get().saturating_add(elapsed_us(started)));
        value
    }
}

struct Flight {
    state: Mutex<FlightState>,
    ready: Condvar,
}

struct FlightState {
    callers: usize,
    result: Option<Result<ResolvedRelease, ResolutionError>>,
}

#[derive(Clone)]
struct SelectedRelease {
    resolution: ResolvedRelease,
    cache: CachePolicy,
}

#[cfg_attr(not(feature = "daemon"), allow(dead_code))]
impl Resolver {
    pub fn new(config: ResolverConfig) -> Self {
        assert!((1..=64).contains(&config.max_realizations));
        Self {
            inner: Arc::new(ResolverInner {
                config,
                flights: Mutex::new(HashMap::new()),
                permits: Mutex::new(0),
                permit_ready: Condvar::new(),
            }),
        }
    }

    pub fn resolve(&self, request: ResolveRequest) -> Result<ResolvedRelease, ResolutionError> {
        self.resolve_detailed(request)
            .map(|success| success.resolution)
    }

    pub fn resolve_detailed(
        &self,
        request: ResolveRequest,
    ) -> Result<ResolutionSuccess, ResolutionError> {
        let (bundle, deadline) = self.validate_request(&request)?;
        if request.purpose == ResolvePurpose::Inspect {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "inspection requests cannot materialize a release",
                false,
            ));
        }

        // Selection and policy enforcement are deliberately per caller. The
        // subsequent Nix work is single-flighted by immutable release identity,
        // but a concurrent prewarm or differently named container must never
        // lend its selector decision to another request.
        let clock = StageClock::default();
        let policy_started = Instant::now();
        let selected = match &request.materialize {
            Materialize::Release(reference) => Some(self.select_release(
                reference,
                request.container_name.as_deref(),
                request.purpose,
                deadline,
                &clock,
            )?),
            _ => None,
        };
        let policy_verification_us = elapsed_us(policy_started);
        let substitution_started = Instant::now();

        let key = request.materialize.key();
        let result = loop {
            let (flight, leader) = self.join_flight(&key);
            let result = if leader {
                let result = self.materialize(
                    &request.materialize,
                    selected.as_ref(),
                    &bundle,
                    deadline,
                    &clock,
                );
                // A failure is the leader's own: its deadline, its bundle, a
                // fetch that may succeed a moment later. Unpublish the flight
                // before waking anyone, so no later arrival joins a result that
                // is already an error.
                if result.is_err() {
                    self.forget_flight(&key, &flight);
                }
                let mut state = flight.state.lock().unwrap();
                state.result = Some(result.clone());
                flight.ready.notify_all();
                result
            } else {
                match wait_for_flight(&flight, deadline) {
                    Ok(resolution) => {
                        self.register_resolution(&resolution, &bundle, deadline, &clock)
                    }
                    Err(error) => Err(error),
                }
            };
            self.release_flight(&key, &flight);
            // A follower that inherited a retryable failure — typically the
            // leader running out of its own, shorter deadline — tries again on
            // its own clock rather than failing with someone else's error. A
            // policy or contract error is the same for every caller of this
            // key, so it is shared as before. Rejoining can only make this
            // caller a leader or a follower of a fresh flight, and leaders
            // never loop, so each follower retries at most once per failure.
            match &result {
                Err(error)
                    if !leader
                        && error.retryable
                        && remaining(deadline, "before retrying a failed realization").is_ok() =>
                {
                    continue
                }
                _ => break result,
            }
        };

        if result.is_err() {
            let _ = remove_bundle_gc_roots(&bundle);
        }
        result.map(|resolution| ResolutionSuccess {
            resolution,
            timings: ResolutionTimings {
                // The outer spans keep their original boundaries so the numbers
                // stay comparable across this change; the fields below are
                // carved out of them rather than added beside them.
                policy_verification_us,
                substitution_us: elapsed_us(substitution_started),
                manifest_fetch_us: clock.manifest_fetch_us.get(),
                staging_us: clock.staging_us.get(),
                evaluation_us: clock.evaluation_us.get(),
                root_registration_us: clock.root_registration_us.get(),
            },
        })
    }

    pub fn inspect(
        &self,
        request: ResolveRequest,
    ) -> Result<(ResolutionSuccess, ClosureReport), ResolutionError> {
        let (_, deadline) = self.validate_request(&request)?;
        if request.purpose != ResolvePurpose::Inspect {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "closure inspection requires an inspection request",
                false,
            ));
        }
        let Materialize::Release(reference) = &request.materialize else {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "only production releases can be inspected",
                false,
            ));
        };
        let clock = StageClock::default();
        let policy_started = Instant::now();
        let selected =
            self.select_release(reference, None, ResolvePurpose::Inspect, deadline, &clock)?;
        let policy_verification_us = elapsed_us(policy_started);
        let substitution_started = Instant::now();
        let closure = self.query_closure(&selected, deadline)?;
        let success = ResolutionSuccess {
            resolution: selected.resolution,
            timings: ResolutionTimings {
                policy_verification_us,
                substitution_us: elapsed_us(substitution_started),
                manifest_fetch_us: clock.manifest_fetch_us.get(),
                // Inspection stages nothing, evaluates nothing, and registers no
                // root: it answers what a release would cost, without paying it.
                ..ResolutionTimings::default()
            },
        };
        Ok((success, closure))
    }

    fn validate_request(
        &self,
        request: &ResolveRequest,
    ) -> Result<(PathBuf, Instant), ResolutionError> {
        if request.version != PROTOCOL_VERSION {
            return Err(ResolutionError::new(
                ErrorCategory::Protocol,
                "unsupported protocol version",
                false,
            ));
        }
        if request.timeout_ms == 0 || request.timeout_ms > 3_600_000 {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "timeout must be between 1ms and 3600s",
                false,
            ));
        }
        if matches!(
            request.purpose,
            ResolvePurpose::Prewarm | ResolvePurpose::Inspect
        ) && (!matches!(request.materialize, Materialize::Release(_))
            || request.container_name.is_some())
        {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "prewarm and inspection requests require a release and no runtime container identity",
                false,
            ));
        }
        match &request.materialize {
            Materialize::Closure(path) => validate_store_path("closure", path).map_err(|_| {
                ResolutionError::new(
                    ErrorCategory::InvalidRequest,
                    "closure is not a canonical Nix store path",
                    false,
                )
            })?,
            Materialize::Flake(installable) => validate_installable(installable)?,
            Materialize::Release(reference) => {
                if !self
                    .inner
                    .config
                    .policy
                    .issuers
                    .contains_key(&reference.issuer)
                {
                    return Err(ResolutionError::new(
                        ErrorCategory::PolicyDenied,
                        "release issuer is not authorized on this node",
                        false,
                    ));
                }
            }
        }
        if matches!(&request.materialize, Materialize::Flake(_))
            && self.inner.config.policy.cache_only
        {
            return Err(ResolutionError::new(
                ErrorCategory::EvaluationDisabled,
                "flake source evaluation is disabled on this node (cache_only)",
                false,
            ));
        }
        if let Materialize::Flake(installable) = &request.materialize {
            let uri = installable
                .rsplit_once('#')
                .map(|(uri, _)| uri)
                .unwrap_or("");
            // SPEC §3's scheme rules, re-applied at the daemon boundary: the
            // annotation planner already rejects node-local schemes and
            // registry names client-side, but a direct socket peer builds its
            // own requests. The lowercase `path:` rewrite the runtime produces
            // for staged in-image sources is the one node-local form admitted
            // here — and it still has to pass the prefix allow-list below.
            if !uri.starts_with("path:") && !uri_carries_remote_scheme(uri) {
                return Err(ResolutionError::new(
                    ErrorCategory::InvalidRequest,
                    "external source URIs must carry an explicit remote scheme; \
                     node-local schemes and registry names are rejected",
                    false,
                ));
            }
            if !self
                .inner
                .config
                .policy
                .eval_allowed_uri_prefixes
                .iter()
                .any(|prefix| uri.starts_with(prefix))
            {
                return Err(ResolutionError::new(
                    ErrorCategory::PolicyDenied,
                    "development source URI is not authorized by node policy",
                    false,
                ));
            }
        }
        if !request.bundle_path.is_absolute() {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "bundle path must be absolute and canonical",
                false,
            ));
        }
        let canonical = std::fs::canonicalize(&request.bundle_path).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "bundle path does not name an accessible directory",
                false,
            )
        })?;
        if canonical != request.bundle_path || !canonical.is_dir() {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "bundle path must be absolute and canonical",
                false,
            ));
        }
        let requested = Duration::from_millis(request.timeout_ms);
        let effective = requested.min(self.inner.config.max_timeout);
        let deadline = Instant::now().checked_add(effective).ok_or_else(|| {
            ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "timeout is out of range",
                false,
            )
        })?;
        Ok((canonical, deadline))
    }

    fn materialize(
        &self,
        materialize: &Materialize,
        selected: Option<&SelectedRelease>,
        bundle: &Path,
        deadline: Instant,
        clock: &StageClock,
    ) -> Result<ResolvedRelease, ResolutionError> {
        if matches!(materialize, Materialize::Release(_)) {
            let selected = selected.ok_or_else(|| {
                ResolutionError::new(
                    ErrorCategory::Internal,
                    "release selection was not available",
                    false,
                )
            })?;
            return self.materialize_release(selected, bundle, deadline, clock);
        }
        if let Materialize::Flake(installable) = materialize {
            return self.materialize_development(installable, bundle, deadline, clock);
        }
        let root = bundle.join(GC_ROOT_NAME);
        prepare_gc_root(&root)?;
        let _permit = self.acquire_permit(deadline, "while waiting for a Nix operation")?;
        let mut command = match materialize {
            Materialize::Closure(store_path) => {
                let mut command = Command::new(&self.inner.config.nix_store);
                command.args([OsStr::new("--realise"), store_path.as_ref()]);
                command.args([OsStr::new("--add-root"), root.as_os_str()]);
                // Same reasoning as `register_root`: the caller asserts this
                // path is present, and a fallback fetch must still be signed.
                command.args([
                    OsStr::new("--option"),
                    OsStr::new("require-sigs"),
                    OsStr::new("true"),
                ]);
                command
            }
            Materialize::Flake(_) => unreachable!(),
            Materialize::Release(_) => unreachable!(),
        };
        let budget = remaining(deadline, "during materialization")?;
        let stdout = StageClock::time(&clock.evaluation_us, || run_command(&mut command, budget))
            .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => {
                ResolutionError::timeout_with("during materialization", &error.to_string())
            }
            _ => ResolutionError::new(
                ErrorCategory::Materialization,
                format!("Nix could not materialize the requested rootfs: {error}"),
                true,
            ),
        })?;
        let store_path = match materialize {
            Materialize::Closure(store_path) => store_path,
            Materialize::Flake(_) | Materialize::Release(_) => unreachable!(),
        };
        validate_realise_output(&stdout, store_path, &root).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::Materialization,
                "Nix returned an invalid or ambiguous rootfs path",
                false,
            )
        })?;
        validate_registered_root(&root, store_path, ErrorCategory::Materialization)?;
        Ok(ResolvedRelease {
            identity: match materialize {
                Materialize::Closure(_) => "legacy-closure".to_string(),
                Materialize::Flake(_) | Materialize::Release(_) => unreachable!(),
            },
            rootfs: store_path.clone(),
            process: None,
            mounts: Vec::new(),
        })
    }

    fn materialize_development(
        &self,
        installable: &str,
        bundle: &Path,
        deadline: Instant,
        clock: &StageClock,
    ) -> Result<ResolvedRelease, ResolutionError> {
        let worker = self.inner.config.development_worker.as_ref();
        if worker.is_none() && !self.inner.config.evaluate_as_caller {
            return Err(ResolutionError::new(
                ErrorCategory::EvaluationDisabled,
                "development source evaluation has no unprivileged worker configured",
                false,
            ));
        }
        let root = bundle.join(GC_ROOT_NAME);
        prepare_gc_root(&root)?;
        let staged = StageClock::time(&clock.staging_us, || {
            stage_development_installable(
                installable,
                worker.map(|worker| worker.user.as_str()),
                deadline,
            )
        })?;
        let evaluation_installable = staged
            .as_ref()
            .map(|(installable, _guard)| installable.as_str())
            .unwrap_or(installable);
        let policy = &self.inner.config.policy;
        let staged_directory = staged.as_ref().map(|(_, guard)| guard.directory.clone());
        if let Some(directory) = &staged_directory {
            refuse_local_locked_inputs(directory)?;
        }
        // An in-image flake evaluates against the lock it ships. Nix fetches an
        // unlocked input before `--no-update-lock-file` refuses it, which is
        // why this gate relies on the confinement below rather than replacing
        // it: the refused fetch can reach nothing of the node's.
        let locked = staged_directory.is_some() && !policy.allow_unlocked_inputs;
        let confinement_root = if policy.unconfined_evaluation {
            None
        } else {
            Some(create_confinement_root()?)
        };
        let timeout = remaining(deadline, "before development source evaluation")?;
        let timeout_seconds = timeout.as_secs().max(1).to_string();
        // Guard slot, not dead code: the worker's cache directory must survive
        // until `run_command` returns, and the command is built inside the
        // block below.
        let mut _worker_cache = None;
        let stdout = {
            let _permit =
                self.acquire_permit(deadline, "while waiting for a development Nix operation")?;
            let mut command = match worker {
                Some(worker) => {
                    let cache = create_worker_cache_directory(&worker.user)?;
                    let mut command = Command::new(&worker.program);
                    command
                        .arg("--user")
                        .arg(&worker.user)
                        .arg("--nix")
                        .arg(&self.inner.config.nix)
                        .arg("--cpu-seconds")
                        .arg(timeout_seconds)
                        .arg("--cache-home")
                        .arg(&cache.directory)
                        .arg("--installable")
                        .arg(evaluation_installable)
                        .env_clear();
                    if let Some(root) = &confinement_root {
                        command.arg("--confine-root").arg(&root.directory);
                    }
                    if locked {
                        command.arg("--locked");
                    }
                    _worker_cache = Some(cache);
                    command
                }
                None => {
                    let nix = resolve_program(&self.inner.config.nix)?;
                    let mut command = Command::new(&nix);
                    command.args([
                        "--extra-experimental-features",
                        "nix-command flakes",
                        "build",
                        "--no-link",
                        "--print-out-paths",
                    ]);
                    if locked {
                        command.arg("--no-update-lock-file");
                    }
                    command.arg(evaluation_installable);
                    if let Some(root) = &confinement_root {
                        let certificates: Vec<PathBuf> = ["NIX_SSL_CERT_FILE", "SSL_CERT_FILE"]
                            .into_iter()
                            .filter_map(std::env::var_os)
                            .map(PathBuf::from)
                            .collect();
                        let confinement = Confinement::for_evaluation(
                            &root.directory,
                            &nix,
                            staged_directory.as_slice(),
                            &certificates,
                        )
                        .map_err(|error| {
                            ResolutionError::new(
                                ErrorCategory::Internal,
                                format!("could not plan evaluation confinement: {error}"),
                                false,
                            )
                        })?;
                        // SAFETY: `enter` performs raw syscalls on data prepared
                        // above and allocates nothing.
                        unsafe {
                            command.pre_exec(move || confinement.enter());
                        }
                    }
                    command
                }
            };
            let budget = remaining(deadline, "during development source evaluation")?;
            StageClock::time(&clock.evaluation_us, || run_command(&mut command, budget)).map_err(
                |error| match error.kind() {
                    io::ErrorKind::TimedOut => ResolutionError::timeout_with(
                        "during development source evaluation",
                        &error.to_string(),
                    ),
                    _ => ResolutionError::new(
                        ErrorCategory::Materialization,
                        format!(
                            "the development resolver could not evaluate the source: {error}{}",
                            evaluation_hint(&error.to_string(), confinement_root.is_some())
                        ),
                        true,
                    ),
                },
            )?
        };
        let store_path = parse_materialized_path(&stdout).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::Materialization,
                "development resolver returned an invalid or ambiguous rootfs path",
                false,
            )
        })?;
        StageClock::time(&clock.root_registration_us, || {
            self.register_root(&store_path, &root, deadline)
        })?;
        Ok(ResolvedRelease {
            identity: development_identity(installable),
            rootfs: store_path,
            process: None,
            mounts: Vec::new(),
        })
    }

    fn select_release(
        &self,
        reference: &ReleaseReference,
        container_name: Option<&str>,
        purpose: ResolvePurpose,
        deadline: Instant,
        clock: &StageClock,
    ) -> Result<SelectedRelease, ResolutionError> {
        let issuer = self
            .inner
            .config
            .policy
            .issuers
            .get(&reference.issuer)
            .ok_or_else(|| {
                ResolutionError::new(
                    ErrorCategory::PolicyDenied,
                    "release issuer is not authorized on this node",
                    false,
                )
            })?;
        if !issuer.permits_release(&reference.name) {
            return Err(ResolutionError::new(
                ErrorCategory::PolicyDenied,
                "release name is not allowed by node policy",
                false,
            ));
        }
        // A network round trip on an HTTPS issuer, and the reason
        // `policy_verification_us` used to grow with catalog latency: it spans
        // this call, so a slow catalog was indistinguishable from a slow policy
        // check.
        let bytes = StageClock::time(&clock.manifest_fetch_us, || {
            release::fetch_manifest(
                &issuer.source,
                &reference.sha256,
                remaining(deadline, "while fetching the release manifest")?,
            )
        })?;
        let manifest = release::parse_full_manifest(&bytes, reference)?;
        if purpose == ResolvePurpose::Runtime && !manifest.selectors.permits(container_name) {
            return Err(ResolutionError::new(
                ErrorCategory::PolicyDenied,
                "release manifest does not permit this container",
                false,
            ));
        }
        let target = manifest
            .targets
            .get(&self.inner.config.policy.system)
            .cloned()
            .ok_or_else(|| {
                let available = manifest
                    .targets
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                ResolutionError::new(
                    ErrorCategory::ArchitectureMismatch,
                    format!(
                        "release has no target for {}; available targets: {available}",
                        self.inner.config.policy.system
                    ),
                    false,
                )
            })?;
        let cache = issuer.caches.get(&target.cache).cloned().ok_or_else(|| {
            ResolutionError::new(
                ErrorCategory::PolicyDenied,
                "release cache identity is not authorized for this issuer",
                false,
            )
        })?;
        let resolution = ResolvedRelease {
            identity: reference.identity(),
            rootfs: target.rootfs,
            process: target.process,
            mounts: target.mounts,
        };
        let response = ResolveResponse::Success {
            version: PROTOCOL_VERSION,
            resolution: Box::new(resolution.clone()),
            timings: ResolutionTimings::default(),
            closure: None,
        };
        if serde_json::to_vec(&response)
            .map_err(|_| {
                ResolutionError::new(
                    ErrorCategory::Internal,
                    "release response could not be encoded",
                    false,
                )
            })?
            .len()
            > MAX_FRAME_BYTES
        {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "selected release metadata exceeds the resolver protocol frame limit",
                false,
            ));
        }
        Ok(SelectedRelease { resolution, cache })
    }

    fn materialize_release(
        &self,
        selected: &SelectedRelease,
        bundle: &Path,
        deadline: Instant,
        clock: &StageClock,
    ) -> Result<ResolvedRelease, ResolutionError> {
        for (index, store_path) in selected.resolution.store_paths().enumerate() {
            let root = gc_root_path(bundle, index)?;
            prepare_gc_root(&root)?;
            // A release with mounts realises more than one path, which is why
            // the clock accumulates rather than assigns.
            StageClock::time(&clock.evaluation_us, || {
                self.materialize_store_path(store_path, &root, &selected.cache, deadline)
            })?;
        }
        Ok(selected.resolution.clone())
    }

    fn query_closure(
        &self,
        selected: &SelectedRelease,
        deadline: Instant,
    ) -> Result<ClosureReport, ResolutionError> {
        #[derive(Deserialize)]
        struct PathInfo {
            #[serde(rename = "narSize")]
            nar_size: u64,
            #[serde(rename = "downloadSize")]
            download_size: Option<u64>,
        }

        let _permit = self.acquire_permit(deadline, "while waiting to inspect the cache")?;
        let mut path_info = Command::new(&self.inner.config.nix);
        path_info.args([
            OsStr::new("path-info"),
            OsStr::new("--extra-experimental-features"),
            OsStr::new("nix-command"),
            OsStr::new("--json"),
            OsStr::new("--json-format"),
            OsStr::new("1"),
            OsStr::new("--recursive"),
            OsStr::new("--size"),
        ]);
        // The registry-free smoke uses the daemon's already-valid local store
        // as its intentionally trivial cache. Other file:// values are real
        // binary caches and remain explicit remote stores.
        if selected.cache.substituter != "file:///nix/store" {
            path_info.args([OsStr::new("--store"), selected.cache.substituter.as_ref()]);
        }
        // No `require-sigs` here, deliberately: `path-info` reads a cache and
        // never adds to the store, and the setting only gates what enters one.
        // The keys below are inert for the same reason — `path-info` never
        // reaches signature verification, and describes an unsigned path just
        // as readily as a signed one. `--store` above is what decides which
        // cache is answering. They are passed anyway so that this call and the
        // realise it prices out name the same cache the same way; nothing here
        // is an authorization check, and `inspect` treats it as none.
        if !selected.cache.public_keys.is_empty() {
            path_info.args([
                OsStr::new("--option"),
                OsStr::new("trusted-public-keys"),
                OsStr::new(&selected.cache.public_keys.join(" ")),
            ]);
        }
        path_info.args(selected.resolution.store_paths());
        let stdout = run_command(
            &mut path_info,
            remaining(deadline, "while querying the authorized cache")?,
        )
        .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => ResolutionError::timeout_with(
                "while querying the authorized cache",
                &error.to_string(),
            ),
            _ => ResolutionError::new(
                ErrorCategory::CacheQuery,
                format!("the authorized cache could not describe the release closure: {error}"),
                true,
            ),
        })?;
        let infos: BTreeMap<String, PathInfo> = serde_json::from_str(&stdout).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::CacheQuery,
                "the authorized cache returned an invalid closure description",
                false,
            )
        })?;
        if infos.is_empty()
            || selected
                .resolution
                .store_paths()
                .any(|path| !infos.contains_key(path))
        {
            return Err(ResolutionError::new(
                ErrorCategory::CacheQuery,
                "the authorized cache omitted a release store path",
                false,
            ));
        }
        for path in infos.keys() {
            validate_store_path("cache closure path", path).map_err(|_| {
                ResolutionError::new(
                    ErrorCategory::CacheQuery,
                    "the authorized cache returned an invalid closure path",
                    false,
                )
            })?;
        }

        let mut validity = Command::new(&self.inner.config.nix_store);
        validity.args([
            OsStr::new("--check-validity"),
            OsStr::new("--print-invalid"),
        ]);
        validity.args(infos.keys());
        let invalid_stdout = run_command(
            &mut validity,
            remaining(deadline, "while checking the node store")?,
        )
        .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => {
                ResolutionError::timeout_with("while checking the node store", &error.to_string())
            }
            _ => ResolutionError::new(
                ErrorCategory::CacheQuery,
                format!("the node store could not report valid closure paths: {error}"),
                true,
            ),
        })?;
        let mut invalid = HashSet::new();
        for path in invalid_stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            if !infos.contains_key(path) || !invalid.insert(path.to_string()) {
                return Err(ResolutionError::new(
                    ErrorCategory::CacheQuery,
                    "the node store returned an invalid path status",
                    false,
                ));
            }
        }

        let mut total_nar_bytes = 0_u64;
        let mut missing_download_bytes = 0_u64;
        let mut closure_paths = Vec::with_capacity(infos.len());
        for (path, info) in infos {
            let present = !invalid.contains(&path);
            let download_bytes = info.download_size.unwrap_or(info.nar_size);
            total_nar_bytes = total_nar_bytes.checked_add(info.nar_size).ok_or_else(|| {
                ResolutionError::new(
                    ErrorCategory::CacheQuery,
                    "closure size exceeds the supported range",
                    false,
                )
            })?;
            if !present {
                missing_download_bytes = missing_download_bytes
                    .checked_add(download_bytes)
                    .ok_or_else(|| {
                        ResolutionError::new(
                            ErrorCategory::CacheQuery,
                            "download estimate exceeds the supported range",
                            false,
                        )
                    })?;
            }
            closure_paths.push(ClosurePathReport {
                path,
                nar_bytes: info.nar_size,
                download_bytes,
                present,
            });
        }
        let report = ClosureReport {
            schema: "imageless.closure-report.v1".to_string(),
            release: selected.resolution.identity.clone(),
            closure_paths,
            total_nar_bytes,
            missing_download_bytes,
        };
        let response = ResolveResponse::Success {
            version: PROTOCOL_VERSION,
            resolution: Box::new(selected.resolution.clone()),
            timings: ResolutionTimings::default(),
            closure: Some(Box::new(report.clone())),
        };
        if serde_json::to_vec(&response).map_or(true, |bytes| bytes.len() > MAX_FRAME_BYTES) {
            return Err(ResolutionError::new(
                ErrorCategory::CacheQuery,
                "closure report exceeds the resolver protocol frame limit",
                false,
            ));
        }
        Ok(report)
    }

    fn materialize_store_path(
        &self,
        store_path: &str,
        root: &Path,
        cache: &CachePolicy,
        deadline: Instant,
    ) -> Result<(), ResolutionError> {
        let _permit = self.acquire_permit(deadline, "while waiting for a Nix operation")?;
        let mut command = Command::new(&self.inner.config.nix_store);
        command.args([OsStr::new("--realise"), store_path.as_ref()]);
        command.args([OsStr::new("--add-root"), root.as_os_str()]);
        command.args([
            OsStr::new("--option"),
            OsStr::new("substituters"),
            cache.substituter.as_ref(),
        ]);
        // SPEC §6 has node policy pin both the substituters a release may come
        // from and the keys for them, but the second half only means anything
        // if Nix actually verifies the signature. `require-sigs` is Nix's
        // default, so on a stock node this changes nothing; it is here for the
        // node whose operator turned it off for reasons that had nothing to do
        // with releases.
        //
        // CONSTRAINT: this binds only because the resolver runs as root, for
        // which `store = auto` opens a LocalStore inside this process. Nix
        // fixes require-sigs on the store object when it is opened, so the same
        // flag sent to a nix-daemon by a client is parsed and then discarded.
        command.args([
            OsStr::new("--option"),
            OsStr::new("require-sigs"),
            OsStr::new("true"),
        ]);
        // An explicit list REPLACES the node's trusted-public-keys rather than
        // extending it, so a cache that names keys is verified against exactly
        // those. Naming none stays legal and means the opposite: verify against
        // whatever the node already trusts.
        if !cache.public_keys.is_empty() {
            command.args([
                OsStr::new("--option"),
                OsStr::new("trusted-public-keys"),
                OsStr::new(&cache.public_keys.join(" ")),
            ]);
        }
        let stdout = run_command(
            &mut command,
            remaining(deadline, "during release materialization")?,
        )
        .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => {
                ResolutionError::timeout_with("during release materialization", &error.to_string())
            }
            _ => ResolutionError::new(
                ErrorCategory::Materialization,
                format!("Nix could not materialize a release store path: {error}"),
                true,
            ),
        })?;
        validate_realise_output(&stdout, store_path, root).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::Materialization,
                "Nix returned an invalid or ambiguous release store path",
                false,
            )
        })?;
        validate_registered_root(root, store_path, ErrorCategory::Materialization)
    }

    fn register_resolution(
        &self,
        resolution: &ResolvedRelease,
        bundle: &Path,
        deadline: Instant,
        clock: &StageClock,
    ) -> Result<ResolvedRelease, ResolutionError> {
        for (index, store_path) in resolution.store_paths().enumerate() {
            let root = gc_root_path(bundle, index)?;
            prepare_gc_root(&root)?;
            // A follower reaching here paid no staging and no evaluation — the
            // leader did — so this is the whole of its materialization cost,
            // and the gap between it and `substitution_us` is the wait.
            StageClock::time(&clock.root_registration_us, || {
                self.register_root(store_path, &root, deadline)
            })?;
        }
        Ok(resolution.clone())
    }

    fn register_root(
        &self,
        store_path: &str,
        root: &Path,
        deadline: Instant,
    ) -> Result<String, ResolutionError> {
        let _permit = self.acquire_permit(deadline, "while waiting to register the GC root")?;
        let mut command = Command::new(&self.inner.config.nix_store);
        command.args([OsStr::new("--realise"), store_path.as_ref()]);
        command.args([OsStr::new("--add-root"), root.as_os_str()]);
        // Normally inert: the path is already valid, so nothing is substituted
        // and no signature is looked at. It matters in the case that is not
        // normal — a collected path, a follower racing the leader — because
        // this call passes no `substituters` and would fall back to the NODE's,
        // which are not the release's authorized cache. Refusing an unsigned
        // fetch is the cheap half of closing that; scoping the substituter is
        // the other half, and is a larger change than this.
        command.args([
            OsStr::new("--option"),
            OsStr::new("require-sigs"),
            OsStr::new("true"),
        ]);
        let stdout = run_command(
            &mut command,
            remaining(deadline, "during GC-root registration")?,
        )
        .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => {
                ResolutionError::timeout_with("during GC-root registration", &error.to_string())
            }
            _ => ResolutionError::new(
                ErrorCategory::RootRegistration,
                format!("Nix could not register the bundle GC root: {error}"),
                true,
            ),
        })?;
        validate_realise_output(&stdout, store_path, root).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::RootRegistration,
                "Nix returned an invalid path while registering the bundle GC root",
                false,
            )
        })?;
        validate_registered_root(root, store_path, ErrorCategory::RootRegistration)?;
        Ok(store_path.to_string())
    }

    fn acquire_permit(
        &self,
        deadline: Instant,
        stage: &str,
    ) -> Result<Permit<'_>, ResolutionError> {
        let mut active = self.inner.permits.lock().unwrap();
        while *active >= self.inner.config.max_realizations {
            let duration = remaining(deadline, stage)?;
            let (next, timed) = self
                .inner
                .permit_ready
                .wait_timeout(active, duration)
                .unwrap();
            active = next;
            if timed.timed_out() && *active >= self.inner.config.max_realizations {
                return Err(ResolutionError::timeout(stage));
            }
        }
        *active += 1;
        Ok(Permit {
            resolver: &self.inner,
        })
    }

    /// Join the in-progress flight for `key`, or publish a new one and lead it.
    fn join_flight(&self, key: &str) -> (Arc<Flight>, bool) {
        let mut flights = self.inner.flights.lock().unwrap();
        if let Some(flight) = flights.get(key) {
            flight.state.lock().unwrap().callers += 1;
            return (Arc::clone(flight), false);
        }
        let flight = Arc::new(Flight {
            state: Mutex::new(FlightState {
                callers: 1,
                result: None,
            }),
            ready: Condvar::new(),
        });
        flights.insert(key.to_string(), Arc::clone(&flight));
        (flight, true)
    }

    /// Unpublish `flight` so the next caller for `key` starts a fresh one;
    /// callers already waiting on it still receive its result.
    fn forget_flight(&self, key: &str, flight: &Arc<Flight>) {
        let mut flights = self.inner.flights.lock().unwrap();
        if flights
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, flight))
        {
            flights.remove(key);
        }
    }

    fn release_flight(&self, key: &str, flight: &Arc<Flight>) {
        let remove = {
            let mut state = flight.state.lock().unwrap();
            state.callers -= 1;
            state.callers == 0
        };
        if remove {
            self.forget_flight(key, flight);
        }
    }
}

struct Permit<'a> {
    resolver: &'a ResolverInner,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut active = self.resolver.permits.lock().unwrap();
        *active -= 1;
        self.resolver.permit_ready.notify_one();
    }
}

fn wait_for_flight(flight: &Flight, deadline: Instant) -> Result<ResolvedRelease, ResolutionError> {
    let mut state = flight.state.lock().unwrap();
    loop {
        if let Some(result) = &state.result {
            return result.clone();
        }
        let duration = remaining(deadline, "while waiting for an identical realization")?;
        let (next, timed) = flight.ready.wait_timeout(state, duration).unwrap();
        state = next;
        if timed.timed_out() && state.result.is_none() {
            return Err(ResolutionError::timeout(
                "while waiting for an identical realization",
            ));
        }
    }
}

/// Resolution identity for telemetry and journals: the runtime's own `path:`
/// rewrite of a staged in-image source reports `development-source`; anything
/// else was an external flake reference (SPEC §3), and operators reading
/// timings should be able to tell the two apart.
fn development_identity(installable: &str) -> String {
    if installable.starts_with("path:") {
        "development-source".to_string()
    } else {
        "external-source".to_string()
    }
}

/// SPEC §3: an external reference must name a remote transport explicitly.
/// `file:`-family schemes read node files, and registry forms (`flake:`, bare
/// words, or a `path`/`PATH` spelling that is not the runtime's own lowercase
/// staging rewrite) would resolve through node-side state an annotation has no
/// business reaching.
fn uri_carries_remote_scheme(uri: &str) -> bool {
    let scheme = match uri.split_once(':') {
        Some((scheme, _)) if !scheme.is_empty() && !scheme.contains('/') => {
            scheme.to_ascii_lowercase()
        }
        _ => return false,
    };
    scheme != "flake" && scheme != "path" && scheme.split('+').next_back() != Some("file")
}

fn validate_installable(installable: &str) -> Result<(), ResolutionError> {
    if installable.is_empty()
        || installable.len() > MAX_ANNOTATION_VALUE_BYTES + 512
        || installable.chars().any(char::is_whitespace)
        || installable.chars().any(char::is_control)
    {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "flake installable is not normalized",
            false,
        ));
    }
    let Some((flake, output)) = installable.rsplit_once('#') else {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "flake installable must include one output",
            false,
        ));
    };
    if flake.is_empty() || flake.contains('#') || validate_output("flake output", output).is_err() {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "flake installable is not normalized",
            false,
        ));
    }
    Ok(())
}

/// An empty directory the evaluator's confined root is mounted over — inside
/// the evaluator's own mount namespace only, so from here it stays empty and is
/// removed when the evaluation is over.
struct ConfinementRoot {
    directory: PathBuf,
}

impl Drop for ConfinementRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.directory);
    }
}

fn create_confinement_root() -> Result<ConfinementRoot, ResolutionError> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    for _ in 0..128 {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!(".imageless-confine-{}-{nonce}", std::process::id()));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                let guard = ConfinementRoot { directory: path };
                std::fs::set_permissions(&guard.directory, std::fs::Permissions::from_mode(0o700))
                    .map_err(|error| {
                        ResolutionError::new(
                            ErrorCategory::Internal,
                            format!("could not prepare evaluation confinement: {error}"),
                            true,
                        )
                    })?;
                return Ok(guard);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(ResolutionError::new(
                    ErrorCategory::Internal,
                    format!("could not prepare evaluation confinement: {error}"),
                    true,
                ))
            }
        }
    }
    Err(ResolutionError::new(
        ErrorCategory::Internal,
        "evaluation confinement namespace exhausted",
        true,
    ))
}

/// The confined root holds only absolute paths, so a bare program name is
/// resolved against PATH here, where the host's PATH is still visible.
fn resolve_program(program: &Path) -> Result<PathBuf, ResolutionError> {
    if program.is_absolute() {
        return Ok(program.to_path_buf());
    }
    if program.components().count() == 1 {
        if let Some(found) = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join(program))
                .find(|candidate| candidate.is_file())
        }) {
            return Ok(found);
        }
    }
    Err(ResolutionError::new(
        ErrorCategory::Internal,
        format!("could not locate the Nix client `{}`", program.display()),
        false,
    ))
}

/// Point a failed evaluation at the node setting that explains it.
fn evaluation_hint(error: &str, confined: bool) -> &'static str {
    // Under confinement an unlocked local input fails while Nix is still
    // locking it, before `--no-update-lock-file` gets to refuse the change.
    if error.contains("requires lock file changes")
        || error.contains("while updating the lock file")
    {
        "; an in-image flake with inputs must ship a complete flake.lock \
         (or the node policy must set allow_unlocked_inputs)"
    } else if confined && error.contains("Operation not permitted") {
        "; evaluation runs in a private mount namespace, and a node that cannot \
         create one must set unconfined_evaluation in its policy"
    } else {
        ""
    }
}

/// Refuse a staged flake.lock that pins a node-local input: an absolute or
/// `..`-escaping `path` input, or any `file:` URL. Such an input names the
/// node's filesystem, which an image never controls (SPEC §2.3). Confinement
/// already denies the read; this turns it into a diagnostic that names the
/// input, before Nix runs.
fn refuse_local_locked_inputs(directory: &Path) -> Result<(), ResolutionError> {
    let bytes = match std::fs::read(directory.join("flake.lock")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(staging_error(error)),
    };
    let invalid =
        |reason: String| ResolutionError::new(ErrorCategory::InvalidRequest, reason, false);
    let lock: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("embedded flake.lock is not valid JSON".to_string()))?;
    let nodes = lock
        .get("nodes")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| invalid("embedded flake.lock has no nodes".to_string()))?;
    for (name, node) in nodes {
        let Some(locked) = node.get("locked") else {
            continue;
        };
        if let Some(reason) = local_lock_reason(locked) {
            let name: String = name
                .chars()
                .filter(|character| !character.is_control())
                .take(64)
                .collect();
            return Err(invalid(format!(
                "embedded flake.lock input `{name}` {reason}; an in-image flake may not lock \
                 node-local inputs"
            )));
        }
    }
    Ok(())
}

fn local_lock_reason(locked: &serde_json::Value) -> Option<&'static str> {
    let field = |key: &str| locked.get(key).and_then(serde_json::Value::as_str);
    if field("type") == Some("path") {
        let path = field("path").unwrap_or("");
        if path.starts_with('/') {
            return Some("is an absolute path");
        }
        if path.split('/').any(|part| part == "..") {
            return Some("escapes its flake with `..`");
        }
    }
    if let Some(url) = field("url") {
        if url.starts_with('/')
            || url
                .get(..5)
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file:"))
        {
            return Some("is a local file URL");
        }
    }
    None
}

struct StagedDevelopmentSource {
    directory: PathBuf,
}

impl Drop for StagedDevelopmentSource {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Worker-owned scratch backing the development worker's `XDG_CACHE_HOME` for
/// one evaluation. Nix refuses to evaluate a flake without a writable fetcher
/// cache, and the worker's HOME is deliberately the unwritable `/var/empty` —
/// so the resolver hands it exactly one cache directory and reclaims it when
/// the evaluation is over.
struct WorkerCacheDirectory {
    directory: PathBuf,
}

impl Drop for WorkerCacheDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn create_worker_cache_directory(
    worker_user: &str,
) -> Result<WorkerCacheDirectory, ResolutionError> {
    let cache_error = |_| {
        ResolutionError::new(
            ErrorCategory::Internal,
            "the development worker cache could not be prepared",
            true,
        )
    };
    let (uid, gid) = user_ids(worker_user).map_err(|_| {
        ResolutionError::new(
            ErrorCategory::Internal,
            "development worker user could not be resolved",
            false,
        )
    })?;
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    for _ in 0..128 {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            ".imageless-dev-cache-{}-{nonce}",
            std::process::id()
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                let guard = WorkerCacheDirectory { directory: path };
                std::fs::set_permissions(&guard.directory, std::fs::Permissions::from_mode(0o700))
                    .map_err(cache_error)?;
                let raw = CString::new(guard.directory.as_os_str().as_bytes())
                    .map_err(|_| cache_error(io::Error::other("path contains NUL")))?;
                if unsafe { libc::lchown(raw.as_ptr(), uid, gid) } != 0 {
                    return Err(cache_error(io::Error::last_os_error()));
                }
                return Ok(guard);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(cache_error(error)),
        }
    }
    Err(cache_error(io::Error::other(
        "worker cache namespace exhausted",
    )))
}

#[derive(Default)]
struct StagingBudget {
    bytes: u64,
    entries: usize,
}

fn stage_development_installable(
    installable: &str,
    worker_user: Option<&str>,
    deadline: Instant,
) -> Result<Option<(String, StagedDevelopmentSource)>, ResolutionError> {
    let Some((source, output)) = installable
        .strip_prefix("path:")
        .and_then(|value| value.rsplit_once('#'))
    else {
        return Ok(None);
    };
    let worker_gid = match worker_user {
        Some(worker_user) => {
            user_ids(worker_user)
                .map_err(|_| {
                    ResolutionError::new(
                        ErrorCategory::Internal,
                        "development worker user could not be resolved",
                        false,
                    )
                })?
                .1
        }
        None => unsafe { libc::getegid() },
    };
    let directory = create_staging_directory(worker_gid)?;
    let staged = StagedDevelopmentSource { directory };
    let mut budget = StagingBudget::default();
    copy_staged_tree(
        Path::new(source),
        &staged.directory,
        worker_gid,
        deadline,
        &mut budget,
    )?;
    Ok(Some((
        format!("path:{}#{output}", staged.directory.display()),
        staged,
    )))
}

fn create_staging_directory(worker_gid: libc::gid_t) -> Result<PathBuf, ResolutionError> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    for _ in 0..128 {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!(".imageless-source-{}-{nonce}", std::process::id()));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                set_staged_access(&path, worker_gid, 0o750)?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => break,
        }
    }
    Err(ResolutionError::new(
        ErrorCategory::Internal,
        "could not allocate embedded source staging directory",
        true,
    ))
}

fn copy_staged_tree(
    source: &Path,
    destination: &Path,
    worker_gid: libc::gid_t,
    deadline: Instant,
    budget: &mut StagingBudget,
) -> Result<(), ResolutionError> {
    let _ = remaining(deadline, "during embedded source staging")?;
    let metadata = std::fs::symlink_metadata(source).map_err(staging_error)?;
    if metadata.file_type().is_symlink() {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "embedded source contains a symlink",
            false,
        ));
    }
    budget.entries = budget.entries.saturating_add(1);
    if budget.entries > MAX_STAGED_SOURCE_ENTRIES {
        return Err(staging_limit("file-count"));
    }
    if metadata.is_dir() {
        if destination.exists() {
            if !destination.is_dir() {
                return Err(staging_error(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "staging destination collision",
                )));
            }
        } else {
            std::fs::create_dir(destination).map_err(staging_error)?;
        }
        for entry in std::fs::read_dir(source).map_err(staging_error)? {
            let entry = entry.map_err(staging_error)?;
            copy_staged_tree(
                &entry.path(),
                &destination.join(entry.file_name()),
                worker_gid,
                deadline,
                budget,
            )?;
        }
        set_staged_access(destination, worker_gid, 0o750)?;
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "embedded source contains a non-regular file",
            false,
        ));
    }
    budget.bytes = budget.bytes.saturating_add(metadata.len());
    if budget.bytes > MAX_STAGED_SOURCE_BYTES {
        return Err(staging_limit("byte"));
    }
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)
        .map_err(staging_error)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)
        .map_err(staging_error)?;
    let copied = io::copy(
        &mut std::io::Read::by_ref(&mut input).take(metadata.len().saturating_add(1)),
        &mut output,
    )
    .map_err(staging_error)?;
    if copied != metadata.len() {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "embedded source changed while it was being staged",
            true,
        ));
    }
    output.sync_all().map_err(staging_error)?;
    let mode = if metadata.mode() & 0o111 == 0 {
        0o640
    } else {
        0o750
    };
    set_staged_access(destination, worker_gid, mode)
}

fn set_staged_access(
    path: &Path,
    worker_gid: libc::gid_t,
    mode: u32,
) -> Result<(), ResolutionError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(staging_error)?;
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "embedded source path contains a NUL byte",
            false,
        )
    })?;
    if unsafe { libc::chown(path.as_ptr(), effective_uid(), worker_gid) } == -1 {
        return Err(staging_error(io::Error::last_os_error()));
    }
    // Nix folds mtimes into the fingerprint that keys a `path:` flake's
    // evaluation cache, and the staged path itself is *not* in it. Measured on
    // the nix the gate pins: two byte-identical trees at different paths share
    // one cache entry when their mtimes agree, and get one entry each when they
    // differ. Staging stamps "now", so every create of an unchanged seed missed
    // that cache — a restart, a CrashLoopBackOff retry, a second replica, and
    // each container of a multi-container pod all paid a cold evaluation.
    //
    // Any deterministic value would do; the epoch is the one the seed layer
    // already uses (kubectl-imageless packs tars with a zero mtime), so this
    // restores determinism the tar had rather than inventing one.
    //
    // Directories are stamped by the caller *after* their children, so a child
    // write cannot bump the parent back to now. Both matter: file and directory
    // mtimes are each in the fingerprint.
    let epoch = [libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    }; 2];
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            epoch.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == -1
    {
        return Err(staging_error(io::Error::last_os_error()));
    }
    Ok(())
}

fn user_ids(name: &str) -> io::Result<(libc::uid_t, libc::gid_t)> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid user name"))?;
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0_u8; 16 * 1024];
    let status = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return Err(if status == 0 {
            io::Error::new(io::ErrorKind::NotFound, "worker user does not exist")
        } else {
            io::Error::from_raw_os_error(status)
        });
    }
    let entry = unsafe { entry.assume_init() };
    Ok((entry.pw_uid, entry.pw_gid))
}

fn staging_error(_error: io::Error) -> ResolutionError {
    ResolutionError::new(
        ErrorCategory::InvalidRequest,
        "embedded source could not be staged safely",
        false,
    )
}

fn staging_limit(kind: &str) -> ResolutionError {
    ResolutionError::new(
        ErrorCategory::InvalidRequest,
        format!("embedded source exceeds the {kind} limit"),
        false,
    )
}

#[cfg_attr(not(feature = "daemon"), allow(dead_code))]
pub fn handle_connection(
    mut stream: UnixStream,
    resolver: &Resolver,
    daemon_uid: u32,
) -> io::Result<()> {
    if !peer_allowed(peer_uid(&stream)?, daemon_uid) {
        return write_frame(
            &mut stream,
            &ResolveResponse::Error {
                version: PROTOCOL_VERSION,
                error: ResolutionError::new(
                    ErrorCategory::Unauthorized,
                    "peer UID does not match resolver UID",
                    false,
                ),
            },
        );
    }
    let request: ResolveRequest = match read_frame(&mut stream) {
        Ok(request) => request,
        Err(_) => {
            return write_frame(
                &mut stream,
                &ResolveResponse::Error {
                    version: PROTOCOL_VERSION,
                    error: ResolutionError::new(
                        ErrorCategory::Protocol,
                        "request frame is invalid",
                        false,
                    ),
                },
            )
        }
    };
    let purpose = request.purpose;
    let response = match if purpose == ResolvePurpose::Inspect {
        resolver
            .inspect(request)
            .map(|(success, closure)| (success, Some(closure)))
    } else {
        resolver
            .resolve_detailed(request)
            .map(|success| (success, None))
    } {
        Ok((success, closure)) => ResolveResponse::Success {
            version: PROTOCOL_VERSION,
            resolution: Box::new(success.resolution),
            timings: success.timings,
            closure: closure.map(Box::new),
        },
        Err(error) => {
            // The requesting runtime relays this diagnostic to a stderr the
            // engine may discard (containerd keeps no create-time stderr);
            // the daemon journal is the durable copy.
            eprintln!("imageless-resolver: resolution failed: {error}");
            ResolveResponse::Error {
                version: PROTOCOL_VERSION,
                error,
            }
        }
    };
    write_frame(&mut stream, &response)
}

#[cfg_attr(not(feature = "daemon"), allow(dead_code))]
pub fn serve(socket_path: &Path, resolver: Resolver) -> io::Result<()> {
    let parent = socket_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no parent"))?;
    std::fs::create_dir_all(parent)?;
    match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(socket_path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "refusing to replace a non-socket resolver path",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    let active = Arc::new(AtomicUsize::new(0));
    let daemon_uid = effective_uid();
    for accepted in listener.incoming() {
        let mut stream = accepted?;
        if !peer_uid(&stream).is_ok_and(|uid| peer_allowed(uid, daemon_uid)) {
            let _ = write_frame(
                &mut stream,
                &ResolveResponse::Error {
                    version: PROTOCOL_VERSION,
                    error: ResolutionError::new(
                        ErrorCategory::Unauthorized,
                        "peer UID does not match resolver UID",
                        false,
                    ),
                },
            );
            continue;
        }
        if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::AcqRel);
            let _ = write_frame(
                &mut stream,
                &ResolveResponse::Error {
                    version: PROTOCOL_VERSION,
                    error: ResolutionError::new(
                        ErrorCategory::Overloaded,
                        "resolver connection limit reached",
                        true,
                    ),
                },
            );
            continue;
        }
        let resolver = resolver.clone();
        let active = Arc::clone(&active);
        thread::spawn(move || {
            struct Active(Arc<AtomicUsize>);
            impl Drop for Active {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _active = Active(active);
            let _ = handle_connection(stream, &resolver, daemon_uid);
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{fake_nix, request, resolver, temporary, RELEASE_REF, STORE};
    use std::os::unix::fs::symlink;
    use std::sync::Barrier;

    #[test]
    fn version_rejection_is_categorized() {
        let dir = temporary("version");
        let nix = fake_nix(&dir, "true");
        let error = resolver(nix, 2, Duration::from_secs(2))
            .resolve(ResolveRequest {
                version: 99,
                purpose: ResolvePurpose::Runtime,
                materialize: Materialize::Closure(STORE.into()),
                bundle_path: dir.clone(),
                timeout_ms: 1000,
                container_name: None,
            })
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::Protocol);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prewarm_purpose_is_explicit_and_release_only() {
        let decoded: ResolveRequest = serde_json::from_value(serde_json::json!({
            "version": PROTOCOL_VERSION,
            "materialize": {"kind": "closure", "value": STORE},
            "bundle_path": "/tmp",
            "timeout_ms": 1000
        }))
        .unwrap();
        assert_eq!(decoded.purpose, ResolvePurpose::Runtime);

        let dir = temporary("prewarm-purpose");
        let bundle = dir.join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        let nix = fake_nix(&dir, "true");
        let error = resolver(nix, 2, Duration::from_secs(2))
            .resolve(ResolveRequest {
                version: PROTOCOL_VERSION,
                purpose: ResolvePurpose::Prewarm,
                materialize: Materialize::Closure(STORE.into()),
                bundle_path: bundle,
                timeout_ms: 1000,
                container_name: None,
            })
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::InvalidRequest);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unauthorized_issuer_is_rejected_before_any_nix_work() {
        let dir = temporary("issuer-policy");
        let nix = fake_nix(&dir, "true");
        let error = resolver(nix, 2, Duration::from_secs(2))
            .resolve(request(
                dir.clone(),
                Materialize::Release(ReleaseReference::parse(RELEASE_REF).unwrap()),
                1000,
            ))
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::PolicyDenied);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn development_evaluation_requires_the_unprivileged_worker() {
        let dir = temporary("development-worker-required");
        let nix = fake_nix(&dir, "true");
        let error = resolver(nix, 2, Duration::from_secs(2))
            .resolve(request(
                dir.clone(),
                Materialize::Flake("path:/source#rootfs".to_string()),
                1000,
            ))
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::EvaluationDisabled);
        assert!(!dir.join(GC_ROOT_NAME).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn socket_handler_authenticates_peer_and_frames_version_errors() {
        let dir = temporary("socket-protocol");
        let nix = fake_nix(&dir, "true");
        let resolver = resolver(nix, 2, Duration::from_secs(2));

        let (mut client, server) = UnixStream::pair().unwrap();
        let rejected = resolver.clone();
        let unauthorized = thread::spawn(move || {
            handle_connection(server, &rejected, effective_uid().wrapping_add(1)).unwrap()
        });
        let response: ResolveResponse = read_frame(&mut client).unwrap();
        assert!(matches!(
            response,
            ResolveResponse::Error {
                error: ResolutionError {
                    category: ErrorCategory::Unauthorized,
                    ..
                },
                ..
            }
        ));
        unauthorized.join().unwrap();

        let (mut client, server) = UnixStream::pair().unwrap();
        let rejected = resolver.clone();
        let handler =
            thread::spawn(move || handle_connection(server, &rejected, effective_uid()).unwrap());
        write_frame(
            &mut client,
            &ResolveRequest {
                version: PROTOCOL_VERSION + 1,
                purpose: ResolvePurpose::Runtime,
                materialize: Materialize::Closure(STORE.into()),
                bundle_path: dir.clone(),
                timeout_ms: 1000,
                container_name: None,
            },
        )
        .unwrap();
        let response: ResolveResponse = read_frame(&mut client).unwrap();
        assert!(matches!(
            response,
            ResolveResponse::Error {
                error: ResolutionError {
                    category: ErrorCategory::Protocol,
                    ..
                },
                ..
            }
        ));
        handler.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn eight_identical_requests_share_materialization_and_get_eight_roots() {
        let dir = temporary("single-flight");
        let log = dir.join("calls");
        let nix = fake_nix(&dir, "printf x >> \"$FAKE_LOG\"; sleep 0.08");
        let resolver = resolver(nix, 2, Duration::from_secs(5));
        let barrier = Arc::new(Barrier::new(9));
        let bundles: Vec<_> = (0..8)
            .map(|index| {
                let bundle = dir.join(format!("bundle-{index}"));
                std::fs::create_dir(&bundle).unwrap();
                bundle
            })
            .collect();
        let threads: Vec<_> = bundles
            .iter()
            .map(|bundle| {
                let bundle = bundle.clone();
                let resolver = resolver.clone();
                let log = log.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    std::env::set_var("FAKE_STORE", STORE);
                    std::env::set_var("FAKE_LOG", log);
                    barrier.wait();
                    resolver
                        .resolve(request(bundle, Materialize::Closure(STORE.into()), 4000))
                        .unwrap()
                })
            })
            .collect();
        barrier.wait();
        for thread in threads {
            assert_eq!(thread.join().unwrap().rootfs, STORE);
        }
        // One leader materialization plus seven necessary per-bundle registrations.
        assert_eq!(std::fs::read_to_string(log).unwrap().len(), 8);
        for bundle in &bundles {
            assert_eq!(
                std::fs::read_link(bundle.join(GC_ROOT_NAME)).unwrap(),
                Path::new(STORE)
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn shared_failure_is_removed_and_a_later_request_retries() {
        let dir = temporary("retry");
        let marker = dir.join("first");
        let nix = fake_nix(
            &dir,
            "if [ ! -e \"$FAIL_MARKER\" ]; then touch \"$FAIL_MARKER\"; sleep 0.1; exit 7; fi",
        );
        std::env::set_var("FAKE_STORE", STORE);
        std::env::set_var("FAIL_MARKER", &marker);
        let resolver = resolver(nix, 2, Duration::from_secs(3));
        let one = dir.join("one");
        let two = dir.join("two");
        std::fs::create_dir(&one).unwrap();
        std::fs::create_dir(&two).unwrap();
        let leader = {
            let resolver = resolver.clone();
            thread::spawn(move || {
                resolver.resolve(request(one, Materialize::Closure(STORE.into()), 2000))
            })
        };
        thread::sleep(Duration::from_millis(20));
        let follower = resolver.resolve(request(
            two.clone(),
            Materialize::Closure(STORE.into()),
            2000,
        ));
        assert_eq!(
            leader.join().unwrap().unwrap_err().category,
            ErrorCategory::Materialization
        );
        // The failure was the leader's (exit 7 is a retryable Materialization
        // error): the follower retries on its own deadline instead of
        // inheriting it, and the failed flight is gone for later callers.
        assert_eq!(follower.unwrap().rootfs, STORE);
        assert_eq!(
            resolver
                .resolve(request(two, Materialize::Closure(STORE.into()), 2000))
                .unwrap()
                .rootfs,
            STORE
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn deadlines_cover_materializer_flight_and_semaphore_waiting() {
        let dir = temporary("deadlines");
        let nix = fake_nix(&dir, "sleep 0.3");
        std::env::set_var("FAKE_STORE", STORE);
        // The leader must outlive three 40 ms requests without hitting its own
        // deadline: it sleeps 0.3 s twice (realization, then its GC root), and
        // a 1 s deadline left only 1.6x of headroom — enough to fail roughly
        // one run in ten with the rest of the suite loading the machine.
        let resolver = resolver(nix, 1, Duration::from_secs(8));
        let one = dir.join("one");
        let two = dir.join("two");
        let three = dir.join("three");
        for bundle in [&one, &two, &three] {
            std::fs::create_dir(bundle).unwrap();
        }
        let leader = {
            let resolver = resolver.clone();
            thread::spawn(move || {
                resolver.resolve(request(one, Materialize::Closure(STORE.into()), 6000))
            })
        };
        thread::sleep(Duration::from_millis(25));
        let flight = resolver
            .resolve(request(two, Materialize::Closure(STORE.into()), 40))
            .unwrap_err();
        assert_eq!(flight.category, ErrorCategory::Timeout);
        let semaphore = resolver
            .resolve(request(
                three,
                Materialize::Closure("/nix/store/11111111111111111111111111111111-other".into()),
                40,
            ))
            .unwrap_err();
        assert_eq!(semaphore.category, ErrorCategory::Timeout);
        leader.join().unwrap().unwrap();

        let four = dir.join("four");
        std::fs::create_dir(&four).unwrap();
        let materializer = resolver
            .resolve(request(four, Materialize::Closure(STORE.into()), 40))
            .unwrap_err();
        assert_eq!(materializer.category, ErrorCategory::Timeout);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn distinct_requests_never_exceed_two_external_nix_operations() {
        let dir = temporary("global-limit");
        let state = dir.join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(state.join("current"), "0").unwrap();
        std::fs::write(state.join("maximum"), "0").unwrap();
        let body = format!(
            r#"
while ! mkdir "{state}/lock" 2>/dev/null; do sleep 0.005; done
current=$(cat "{state}/current"); current=$((current + 1)); printf '%s' "$current" > "{state}/current"
maximum=$(cat "{state}/maximum"); if [ "$current" -gt "$maximum" ]; then printf '%s' "$current" > "{state}/maximum"; fi
rmdir "{state}/lock"
sleep 0.08
while ! mkdir "{state}/lock" 2>/dev/null; do sleep 0.005; done
current=$(cat "{state}/current"); current=$((current - 1)); printf '%s' "$current" > "{state}/current"
rmdir "{state}/lock"
"#,
            state = state.display()
        );
        let nix = fake_nix(&dir, &body);
        std::env::set_var("FAKE_STORE", STORE);
        let resolver = resolver(nix, 2, Duration::from_secs(5));
        let threads: Vec<_> = (0..6)
            .map(|index| {
                let bundle = dir.join(format!("distinct-{index}"));
                std::fs::create_dir(&bundle).unwrap();
                let resolver = resolver.clone();
                thread::spawn(move || {
                    let path = format!("/nix/store/{:032}-request-{index}", index + 1);
                    resolver.resolve(request(bundle, Materialize::Closure(path), 4000))
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        assert_eq!(std::fs::read_to_string(state.join("maximum")).unwrap(), "2");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn follower_gc_root_registration_uses_the_original_deadline() {
        let dir = temporary("root-timeout");
        let leader_started = dir.join("leader-started");
        // The follower's deadline has to outlast the leader by a wide margin
        // and expire well inside its own root registration, because a deadline
        // that runs out in the *wait* is a timeout too — with a different
        // diagnostic, which is exactly what this test distinguishes. The
        // leader's nominal ~160 ms of work (realization, then its own GC root)
        // is what the follower waits through, and the original 250 ms deadline
        // left only 1.6x of headroom for it — enough to fail roughly one run in
        // six with the rest of the suite loading the machine.
        let nix = fake_nix(
            &dir,
            &format!(
                "case \"$root\" in *slow-root*) sleep 4 ;; *leader*) touch {}; sleep 0.08 ;; esac",
                leader_started.display()
            ),
        );
        std::env::set_var("FAKE_STORE", STORE);
        let resolver = resolver(nix, 2, Duration::from_secs(6));
        let leader_bundle = dir.join("leader");
        let slow_bundle = dir.join("slow-root");
        std::fs::create_dir(&leader_bundle).unwrap();
        std::fs::create_dir(&slow_bundle).unwrap();
        let leader = {
            let resolver = resolver.clone();
            thread::spawn(move || {
                resolver.resolve(request(
                    leader_bundle,
                    Materialize::Closure(STORE.into()),
                    1000,
                ))
            })
        };
        let wait_started = Instant::now();
        while !leader_started.exists() {
            assert!(wait_started.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(5));
        }
        let error = resolver
            .resolve(request(
                slow_bundle,
                Materialize::Closure(STORE.into()),
                1500,
            ))
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::Timeout);
        assert!(error.diagnostic.contains("GC-root registration"));
        leader.join().unwrap().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reserved_root_collision_is_refused_and_stale_symlink_is_replaced() {
        let dir = temporary("roots");
        let nix = fake_nix(&dir, "true");
        std::env::set_var("FAKE_STORE", STORE);
        let resolver = resolver(nix, 1, Duration::from_secs(2));
        let collision = dir.join("collision");
        std::fs::create_dir(&collision).unwrap();
        std::fs::write(collision.join(GC_ROOT_NAME), "reserved").unwrap();
        assert_eq!(
            resolver
                .resolve(request(collision, Materialize::Closure(STORE.into()), 1000,))
                .unwrap_err()
                .category,
            ErrorCategory::RootCollision
        );

        let stale = dir.join("stale");
        std::fs::create_dir(&stale).unwrap();
        symlink(
            "/nix/store/22222222222222222222222222222222-stale",
            stale.join(GC_ROOT_NAME),
        )
        .unwrap();
        resolver
            .resolve(request(
                stale.clone(),
                Materialize::Closure(STORE.into()),
                1000,
            ))
            .unwrap();
        assert_eq!(
            std::fs::read_link(stale.join(GC_ROOT_NAME)).unwrap(),
            Path::new(STORE)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn in_process_resolution_fails_closed_without_a_node_policy() {
        let dir = temporary("in-process-default");
        let bundle = dir.join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        let absent_default = dir.join("policy.json");

        let error = resolve_in_process_at(
            &PolicySource::File(None),
            &absent_default,
            &request(
                bundle.clone(),
                Materialize::Flake("path:/source#rootfs".to_string()),
                1000,
            ),
        )
        .unwrap_err();
        assert_eq!(error.category, ErrorCategory::EvaluationDisabled);
        assert!(error
            .diagnostic
            .contains(&format!("write {}", absent_default.display())));
        assert!(!bundle.join(GC_ROOT_NAME).exists());

        // An explicitly configured policy path must load; it never defaults.
        let explicit = resolve_in_process_at(
            &PolicySource::File(Some(dir.join("missing-explicit.json"))),
            &absent_default,
            &request(
                bundle,
                Materialize::Flake("path:/source#rootfs".to_string()),
                1000,
            ),
        )
        .unwrap_err();
        assert_eq!(explicit.category, ErrorCategory::PolicyDenied);
        assert!(explicit.diagnostic.contains("could not be loaded"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn staged_locks_may_not_pin_node_local_inputs() {
        let dir = temporary("staged-lock");
        let lock = |nodes: serde_json::Value| {
            std::fs::write(
                dir.join("flake.lock"),
                serde_json::to_vec(
                    &serde_json::json!({ "nodes": nodes, "root": "root", "version": 7 }),
                )
                .unwrap(),
            )
            .unwrap();
            refuse_local_locked_inputs(&dir)
        };
        // No lock at all is the lock-less seed: gated by --no-update-lock-file
        // under confinement, not here.
        assert!(refuse_local_locked_inputs(&dir).is_ok());
        // Remote inputs and in-tree relative subflakes are ordinary.
        assert!(lock(serde_json::json!({
            "root": { "inputs": { "nixpkgs": "nixpkgs", "sub": "sub" } },
            "nixpkgs": { "locked": { "type": "github", "owner": "nixos", "repo": "nixpkgs", "rev": "0" } },
            "sub": { "locked": { "type": "path", "path": "./sub" }, "parent": [] },
        }))
        .is_ok());
        for (locked, reason) in [
            (
                serde_json::json!({ "type": "path", "path": "/srv/data" }),
                "absolute path",
            ),
            (
                serde_json::json!({ "type": "path", "path": "../../etc" }),
                "escapes its flake",
            ),
            (
                serde_json::json!({ "type": "git", "url": "file:///srv/repo" }),
                "local file URL",
            ),
            (
                serde_json::json!({ "type": "tarball", "url": "FILE:/srv/a.tar" }),
                "local file URL",
            ),
            (
                serde_json::json!({ "type": "git", "url": "/srv/repo" }),
                "local file URL",
            ),
        ] {
            let error = lock(serde_json::json!({
                "root": { "inputs": { "data": "data" } },
                "data": { "locked": locked, "flake": false },
            }))
            .unwrap_err();
            assert_eq!(error.category, ErrorCategory::InvalidRequest);
            assert!(error.diagnostic.contains("`data`"), "{}", error.diagnostic);
            assert!(error.diagnostic.contains(reason), "{}", error.diagnostic);
        }
        std::fs::write(dir.join("flake.lock"), "{ not json").unwrap();
        assert!(refuse_local_locked_inputs(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn evaluation_failures_name_the_node_setting_that_explains_them() {
        assert!(
            evaluation_hint("flake 'path:/x' requires lock file changes", false)
                .contains("allow_unlocked_inputs")
        );
        assert!(
            evaluation_hint("… while updating the lock file of flake 'path:/x'", true)
                .contains("allow_unlocked_inputs")
        );
        assert!(
            evaluation_hint("Operation not permitted (os error 1)", true)
                .contains("unconfined_evaluation")
        );
        assert_eq!(
            evaluation_hint("Operation not permitted (os error 1)", false),
            ""
        );
    }

    #[test]
    fn in_process_policy_enables_caller_evaluation_without_a_worker() {
        let dir = temporary("in-process-eval");
        let bundle = dir.join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        let source = dir.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("flake.nix"), "{ outputs = _: { }; }").unwrap();

        // The fake evaluator logs its argv next to itself, which a confined
        // evaluator cannot write; confinement has its own test.
        let policy = dir.join("policy.json");
        std::fs::write(
            &policy,
            r#"{"system":"x86_64-linux","cache_only":false,"eval_allowed_uri_prefixes":["path:"],"issuers":{},"unconfined_evaluation":true}"#,
        )
        .unwrap();
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();

        let log = dir.join("eval-args");
        let eval_nix = dir.join("fake-eval-nix");
        crate::testutil::executable(
            &eval_nix,
            &format!(
                "printf '%s\\n' \"$@\" >> {log}\nprintf '%s\\n' {STORE}",
                log = log.display()
            ),
        );
        std::env::set_var("IMAGELESS_NIX", &eval_nix);
        std::env::set_var("IMAGELESS_NIX_STORE", fake_nix(&dir, "true"));
        std::env::set_var("FAKE_STORE", STORE);

        let success = resolve_in_process_at(
            &PolicySource::File(None),
            &policy,
            &request(
                bundle.clone(),
                Materialize::Flake(format!("path:{}#rootfs", source.display())),
                5000,
            ),
        )
        .unwrap();
        std::env::remove_var("IMAGELESS_NIX");
        std::env::remove_var("IMAGELESS_NIX_STORE");

        assert_eq!(success.resolution.rootfs, STORE);
        assert_eq!(success.resolution.identity, "development-source");
        assert_eq!(
            std::fs::read_link(bundle.join(GC_ROOT_NAME)).unwrap(),
            Path::new(STORE)
        );
        let arguments = std::fs::read_to_string(&log).unwrap();
        // Evaluation ran as the caller (plain `nix build`, no worker flags) on
        // a staged copy of the source, with no imageless-dev user involved.
        assert!(arguments.contains("--print-out-paths"));
        assert!(!arguments.contains("--user"));
        assert!(arguments.contains(".imageless-source-"));
        // A staged in-image flake evaluates against the lock it ships.
        assert!(arguments.contains("--no-update-lock-file"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(feature = "inline-policy")]
    #[test]
    fn inline_policy_authorizes_evaluation_without_a_file() {
        let dir = temporary("in-process-inline");
        let bundle = dir.join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        let source = dir.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("flake.nix"), "{ outputs = _: { }; }").unwrap();

        let log = dir.join("eval-args");
        let eval_nix = dir.join("fake-eval-nix");
        crate::testutil::executable(
            &eval_nix,
            &format!(
                "printf '%s\\n' \"$@\" >> {log}\nprintf '%s\\n' {STORE}",
                log = log.display()
            ),
        );
        std::env::set_var("IMAGELESS_NIX", &eval_nix);
        std::env::set_var("IMAGELESS_NIX_STORE", fake_nix(&dir, "true"));
        std::env::set_var("FAKE_STORE", STORE);

        // The same permissive policy, handed in inline rather than through a
        // file — no ownership check, and it authorizes exactly as the file did.
        // Unconfined for the same reason as the file-policy test above.
        let inline = PolicySource::Inline(
            r#"{"system":"x86_64-linux","cache_only":false,"eval_allowed_uri_prefixes":["path:"],"issuers":{},"unconfined_evaluation":true}"#
                .to_string(),
        );
        let success = resolve_in_process_at(
            &inline,
            Path::new("/nonexistent/default-policy.json"),
            &request(
                bundle.clone(),
                Materialize::Flake(format!("path:{}#rootfs", source.display())),
                5000,
            ),
        )
        .unwrap();
        std::env::remove_var("IMAGELESS_NIX");
        std::env::remove_var("IMAGELESS_NIX_STORE");
        assert_eq!(success.resolution.rootfs, STORE);
        assert_eq!(success.resolution.identity, "development-source");

        // Malformed inline policy fails closed and never defaults.
        let bad = resolve_in_process_at(
            &PolicySource::Inline("{ not json".to_string()),
            Path::new("/nonexistent/default-policy.json"),
            &request(
                bundle,
                Materialize::Flake(format!("path:{}#rootfs", source.display())),
                5000,
            ),
        )
        .unwrap_err();
        assert_eq!(bad.category, ErrorCategory::PolicyDenied);
        assert!(bad.diagnostic.contains("IMAGELESS_POLICY_JSON"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn evaluation_resolver(prefixes: &[&str], tools: Option<(&Path, &Path)>) -> Resolver {
        let mut config = ResolverConfig::from_environment(2, 3600);
        config.policy = ResolverPolicy {
            system: "x86_64-linux".to_string(),
            cache_only: false,
            eval_allowed_uri_prefixes: prefixes.iter().map(|p| p.to_string()).collect(),
            issuers: HashMap::new(),
            unconfined_evaluation: true,
            allow_unlocked_inputs: false,
        };
        config.evaluate_as_caller = true;
        if let Some((nix, nix_store)) = tools {
            config.nix = nix.to_path_buf();
            config.nix_store = nix_store.to_path_buf();
        }
        Resolver::new(config)
    }

    #[test]
    fn a_follower_retries_a_retryable_failure_instead_of_inheriting_it() {
        let dir = temporary("flight-retry");
        let first = dir.join("first-call");
        let eval_nix = dir.join("fake-eval-nix");
        // The first evaluation fails (retryable: a Materialization error)
        // after long enough for a second caller to join its flight; every
        // later evaluation succeeds.
        crate::testutil::executable(
            &eval_nix,
            &format!(
                "if mkdir {first} 2>/dev/null; then sleep 1; echo transient >&2; exit 1; fi\n\
                 printf '%s\\n' {STORE}",
                first = first.display()
            ),
        );
        std::env::set_var("FAKE_STORE", STORE);
        let store_nix = fake_nix(&dir, "true");
        let resolver =
            evaluation_resolver(&["git+https://git.example/"], Some((&eval_nix, &store_nix)));
        let installable = "git+https://git.example/app?rev=0123456789abcdef#rootfs";
        let call = |label: &str| {
            let bundle = dir.join(label);
            std::fs::create_dir(&bundle).unwrap();
            let resolver = resolver.clone();
            std::thread::spawn(move || {
                resolver.resolve_detailed(request(
                    bundle,
                    Materialize::Flake(installable.to_string()),
                    10_000,
                ))
            })
        };
        let leader = call("leader");
        std::thread::sleep(Duration::from_millis(300));
        let follower = call("follower");
        let leader = leader.join().unwrap();
        let follower = follower.join().unwrap();
        assert_eq!(leader.unwrap_err().category, ErrorCategory::Materialization);
        let follower = follower.expect("the follower inherited the leader's failure");
        assert_eq!(follower.resolution.rootfs, STORE);
        // The roots point at a fake store path: look at the links themselves.
        assert!(std::fs::symlink_metadata(dir.join("follower").join(GC_ROOT_NAME)).is_ok());
        assert!(std::fs::symlink_metadata(dir.join("leader").join(GC_ROOT_NAME)).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn external_references_are_denied_without_an_allow_listed_prefix() {
        let dir = temporary("external-prefix-denied");
        let bundle = dir.join("bundle");
        std::fs::create_dir(&bundle).unwrap();

        // An embedded-flake policy (`path:` only) must not authorize an
        // external reference, and the denial happens before any Nix work.
        let denied = evaluation_resolver(&["path:"], None)
            .resolve_detailed(request(
                bundle.clone(),
                Materialize::Flake("github:acme/app?rev=0123456#rootfs".to_string()),
                1000,
            ))
            .unwrap_err();
        assert_eq!(denied.category, ErrorCategory::PolicyDenied);
        assert!(denied.diagnostic.contains("not authorized by node policy"));

        // An empty allow-list denies every source, staged rewrites included.
        let empty = evaluation_resolver(&[], None)
            .resolve_detailed(request(
                bundle.clone(),
                Materialize::Flake("path:/staged/source#rootfs".to_string()),
                1000,
            ))
            .unwrap_err();
        assert_eq!(empty.category, ErrorCategory::PolicyDenied);
        assert!(!bundle.join(GC_ROOT_NAME).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn daemon_rejects_node_local_schemes_regardless_of_policy() {
        let dir = temporary("external-scheme-rejected");
        let bundle = dir.join("bundle");
        std::fs::create_dir(&bundle).unwrap();

        // Even a policy that allow-lists these prefixes cannot launder them:
        // the scheme rules hold at the daemon boundary, before the allow-list
        // is consulted (SPEC §3 — rejected before policy).
        for (prefixes, installable) in [
            (["file://"].as_slice(), "file:///etc#rootfs"),
            (&["git+file://"], "git+file:///srv/repo#rootfs"),
            (&["flake:"], "flake:nixpkgs#rootfs"),
            (&["nixpkgs"], "nixpkgs#rootfs"),
            // The staging rewrite is lowercase `path:`; a case-mangled
            // spelling is not the runtime's rewrite and stays node-local.
            (&["PATH:"], "PATH:/etc#rootfs"),
        ] {
            let error = evaluation_resolver(prefixes, None)
                .resolve_detailed(request(
                    bundle.clone(),
                    Materialize::Flake(installable.to_string()),
                    1000,
                ))
                .unwrap_err();
            assert_eq!(
                error.category,
                ErrorCategory::InvalidRequest,
                "{installable} must be rejected as node-local"
            );
            assert!(error.diagnostic.contains("remote scheme"));
        }
        assert!(!bundle.join(GC_ROOT_NAME).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A seed tree with a nested directory, so the directory stamp is covered
    /// too: Nix folds both file and directory mtimes into the fingerprint.
    fn seed_tree(root: &Path) {
        std::fs::create_dir_all(root.join("nested/deeper")).unwrap();
        std::fs::write(root.join("flake.nix"), "{ outputs = _: { }; }\n").unwrap();
        std::fs::write(root.join("nested/deeper/data"), "payload").unwrap();
        std::fs::write(root.join("nested/run.sh"), "#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(
            root.join("nested/run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    fn stage(source: &Path) -> (String, StagedDevelopmentSource) {
        stage_development_installable(
            &format!("path:{}#rootfs", source.display()),
            None,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap()
        .expect("a path: installable is staged")
    }

    fn walk(root: &Path, visit: &mut impl FnMut(&Path, &std::fs::Metadata)) {
        let metadata = std::fs::symlink_metadata(root).unwrap();
        if metadata.is_dir() {
            for entry in std::fs::read_dir(root).unwrap() {
                walk(&entry.unwrap().path(), visit);
            }
        }
        visit(root, &metadata);
    }

    #[test]
    fn staged_copies_carry_epoch_mtimes_on_files_and_directories() {
        // Nix keys a `path:` flake's evaluation cache on mtimes and not on the
        // staged path, so stamping "now" made every create of an unchanged seed
        // miss that cache. Directories count as much as files, which is why the
        // seed tree here is nested rather than flat.
        let dir = temporary("staged-mtimes");
        let source = dir.join("seed");
        seed_tree(&source);
        let (_, staged) = stage(&source);

        let mut checked = 0;
        walk(&staged.directory, &mut |path, metadata| {
            assert_eq!(
                metadata.mtime(),
                0,
                "{} was staged with mtime {}",
                path.display(),
                metadata.mtime()
            );
            checked += 1;
        });
        // The tree, not just its root: 3 directories plus 3 files.
        assert_eq!(checked, 6, "the whole staged tree must be stamped");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn two_stagings_of_one_seed_are_identical_apart_from_their_path() {
        // The property the evaluation cache actually keys on. Staging stays
        // per-create and nonce'd; what changes is that two creates of the same
        // seed now present Nix with indistinguishable trees.
        let dir = temporary("staged-stable");
        let source = dir.join("seed");
        seed_tree(&source);

        let mut fingerprints = Vec::new();
        for _ in 0..2 {
            let (_, staged) = stage(&source);
            let mut entries = Vec::new();
            walk(&staged.directory, &mut |path, metadata| {
                entries.push((
                    path.strip_prefix(&staged.directory).unwrap().to_path_buf(),
                    metadata.mtime(),
                    metadata.mode(),
                    metadata.is_dir(),
                ));
            });
            entries.sort();
            fingerprints.push(entries);
        }
        assert_eq!(fingerprints[0], fingerprints[1]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn staged_content_and_modes_survive_the_mtime_pin() {
        // utimensat runs after the chmod/chown, so this pins that stamping the
        // tree did not undo the access control staging exists to apply.
        let dir = temporary("staged-content");
        let source = dir.join("seed");
        seed_tree(&source);
        let (installable, staged) = stage(&source);

        assert!(installable.ends_with("#rootfs"));
        assert_eq!(
            std::fs::read_to_string(staged.directory.join("nested/deeper/data")).unwrap(),
            "payload"
        );
        // Executability is preserved as the 0o750/0o640 split staging applies.
        let script = std::fs::metadata(staged.directory.join("nested/run.sh")).unwrap();
        assert_eq!(script.mode() & 0o777, 0o750);
        let data = std::fs::metadata(staged.directory.join("nested/deeper/data")).unwrap();
        assert_eq!(data.mode() & 0o777, 0o640);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn external_references_evaluate_unstaged_with_their_own_identity() {
        let dir = temporary("external-accepted");
        let bundle = dir.join("bundle");
        std::fs::create_dir(&bundle).unwrap();

        let log = dir.join("eval-args");
        let eval_nix = dir.join("fake-eval-nix");
        crate::testutil::executable(
            &eval_nix,
            &format!(
                "printf '%s\\n' \"$@\" >> {log}\nprintf '%s\\n' {STORE}",
                log = log.display()
            ),
        );

        let installable = "git+https://git.example/app?rev=0123456789abcdef#rootfs";
        std::env::set_var("FAKE_STORE", STORE);
        let store_nix = fake_nix(&dir, "true");
        let success =
            evaluation_resolver(&["git+https://git.example/"], Some((&eval_nix, &store_nix)))
                .resolve_detailed(request(
                    bundle.clone(),
                    Materialize::Flake(installable.to_string()),
                    5000,
                ))
                .unwrap();

        // External references are evaluated verbatim — staging is for the
        // in-image `path:` rewrite only — and report a distinct identity so
        // telemetry can tell the two evaluation modes apart.
        assert_eq!(success.resolution.rootfs, STORE);
        assert_eq!(success.resolution.identity, "external-source");
        let arguments = std::fs::read_to_string(&log).unwrap();
        assert!(arguments.contains(installable));
        assert!(!arguments.contains(".imageless-source-"));
        assert_eq!(
            std::fs::read_link(bundle.join(GC_ROOT_NAME)).unwrap(),
            Path::new(STORE)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
