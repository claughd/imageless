//! kubectl plugin front end: hand-rolled argv, no framework.
//!
//! `kubectl imageless run ./dir --repo HOST/REPO -- CMD` packs the directory
//! into a seed OCI image exactly as the node would stage it, pushes it to the
//! repository by digest, prints the layer/config/manifest digests on stderr,
//! and prints the pod manifest on stdout so it pipes straight into
//! `kubectl apply -f -`. `--dry-run` stops before the push, fully offline.
//!
//! `run --external <flake-ref>` packs nothing: the pod names the reference and
//! the node evaluates it under node policy. Which mode runs is decided by the
//! flag alone — never by what the argument looks like or by what exists on
//! disk, so a trust-model switch can never hinge on the working directory's
//! contents.
//!
//! Within the packed family the argument's *shape* does decide one thing: a
//! regular file is a shebang script, desugared into a generated seed, and a
//! directory is a seed already. That is a `stat`, not a guess about spelling,
//! and both halves push the same kind of image to the same repository — no
//! trust boundary moves.

mod auth;
mod catalog;
mod doctor;
mod flakeref;
mod generate;
mod kubectl;
mod oci;
mod pack;
mod placeholder;
mod podspec;
mod registry;
mod shebang;

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("run") => match parse_run(&arguments[1..]) {
            Ok(ParsedRun::Help) => help(),
            Ok(ParsedRun::Run(options)) => run(&options),
            Err(error) => {
                eprintln!("kubectl-imageless: {error}\n");
                usage()
            }
        },
        Some("doctor") => match parse_doctor(&arguments[1..]) {
            Ok(ParsedDoctor::Help) => {
                println!("{DOCTOR_USAGE}");
                ExitCode::SUCCESS
            }
            Ok(ParsedDoctor::Doctor(options)) => doctor::run(&options),
            Err(error) => {
                eprintln!("kubectl-imageless: {error}\n");
                eprintln!("{DOCTOR_USAGE}");
                ExitCode::from(2)
            }
        },
        Some("pin") => match parse_pin(&arguments[1..]) {
            Ok(ParsedPin::Help) => {
                println!("{PIN_USAGE}");
                ExitCode::SUCCESS
            }
            Ok(ParsedPin::Pin(options)) => pin(&options),
            Err(error) => {
                eprintln!("kubectl-imageless: {error}\n");
                eprintln!("{PIN_USAGE}");
                ExitCode::from(2)
            }
        },
        Some("version") => {
            println!("kubectl-imageless {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("help") | Some("--help") | Some("-h") => help(),
        Some(other) => {
            eprintln!("kubectl-imageless: unknown command `{other}`\n");
            usage()
        }
        None => usage(),
    }
}

const USAGE: &str =
    "kubectl imageless — run a directory, a script, or a flake reference on an imageless cluster\n\
     \n\
     Usage:\n\
     \x20 kubectl imageless run <dir|script> --repo HOST/REPO [flags] -- COMMAND [ARG...]\n\
     \x20 kubectl imageless run --external <flake-ref> [flags] -- COMMAND [ARG...]\n\
     \x20 kubectl imageless run --release <issuer>/<name>[:channel] --catalog SRC \\\n\
     \x20                       [flags] -- COMMAND [ARG...]\n\
     \x20 kubectl imageless pin <issuer>/<name>[:channel] --catalog SRC\n\
     \x20 kubectl imageless doctor [flags]\n\
     \x20 kubectl imageless version\n\
     \n\
     The directory must contain a flake.nix whose output builds the container\n\
     rootfs. It is packed into a seed OCI image under the same bounds the node\n\
     stages it with, so a refusal happens here, with a path, not on the node.\n\
     \n\
     Given a file instead, it must carry a nix shebang —\n\
     \x20 #!/usr/bin/env nix\n\
     \x20 #! nix shell nixpkgs#python3 --command python3\n\
     — and the seed is generated from it: a flake whose rootfs is a buildEnv of\n\
     the named packages plus the script, and a flake.lock pinning the nixpkgs\n\
     this plugin was built against. The pod runs the interpreter against the\n\
     script; anything after -- becomes the script's arguments. Nothing about\n\
     this reaches the node — it receives an ordinary seed, and the generated\n\
     flake is meant to be committed and edited once the script outgrows a\n\
     shebang.\n\
     \n\
     With --external nothing is packed: the pod names the flake reference and\n\
     the node evaluates it under node policy (cache_only: false, an allow-listed\n\
     eval_allowed_uri_prefixes entry, and run.imageless.* passed through the\n\
     containerd handler). Kubernetes still needs an image to create the container\n\
     from: a content-free placeholder is pushed to --repo unless --image names\n\
     one the cluster can already pull.\n\
     \n\
     With --release the pod names a digest-addressed release the node resolves\n\
     against its own issuer catalogs — the cache-only production profile, which\n\
     evaluates nothing. The channel is resolved here, against --catalog, and the\n\
     pod records the resulting digest: republishing the channel afterwards does\n\
     not change what an applied pod runs. Nodes never resolve channels.\n\
     \n\
     Flags:\n\
     \x20 --repo HOST/REPO      repository the pushed manifest belongs to (required\n\
     \x20                       when packing; --image replaces it otherwise)\n\
     \x20 --external            deploy <flake-ref> instead of packing a directory\n\
     \x20 --release             deploy <issuer>/<name>[:channel] instead of packing\n\
     \x20 --catalog SRC         --release only: issuer catalog to resolve the channel\n\
     \x20                       against — an https:// base URL or a local directory\n\
     \x20 --unpinned            allow an --external reference that pins nothing, or a\n\
     \x20                       generated flake that tracks nixos-unstable instead of\n\
     \x20                       the pinned nixpkgs (unlocked: a node refuses it unless\n\
     \x20                       its policy sets allow_unlocked_inputs)\n\
     \x20 --image REF           an image the cluster can already pull (--external and\n\
     \x20                       --release only)\n\
     \x20 --name NAME           pod name (default: derived from the directory, or from\n\
     \x20                       the reference's repository — never its revision)\n\
     \x20 --namespace NS        pod namespace\n\
     \x20 --runtime-class NAME  RuntimeClass of imageless nodes (default: imageless)\n\
     \x20 --output NAME         flake output to materialize (default: rootfs)\n\
     \x20 --arch ARCH           architecture in the image config — the seed itself is\n\
     \x20                       portable, but the node's runtime rejects a mismatch\n\
     \x20                       (default: amd64)\n\
     \x20 --include-vcs         pack .git/.hg/.jj/.svn instead of skipping them\n\
     \x20 --tag TAG             also tag the pushed manifest — the pod stays digest-pinned,\n\
     \x20                       but some registries (GHCR, ECR) garbage-collect untagged\n\
     \x20                       manifests\n\
     \x20 --plain-http          push over http:// to a non-loopback registry (localhost,\n\
     \x20                       *.localhost, 127.0.0.1 and [::1] use http automatically)\n\
     \x20 --dry-run             print digests and the pod manifest; no network\n\
     \x20 --writable PATH       mount an emptyDir at PATH (repeatable) — the materialized\n\
     \x20                       root is read-only, so this is where a workload writes\n\
     \x20 --emit-seed DIR       script only: write the generated seed (flake.nix,\n\
     \x20                       flake.lock, the script) to DIR and stop — the way out\n\
     \x20                       of shebang mode once the script outgrows it";

const PIN_USAGE: &str =
    "kubectl imageless pin — resolve a release channel to the digest a node accepts\n\
     \n\
     Usage:\n\
     \x20 kubectl imageless pin <issuer>/<name>[:channel] --catalog SRC\n\
     \n\
     Reads the catalog's refs/<name>/<channel> pointer and prints\n\
     issuer/name@sha256:<digest> on stdout — the only release form an\n\
     imageless.run/release-v1 annotation may carry. The channel defaults to\n\
     `stable`.\n\
     \n\
     This is client-side only. A node resolves digests and never channels\n\
     (SPEC §6): node-side resolution of a mutable pointer would make what a node\n\
     runs depend on what the catalog said at container start, rather than on\n\
     what the pod's author approved. Pinning here is what keeps that promise —\n\
     the digest is chosen once, by you, and recorded.\n\
     \n\
     Flags:\n\
     \x20 --catalog SRC         issuer catalog: an https:// base URL or a local\n\
     \x20                       directory (required). http:// has no override\n\
     \x20 --timeout-seconds N   bound on the pointer fetch (default: 10)\n\
     \n\
     Exit: 0 resolved, 1 the channel could not be resolved, 2 usage.";

const DOCTOR_USAGE: &str = "kubectl imageless doctor — report whether a cluster is prepared\n\
     \n\
     Usage:\n\
     \x20 kubectl imageless doctor [flags]\n\
     \n\
     Reports what the API server can be asked about: the RuntimeClass, the node\n\
     label it schedules on, and which nodes carry it. The node-local half of the\n\
     seam — containerd's runtime handler, the shim binary, the policy file — has\n\
     no API representation, so a green report is not proof a pod will start.\n\
     \n\
     kubectl's connection flags (--context, --namespace, --kubeconfig, --as, …)\n\
     are forwarded verbatim. They must come after `imageless`: kubectl stops\n\
     collecting the plugin command path at the first flag, so\n\
     `kubectl --context x imageless doctor` never reaches this plugin at all.\n\
     \n\
     Flags:\n\
     \x20 --runtime-class NAME  RuntimeClass to look for (default: imageless)\n\
     \x20 --policy PATH         also check a node policy file (a local copy; doctor\n\
     \x20                       cannot read a node's /etc/imageless/policy.json)\n\
     \x20 --source REF          also check a flake reference against the contract, and\n\
     \x20                       against --policy's prefixes when both are given\n\
     \x20 --repo HOST/REPO      also check registry reachability and credentials\n\
     \x20 --plain-http          reach --repo over http://\n\
     \x20 --json                one JSON document on stdout instead of text\n\
     \x20 --strict              treat warnings as failures (exit 1)\n\
     \n\
     Exit: 0 healthy, 1 a check failed, 2 usage, 3 the cluster could not be probed.";

/// Bound on a channel-pointer fetch, in seconds.
///
/// `pin` exposes this as `--timeout-seconds` because it is the command a
/// release pipeline puts in a loop, where a catalog that hangs would hang the
/// pipeline. `run --release` takes the default and offers no knob: it is
/// already the long-running command of the two — it packs or pushes an image
/// after this — so a second timeout dial there would be tuning the fastest step
/// of a slow operation. Anyone who needs the knob can `pin` and pass the
/// resulting reference.
const POINTER_TIMEOUT_SECONDS: u64 = 10;

#[cfg_attr(test, derive(Debug))]
enum ParsedPin {
    Help,
    Pin(PinOptions),
}

#[cfg_attr(test, derive(Debug))]
struct PinOptions {
    coordinate: String,
    catalog: String,
    timeout: std::time::Duration,
}

fn parse_pin(arguments: &[String]) -> Result<ParsedPin, String> {
    let mut coordinate = None;
    let mut catalog = None;
    let mut timeout = POINTER_TIMEOUT_SECONDS;

    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        let mut value = |flag: &str| match iterator.next() {
            Some(next) if !next.starts_with('-') => Ok(next.clone()),
            _ => Err(format!("{flag} requires a value")),
        };
        match argument.as_str() {
            "--help" | "-h" => return Ok(ParsedPin::Help),
            "--catalog" => catalog = Some(value("--catalog")?),
            "--timeout-seconds" => {
                let raw = value("--timeout-seconds")?;
                timeout = raw
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| (1..=600).contains(seconds))
                    .ok_or_else(|| format!("--timeout-seconds `{raw}` must be 1-600"))?;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            positional if coordinate.is_none() => coordinate = Some(positional.to_string()),
            extra => return Err(format!("unexpected argument `{extra}`")),
        }
    }
    Ok(ParsedPin::Pin(PinOptions {
        coordinate: coordinate.ok_or("a release coordinate is required: issuer/name[:channel]")?,
        catalog: catalog.ok_or(
            "--catalog is required: this command has no node policy to read an issuer's \
             catalog from",
        )?,
        timeout: std::time::Duration::from_secs(timeout),
    }))
}

fn pin(options: &PinOptions) -> ExitCode {
    let resolved = catalog::parse_coordinate(&options.coordinate)
        .and_then(|coordinate| {
            let catalog = catalog::Catalog::parse(&options.catalog)?;
            let digest = catalog::resolve(&catalog, &coordinate, options.timeout)?;
            Ok(format!(
                "{}/{}@sha256:{digest}",
                coordinate.issuer, coordinate.name
            ))
        })
        // The node's own parser has the last word, so a reference this command
        // prints is one a node would accept — including the issuer and name
        // rules `catalog` deliberately does not restate.
        .and_then(|reference| {
            imageless::ReleaseReference::parse(&reference)
                .map(|_| reference)
                .map_err(|error| error.to_string())
        });
    match resolved {
        Ok(reference) => {
            println!("{reference}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

#[cfg_attr(test, derive(Debug))]
enum ParsedDoctor {
    Help,
    Doctor(Box<doctor::Options>),
}

fn parse_doctor(arguments: &[String]) -> Result<ParsedDoctor, String> {
    let (connection, arguments) = kubectl::split_connection_flags(arguments);
    let mut runtime_class = "imageless".to_string();
    let mut json = false;
    let mut strict = false;
    let mut policy = None;
    let mut source = None;
    let mut repo = None;
    let mut plain_http = false;

    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        let mut value = |flag: &str| match iterator.next() {
            Some(next) if !next.starts_with('-') => Ok(next.clone()),
            _ => Err(format!("{flag} requires a value")),
        };
        match argument.as_str() {
            "--help" | "-h" => return Ok(ParsedDoctor::Help),
            "--runtime-class" => runtime_class = value("--runtime-class")?,
            "--policy" => policy = Some(PathBuf::from(value("--policy")?)),
            "--source" => source = Some(value("--source")?),
            "--repo" => repo = Some(value("--repo")?),
            "--plain-http" => plain_http = true,
            "--json" => json = true,
            "--strict" => strict = true,
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            extra => return Err(format!("unexpected argument `{extra}`")),
        }
    }
    Ok(ParsedDoctor::Doctor(Box::new(doctor::Options {
        runtime_class,
        connection,
        json,
        strict,
        policy,
        source,
        repo,
        plain_http,
    })))
}

/// Requested help goes to stdout and succeeds; usage shown on a parse error
/// goes to stderr with the conventional usage-error exit code.
fn help() -> ExitCode {
    println!("{USAGE}");
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

#[cfg_attr(test, derive(Debug))]
enum ParsedRun {
    Help,
    Run(Box<RunOptions>),
}

/// What the pod will run. The variant is chosen by `--external` and
/// `--release` alone: the parser never stats the argument and never inspects
/// its shape, so `run github:owner/repo` packs (and fails) rather than quietly
/// shipping a node-evaluated reference, and a directory genuinely named
/// `github:owner` still packs.
///
/// `Local` covers both packed shapes — a seed directory and a shebang script —
/// because telling them apart is a `stat`, and statting is what this parser
/// does not do. `run_packed` makes that call once, where the error can name the
/// path that was actually looked at.
#[cfg_attr(test, derive(Debug))]
enum Source {
    Local(PathBuf),
    External(String),
    /// A release coordinate as typed — `issuer/name[:channel]`. It is resolved
    /// against `--catalog` before the pod is written, so what lands in the
    /// manifest is always a digest.
    Release(String),
}

#[cfg_attr(test, derive(Debug))]
struct RunOptions {
    source: Source,
    /// `None` only in external mode with `--image`, where nothing is pushed.
    repo: Option<String>,
    /// Issuer catalog for `--release`. Required there and refused elsewhere.
    catalog: Option<String>,
    image: Option<String>,
    unpinned: bool,
    name: Option<String>,
    namespace: Option<String>,
    runtime_class: String,
    output: Option<String>,
    arch: String,
    include_vcs: bool,
    tag: Option<String>,
    plain_http: bool,
    dry_run: bool,
    /// Write the seed a shebang script desugars into, and stop. The escape
    /// hatch out of this mode: a script that has outgrown a shebang becomes an
    /// ordinary directory here, with no step that only the plugin can perform.
    emit_seed: Option<String>,
    /// Paths that get an emptyDir each: the materialized root is read-only.
    writable: Vec<String>,
    command: Vec<String>,
}

fn parse_run(arguments: &[String]) -> Result<ParsedRun, String> {
    let mut source = None;
    let mut repo = None;
    let mut catalog = None;
    let mut external = false;
    let mut release = false;
    let mut unpinned = false;
    let mut image = None;
    let mut name = None;
    let mut namespace = None;
    let mut runtime_class = "imageless".to_string();
    let mut output = None;
    let mut arch = "amd64".to_string();
    let mut include_vcs = false;
    let mut tag = None;
    let mut plain_http = false;
    let mut dry_run = false;
    let mut emit_seed = None;
    let mut writable: Vec<String> = Vec::new();
    let mut command = Vec::new();

    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        let mut value = |flag: &str| {
            // A following flag is a missing value, not a value — accepting it
            // would validate a repo literally named `--dry-run`.
            match iterator.next() {
                Some(next) if !next.starts_with('-') => Ok(next.clone()),
                _ => Err(format!("{flag} requires a value")),
            }
        };
        match argument.as_str() {
            "--" => {
                command = iterator.cloned().collect();
                break;
            }
            "--help" | "-h" => return Ok(ParsedRun::Help),
            "--repo" => repo = Some(value("--repo")?),
            "--catalog" => catalog = Some(value("--catalog")?),
            "--external" => external = true,
            "--release" => release = true,
            "--unpinned" => unpinned = true,
            "--image" => image = Some(value("--image")?),
            "--name" => name = Some(value("--name")?),
            "--namespace" => namespace = Some(value("--namespace")?),
            "--runtime-class" => runtime_class = value("--runtime-class")?,
            "--output" => output = Some(value("--output")?),
            "--arch" => arch = value("--arch")?,
            "--include-vcs" => include_vcs = true,
            "--tag" => tag = Some(value("--tag")?),
            "--plain-http" => plain_http = true,
            "--dry-run" => dry_run = true,
            "--writable" => writable.push(value("--writable")?),
            "--emit-seed" => {
                let directory = value("--emit-seed")?;
                // `create_dir_all("")` is an Ok no-op and `Path::new("").join(x)`
                // is a bare relative name, so an empty value scattered flake.nix,
                // flake.lock and the script into the cwd and reported success
                // with a hole where the destination should be. `--emit-seed
                // "$OUTDIR"` with OUTDIR unset is the way to hit it.
                if directory.is_empty() {
                    return Err("--emit-seed needs a directory".to_string());
                }
                emit_seed = Some(directory);
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            positional if source.is_none() => source = Some(positional.to_string()),
            extra => return Err(format!("unexpected argument `{extra}`")),
        }
    }

    if external && release {
        return Err(
            "pass --external (a flake the node evaluates) or --release (a digest-addressed \
             release the node resolves), not both: SPEC §3 makes the two annotation families \
             mutually exclusive, and a node refuses a pod carrying both"
                .to_string(),
        );
    }
    // Flags that only make sense for one mode are refused rather than ignored:
    // a silently dropped `--tag` looks like it worked.
    let packs = !external && !release;
    if packs {
        if image.is_some() {
            return Err(
                "--image applies to --external and --release; a packed directory's image \
                 is the seed pushed to --repo"
                    .to_string(),
            );
        }
        // `--unpinned` is not refused here the way `--image` is: in the packed
        // family it means something for a shebang script, whose generated
        // flake it un-pins, and nothing for a directory, which pins its own
        // bytes. Which of the two this is takes a `stat`, so the refusal lives
        // in `run_packed` — later, but with the path in hand.
    } else if include_vcs {
        return Err(format!(
            "--include-vcs applies to a packed directory; {} packs nothing",
            if external { "--external" } else { "--release" }
        ));
    }
    if release {
        if unpinned {
            return Err(
                "--unpinned applies to --external references; a release is digest-addressed \
                 by definition, and `pin` resolves the channel before the pod is written"
                    .to_string(),
            );
        }
        if output.is_some() {
            return Err(
                "--output selects a flake output; a release manifest names its own rootfs \
                 and process metadata"
                    .to_string(),
            );
        }
    }
    if catalog.is_some() && !release {
        return Err(
            "--catalog resolves a --release channel; nothing else here consults a catalog"
                .to_string(),
        );
    }
    if image.is_some() {
        if repo.is_some() {
            return Err(
                "pass --repo (to push a placeholder image there) or --image (an image the \
                 cluster can already pull), not both"
                    .to_string(),
            );
        }
        // These three describe the manifest this command pushes, and with
        // `--image` it pushes none.
        for (flag, present) in [("--tag", tag.is_some()), ("--plain-http", plain_http)] {
            if present {
                return Err(format!(
                    "{flag} applies to the manifest this command pushes; --image pushes nothing"
                ));
            }
        }
    }

    let source = source.ok_or(if external {
        "an external flake reference is required after --external"
    } else if release {
        "a release coordinate is required after --release: issuer/name[:channel]"
    } else {
        "a source directory is required"
    })?;
    let source = if external {
        flakeref::validate(&source)?;
        match flakeref::pin(&source)? {
            Some(_) => {}
            None if unpinned => {}
            None => {
                return Err(format!(
                    "{}; pass --unpinned to deploy it anyway",
                    flakeref::unpinned_diagnostic(&source)
                ))
            }
        }
        Source::External(source)
    } else if release {
        // Parsed here so a typo fails before anything is pushed; resolved later,
        // because resolution touches the network.
        catalog::parse_coordinate(&source)?;
        Source::Release(source)
    } else {
        Source::Local(PathBuf::from(source))
    };
    if release && catalog.is_none() {
        return Err(
            "--release needs --catalog: a client has no node policy to look an issuer's \
             catalog up in, and guessing one would resolve a channel against a catalog \
             nobody named"
                .to_string(),
        );
    }
    if !packs && repo.is_none() && image.is_none() {
        return Err(format!(
            "{} needs somewhere to get the pod's image: --repo HOST/REPO to push \
             a placeholder to, or --image REF naming one the cluster can already pull",
            if external { "--external" } else { "--release" }
        ));
    }
    if packs && repo.is_none() && emit_seed.is_none() {
        return Err("--repo HOST/REPO is required".to_string());
    }
    if emit_seed.is_some() && !packs {
        return Err(format!(
            "--emit-seed writes the seed a shebang script generates; {} generates none",
            if external { "--external" } else { "--release" }
        ));
    }
    // --emit-seed stops before anything is pushed, so these would be ignored:
    // refused instead, like every other flag that cannot apply.
    if emit_seed.is_some() {
        for (flag, present) in [
            ("--tag", tag.is_some()),
            ("--plain-http", plain_http),
            ("--dry-run", dry_run),
            ("--writable", !writable.is_empty()),
        ] {
            if present {
                return Err(format!(
                    "{flag} does not apply to --emit-seed, which writes a seed and pushes nothing"
                ));
            }
        }
    }
    // `--image` pushes nothing, so there is no push target to validate — every
    // check below is about one.
    if let Some(repo) = &repo {
        // Requiring a `/` enforces the promised HOST/REPO shape; a bare name
        // would silently resolve against docker.io/library on the node. A colon
        // after that slash is a smuggled tag — the push is digest-addressed,
        // and `--tag` is the one way to name one. Before the slash it is a host
        // port. URL metacharacters are rejected outright: the repository is
        // interpolated into every request target, where a `?` would silently
        // retarget the API path and a `%` would decode into a different
        // repository.
        let path = repo.split_once('/').map(|(_, path)| path);
        if repo.is_empty()
            || repo.contains(char::is_whitespace)
            || repo.contains('@')
            || repo.contains("://")
            || repo.contains(['?', '#', '%', '[', ']', '\\'])
            || !repo.is_ascii()
            || path.is_none_or(|path| {
                path.is_empty() || path.contains(':') || path.split('/').any(str::is_empty)
            })
        {
            return Err(format!(
                "--repo `{repo}` must be a bare repository like registry.example/team/app \
                 (no scheme, tag, or digest)"
            ));
        }
    }
    if let Some(tag) = &tag {
        // The OCI tag grammar; failing here beats a registry's opaque 400.
        let valid = tag.len() <= 128
            && tag
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphanumeric() || first == '_')
            && tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !valid {
            return Err(format!(
                "--tag `{tag}` must match [A-Za-z0-9_][A-Za-z0-9._-]{{0,127}}"
            ));
        }
    }
    // A shebang script is the one source that names its own entrypoint, and
    // whether this is one takes a `stat`. The packed family therefore carries
    // the requirement to `run_packed`, which knows; the reference modes can
    // still enforce it here, where nothing about the argument is in doubt.
    if command.is_empty() && !packs {
        return Err(
            "a workload command is required (the materialized rootfs chooses its own layout, \
             so there is no entrypoint to fall back on); pass it after `--`"
                .to_string(),
        );
    }
    Ok(ParsedRun::Run(Box::new(RunOptions {
        source,
        catalog,
        repo,
        image,
        unpinned,
        name,
        namespace,
        runtime_class,
        output,
        arch,
        include_vcs,
        tag,
        plain_http,
        dry_run,
        emit_seed,
        writable,
        command,
    })))
}

fn run(options: &RunOptions) -> ExitCode {
    match &options.source {
        Source::Local(directory) => run_packed(options, directory),
        Source::External(reference) => run_external(options, reference),
        Source::Release(coordinate) => run_release(options, coordinate),
    }
}

/// Pin-on-apply: resolve the channel now, write the digest into the pod, and
/// never let the coordinate itself reach the manifest.
///
/// This is the whole point of the mode. A node resolves digests and never
/// channels (SPEC §6), so if the channel were carried through, the pod would
/// name something no node would accept. Resolving here means the digest is
/// chosen once, at authoring time, by whoever ran this command — and the pod is
/// a record of that decision rather than a subscription to the catalog.
fn run_release(options: &RunOptions, coordinate: &str) -> ExitCode {
    let catalog_source = options
        .catalog
        .as_deref()
        .expect("release mode requires --catalog");
    let pinned = catalog::parse_coordinate(coordinate).and_then(|coordinate| {
        let catalog = catalog::Catalog::parse(catalog_source)?;
        let digest = catalog::resolve(
            &catalog,
            &coordinate,
            std::time::Duration::from_secs(POINTER_TIMEOUT_SECONDS),
        )?;
        let reference = format!("{}/{}@sha256:{digest}", coordinate.issuer, coordinate.name);
        // The node's parser has the last word, exactly as in `pin`.
        imageless::ReleaseReference::parse(&reference)
            .map_err(|error| error.to_string())
            .map(|_| (coordinate, reference))
    });
    let (coordinate, reference) = match pinned {
        Ok(pinned) => pinned,
        Err(error) => return fail(error),
    };
    eprintln!(
        "notice: `{}/{}:{}` resolved to {reference} — the pod records that digest, so \
         republishing the channel does not change what this pod runs.",
        coordinate.issuer, coordinate.name, coordinate.channel
    );
    eprintln!(
        "notice: the node must allow-list issuer `{}` in its policy, with a catalog it \
         trusts and the substituters the release's closure comes from. This command's \
         --catalog is the client's; a node never reads it.",
        coordinate.issuer
    );

    let pod_image = match (&options.image, &options.repo) {
        (Some(image), _) => {
            eprintln!(
                "notice: --image `{image}` is used verbatim; nothing here checks the node can pull\n\
                 it, and its config's Env, User and WorkingDir still reach the container."
            );
            image.clone()
        }
        (None, Some(repo)) => {
            let image = oci::assemble(placeholder::layer(), &options.arch);
            report_digests(&image);
            eprintln!(
                "notice: the image is a placeholder — Kubernetes requires one and the node replaces\n\
                 the root filesystem from the release manifest. Its flake fails the container\n\
                 create with a diagnosis if imageless.run/release-v1 never reaches the runtime."
            );
            if !options.dry_run {
                if let Err(error) = push(options, repo, &image) {
                    return fail(error);
                }
            }
            format!("{repo}@{}", image.manifest_digest)
        }
        // The parser refuses this combination; reaching it is a bug here.
        (None, None) => return fail("--release needs --repo or --image".to_string()),
    };

    let name = options
        .name
        .clone()
        // The digest is deliberately not part of the name: a pod named after
        // one revision of a channel is a pod nobody can `kubectl get` twice.
        .unwrap_or_else(|| podspec::sanitize_name(&coordinate.name));
    emit(&podspec::PodSpec {
        name: &name,
        namespace: options.namespace.as_deref(),
        image: &pod_image,
        runtime_class: &options.runtime_class,
        deploy: podspec::Deploy::Release(&reference),
        output: options.output.as_deref(),
        command: &options.command,
        writable: &options.writable,
    })
}

/// The one place a packed source's shape is decided. A directory is a seed the
/// author wrote; a regular file is a shebang script this desugars into one.
fn run_packed(options: &RunOptions, path: &Path) -> ExitCode {
    let metadata = std::fs::metadata(path);
    if matches!(&metadata, Ok(metadata) if metadata.is_file()) {
        return run_script(options, path);
    }
    // Only an actual directory earns the "seed directory already" refusal.
    // Gating on "not a regular file" instead told anyone who mistyped a script
    // name that their missing file was a seed directory, and — worse —
    // returned before `packing_failure_hint`, so `run github:owner/repo
    // --unpinned` lost the `pass --external` line that the same command
    // without `--unpinned` prints. Everything else falls through to the
    // packer, whose errors already name the path.
    if matches!(&metadata, Ok(metadata) if metadata.is_dir()) {
        for (flag, present) in [
            ("--unpinned", options.unpinned),
            ("--emit-seed", options.emit_seed.is_some()),
        ] {
            if present {
                return fail(format!(
                    "{flag} applies to a shebang script, whose seed this generates; {} is a seed \
                     directory already",
                    path.display()
                ));
            }
        }
    }
    if options.command.is_empty() {
        return fail(
            "a workload command is required (the materialized rootfs chooses its own layout, \
             so there is no entrypoint to fall back on); pass it after `--`"
                .to_string(),
        );
    }
    // `run_packed` routed here by following symlinks, so the packer must too:
    // a symlink to a seed directory is that directory. Only the root is
    // resolved — symlinks inside the tree are still refused by the packer.
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let packed = match pack::pack_source(
        &resolved,
        &pack::PackOptions {
            include_vcs: options.include_vcs,
        },
    ) {
        Ok(packed) => packed,
        Err(error) => {
            if let Some(hint) = packing_failure_hint(path) {
                eprintln!("kubectl-imageless: {error}");
                eprintln!("{hint}");
                return ExitCode::FAILURE;
            }
            return fail(error);
        }
    };
    let directory = path;
    for path in &packed.skipped_vcs {
        eprintln!(
            "notice: skipped version-control directory {} (pass --include-vcs to pack it)",
            path.display()
        );
    }
    if !packed.has_lock {
        eprintln!(
            "warning: {} has no flake.lock — a node refuses a seed with inputs and no\n\
             complete lock unless its policy sets allow_unlocked_inputs, and a node that\n\
             locks at run time can materialize different closures; commit a flake.lock",
            directory.display()
        );
    }
    eprintln!(
        "packed {} entries, {} bytes (node bounds: {} entries, {} bytes)",
        packed.entries,
        packed.bytes,
        imageless::MAX_STAGED_SOURCE_ENTRIES,
        imageless::MAX_STAGED_SOURCE_BYTES
    );
    let image = oci::assemble(packed.tar, &options.arch);
    report_digests(&image);
    let repo = options
        .repo
        .as_deref()
        .expect("packed mode requires --repo");
    if !options.dry_run {
        if let Err(error) = push(options, repo, &image) {
            return fail(error);
        }
    }

    let name = options
        .name
        .clone()
        .unwrap_or_else(|| podspec::derive_name(directory));
    emit(&podspec::PodSpec {
        name: &name,
        namespace: options.namespace.as_deref(),
        image: &format!("{repo}@{}", image.manifest_digest),
        runtime_class: &options.runtime_class,
        deploy: podspec::Deploy::Source(podspec::EMBEDDED_SOURCE),
        output: options.output.as_deref(),
        command: &options.command,
        writable: &options.writable,
    })
}

/// Desugar a `nix`-shebang script into a seed and deploy that.
///
/// Purely client-side: what leaves here is a seed image carrying a `flake.nix`,
/// a `flake.lock`, and the script, and from that point on this is the packed
/// path exactly — same bounds, same push, same pod. The node is never told a
/// shebang existed, and the generated seed can be committed and hand-edited
/// afterwards, which is the intended way out of this mode rather than a
/// loophole in it.
fn run_script(options: &RunOptions, script: &Path) -> ExitCode {
    if options.include_vcs {
        return fail(
            "--include-vcs applies to a packed directory; a script's seed is generated, not \
             walked"
                .to_string(),
        );
    }
    if options.output.is_some() {
        return fail(
            "--output selects a flake output; a generated flake has exactly one, `rootfs` — \
             to build something else, commit the generated seed and edit it"
                .to_string(),
        );
    }
    let Some(file_name) = script.file_name().and_then(|name| name.to_str()) else {
        return fail(format!(
            "{}: file name is not valid UTF-8",
            script.display()
        ));
    };
    // Charged against the stat size before the read, the way `pack::read_regular`
    // charges a seed directory's files. Reading first and letting
    // `pack_generated` refuse afterwards meant the identical bytes that a seed
    // directory rejects in 0.00s at 3 MB of RSS instead pulled the whole file
    // into memory twice — once here, once in `without_shebang` — so under a
    // memory limit the client was SIGKILLed with no diagnostic at all. It also
    // let `--emit-seed`, which returns before the packer, write out a seed of
    // any size with no complaint.
    match std::fs::metadata(script) {
        Ok(metadata) if metadata.len() > imageless::MAX_STAGED_SOURCE_BYTES => {
            return fail(format!(
                "{}: the script exceeds the node's staging bound of {} bytes",
                script.display(),
                imageless::MAX_STAGED_SOURCE_BYTES
            ))
        }
        Ok(_) => {}
        Err(error) => return fail(format!("{}: {error}", script.display())),
    }
    let contents = match std::fs::read(script) {
        Ok(contents) => contents,
        Err(error) => return fail(format!("{}: {error}", script.display())),
    };
    let Ok(text) = std::str::from_utf8(&contents) else {
        return fail(format!(
            "{}: not UTF-8 — a seed directory may carry arbitrary bytes, but a shebang has to \
             be read to be desugared",
            script.display()
        ));
    };
    if !shebang::is_nix_shebang(text) {
        return fail(format!(
            "{}: a packed source is either a directory containing flake.nix or a script whose \
             first line is a nix shebang (#!/usr/bin/env nix); this is a file that is neither",
            script.display()
        ));
    }
    let parsed = match shebang::parse(text) {
        Ok(parsed) => parsed,
        Err(error) => return fail(format!("{}: {error}", script.display())),
    };
    for warning in &parsed.warnings {
        eprintln!("warning: {warning}");
    }
    let system = match generate::nix_system(&options.arch) {
        Ok(system) => system,
        Err(error) => return fail(error),
    };

    let vendored = generate::vendored_pin();
    let pin =
        if options.unpinned {
            eprintln!(
                "warning: --unpinned: the generated flake tracks the nixos-unstable branch and\n\
             ships no flake.lock, so two runs of this script can materialize different\n\
             closures — and nothing recorded here says which one a given pod got"
            );
            None
        } else {
            match vendored.as_ref() {
                Some(pin) => Some(pin),
                // A binary built outside the flake never learned a narHash, and the
                // client has no Nix with which to compute one. Emitting a lock with
                // a guessed hash would fail on the node with a hash mismatch; a
                // lock with no hash at all is not a pin. Say which build this is.
                None => return fail(
                    "this build of kubectl-imageless carries no vendored nixpkgs, so it cannot \
                     write a flake.lock (it was built by cargo rather than by the flake, which \
                     bakes the rev and narHash in). Use a flake-built plugin, pass --unpinned \
                     to accept an unlocked seed, or write the seed directory by hand"
                        .to_string(),
                ),
            }
        };

    let seed = match generate::seed(file_name, text, &parsed, system, pin) {
        Ok(seed) => seed,
        Err(error) => return fail(error),
    };
    if let Some(directory) = &options.emit_seed {
        return match write_seed(Path::new(directory), &seed) {
            Ok(()) => {
                eprintln!(
                    "wrote the generated seed to {directory}; `run {directory} --repo …` from \
                     here on, and the shebang stops mattering"
                );
                ExitCode::SUCCESS
            }
            Err(error) => fail(error),
        };
    }
    let packed = match pack::pack_generated(&seed.files) {
        Ok(packed) => packed,
        Err(error) => return fail(error),
    };
    eprintln!(
        "generated a seed from {}: nixpkgs#{} in the rootfs, {} runs it",
        script.display(),
        parsed.packages.join(", nixpkgs#"),
        seed.command.join(" ")
    );
    eprintln!(
        "packed {} entries, {} bytes (node bounds: {} entries, {} bytes)",
        packed.entries,
        packed.bytes,
        imageless::MAX_STAGED_SOURCE_ENTRIES,
        imageless::MAX_STAGED_SOURCE_BYTES
    );
    let image = oci::assemble(packed.tar, &options.arch);
    report_digests(&image);
    let repo = options
        .repo
        .as_deref()
        .expect("packed mode requires --repo");
    if !options.dry_run {
        if let Err(error) = push(options, repo, &image) {
            return fail(error);
        }
    }

    // The pod's command is the generated one; anything after `--` becomes an
    // argument to the script, which is where a reader would expect it.
    let mut command = seed.command;
    command.extend(options.command.iter().cloned());
    let name = options.name.clone().unwrap_or_else(|| {
        podspec::sanitize_name(&script.file_stem().unwrap_or_default().to_string_lossy())
    });
    emit(&podspec::PodSpec {
        name: &name,
        namespace: options.namespace.as_deref(),
        image: &format!("{repo}@{}", image.manifest_digest),
        runtime_class: &options.runtime_class,
        deploy: podspec::Deploy::Source(podspec::EMBEDDED_SOURCE),
        output: None,
        command: &command,
        writable: &options.writable,
    })
}

/// Write a generated seed out as a directory. Refuses to overwrite: the point
/// is to hand a tree over to be edited and committed, and silently replacing
/// one that has already been edited is the one way this could lose work.
fn write_seed(directory: &Path, seed: &generate::GeneratedSeed) -> Result<(), String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    // `symlink_metadata`, not `exists`: a dangling symlink "does not exist",
    // and writing through it would land outside DIR.
    for file in &seed.files {
        let path = directory.join(&file.name);
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(format!("{}: already exists", path.display()));
        }
    }
    for file in &seed.files {
        let path = directory.join(&file.name);
        // O_EXCL refuses any existing entry, a symlink included, so a path
        // planted after the check above still is not written through.
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(file.mode)
            .open(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        std::io::Write::write_all(&mut output, &file.data)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        // The mode given to open() is filtered by the umask; the seed's modes
        // are part of what it hands over.
        output
            .set_permissions(std::fs::Permissions::from_mode(file.mode))
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(())
}

/// Nothing is packed here: the pod names the reference, and the node decides
/// whether it may evaluate it. Everything this mode can check has already been
/// checked by the parser, so the work left is choosing the pull target and
/// saying plainly what the node still has to be configured to do.
fn run_external(options: &RunOptions, reference: &str) -> ExitCode {
    eprintln!(
        "warning: --external moves trust from the image to the node: nothing is packed, so what\n\
         runs is whatever the node evaluates from `{reference}`, fetched with the node's network\n\
         and credentials. Inputs the referenced flake leaves unlocked resolve at evaluation time."
    );
    eprintln!(
        "notice: the node must have evaluation enabled (cache_only: false) and an\n\
         eval_allowed_uri_prefixes entry matching this reference — ask the node operator for\n\
         `{}`, which is matched as a literal byte prefix and authorizes every reference\n\
         beginning with those bytes.",
        flakeref::policy_prefix(reference)
    );
    // With a placeholder a dropped annotation fails the create loudly; with a
    // borrowed image the pod silently runs that image instead. The operator
    // must know which of the two they bought.
    let tail = match &options.image {
        Some(image) => format!("the pod silently runs `{image}` instead"),
        None => "the container fails to create with the placeholder's own diagnosis".to_string(),
    };
    eprintln!(
        "notice: the containerd runtime handler must also pass run.imageless.* through\n\
         (pod_annotations and container_annotations); a handler configured only for\n\
         imageless.run/* drops the annotation and {tail}. `kubectl imageless doctor` reports\n\
         whether this cluster is prepared."
    );
    if options.unpinned {
        eprintln!("warning: {}", flakeref::unpinned_diagnostic(reference));
    }

    let pod_image = match (&options.image, &options.repo) {
        (Some(image), _) => {
            eprintln!(
                "notice: --image `{image}` is used verbatim; nothing here checks the node can pull\n\
                 it, and its config's Env, User and WorkingDir still reach the container."
            );
            if !image.contains("@sha256:") {
                eprintln!(
                    "notice: --image `{image}` is not digest-pinned; the pod's only pinned\n\
                     identity is the flake reference."
                );
            }
            image.clone()
        }
        (None, Some(repo)) => {
            let image = oci::assemble(placeholder::layer(), &options.arch);
            report_digests(&image);
            eprintln!(
                "notice: the image is a placeholder — Kubernetes requires one and the node replaces\n\
                 the root filesystem. It carries no process metadata, only a flake that fails the\n\
                 container create with a diagnosis if run.imageless.source never reaches the runtime."
            );
            if !options.dry_run {
                if let Err(error) = push(options, repo, &image) {
                    return fail(error);
                }
            }
            format!("{repo}@{}", image.manifest_digest)
        }
        // The parser refuses this combination; reaching it is a bug here.
        (None, None) => return fail("--external needs --repo or --image".to_string()),
    };

    let name = options
        .name
        .clone()
        .unwrap_or_else(|| flakeref::derive_name(reference));
    emit(&podspec::PodSpec {
        name: &name,
        namespace: options.namespace.as_deref(),
        image: &pod_image,
        runtime_class: &options.runtime_class,
        deploy: podspec::Deploy::Source(reference),
        output: options.output.as_deref(),
        command: &options.command,
        writable: &options.writable,
    })
}

/// stdout is exactly one JSON document, so it pipes into `kubectl apply -f -`.
fn emit(spec: &podspec::PodSpec) -> ExitCode {
    match podspec::pod(spec) {
        Ok(pod) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&pod).expect("pod manifest serializes")
            );
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

fn report_digests(image: &oci::SeedImage) {
    eprintln!(
        "layer    {} ({} bytes)",
        image.layer_digest,
        image.layer.len()
    );
    eprintln!(
        "config   {} ({} bytes)",
        image.config_digest,
        image.config.len()
    );
    eprintln!(
        "manifest {} ({} bytes)",
        image.manifest_digest,
        image.manifest.len()
    );
}

/// A reference typed without `--external` fails as a missing directory, which
/// is true but unhelpful. "Looks like a reference" is defined as "the node
/// would accept it as one", so the hint cannot point at something `--external`
/// would then refuse.
fn packing_failure_hint(source: &Path) -> Option<String> {
    (!source.exists() && flakeref::looks_like_reference(&source.to_string_lossy())).then(|| {
        "hint: that looks like a flake reference — pass --external to deploy it as one\n\
         (the node evaluates it under node policy; nothing is packed)"
            .to_string()
    })
}

/// Blobs before the manifest — the spec lets a registry reject a manifest
/// whose descriptors it cannot resolve. Every byte buffer uploads verbatim:
/// the digests were computed over exactly these bytes, never a re-encoding.
fn push(options: &RunOptions, repo: &str, image: &oci::SeedImage) -> Result<(), String> {
    let mut registry = registry::Registry::connect(repo, options.plain_http)?;
    registry.ensure_blob(&image.layer_digest, &image.layer)?;
    registry.ensure_blob(&image.config_digest, &image.config)?;
    registry.put_manifest(
        &image.manifest_digest,
        oci::MANIFEST_MEDIA_TYPE,
        &image.manifest,
    )?;
    if let Some(tag) = &options.tag {
        // A second PUT of the identical bytes: the only portable way to keep
        // registries that garbage-collect untagged manifests from reaping
        // the seed. The pod reference stays digest-pinned regardless.
        registry.put_manifest(tag, oci::MANIFEST_MEDIA_TYPE, &image.manifest)?;
        eprintln!(
            "pushed {repo}@{} (also tagged {repo}:{tag})",
            image.manifest_digest
        );
        return Ok(());
    }
    eprintln!("pushed {repo}@{}", image.manifest_digest);
    Ok(())
}

fn fail(message: String) -> ExitCode {
    eprintln!("kubectl-imageless: {message}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    fn parse(words: &[&str]) -> Result<RunOptions, String> {
        match parse_run(&arguments(words))? {
            ParsedRun::Run(options) => Ok(*options),
            ParsedRun::Help => panic!("unexpected help request"),
        }
    }

    fn directory(options: &RunOptions) -> &Path {
        match &options.source {
            Source::Local(directory) => directory,
            other => panic!("expected a directory, got {other:?}"),
        }
    }

    fn external(options: &RunOptions) -> &str {
        match &options.source {
            Source::External(reference) => reference,
            other => panic!("expected a flake reference, got {other:?}"),
        }
    }

    fn release(options: &RunOptions) -> &str {
        match &options.source {
            Source::Release(coordinate) => coordinate,
            other => panic!("expected a release coordinate, got {other:?}"),
        }
    }

    #[test]
    fn a_release_coordinate_is_kept_unresolved_until_the_catalog_is_read() {
        // The parser validates the shape but never touches the network, so a
        // typo fails before anything is pushed and `--dry-run` stays offline
        // right up to the point where a catalog is genuinely needed.
        let options = parse(&[
            "--release",
            "example/agent:edge",
            "--catalog",
            "/srv/catalog",
            "--image",
            "localhost/placeholder:v1",
            "--",
            "/bin/agent",
        ])
        .unwrap();
        assert_eq!(release(&options), "example/agent:edge");
        assert_eq!(options.catalog.as_deref(), Some("/srv/catalog"));
    }

    #[test]
    fn a_catalog_without_release_is_refused_rather_than_ignored() {
        for arguments in [
            vec![
                "./app",
                "--repo",
                "r.example/t/a",
                "--catalog",
                "/srv/catalog",
            ],
            vec![
                "--external",
                "github:o/r/0123456789abcdef0123456789abcdef01234567",
                "--repo",
                "r.example/t/a",
                "--catalog",
                "/srv/catalog",
            ],
        ] {
            let mut words = arguments.clone();
            words.extend(["--", "/bin/true"]);
            let error = parse(&words).unwrap_err();
            assert!(
                error.contains("--catalog resolves a --release channel"),
                "{error}"
            );
        }
    }

    #[test]
    fn run_parses_the_full_flag_set() {
        let options = parse(&[
            "./app",
            "--repo",
            "registry.example/team/app",
            "--name",
            "demo",
            "--namespace",
            "staging",
            "--runtime-class",
            "imageless-arm",
            "--output",
            "server",
            "--arch",
            "arm64",
            "--include-vcs",
            "--tag",
            "v1",
            "--plain-http",
            "--dry-run",
            "--",
            "/bin/server",
            "--port=8080",
        ])
        .unwrap();
        assert_eq!(directory(&options), Path::new("./app"));
        assert_eq!(options.repo.as_deref(), Some("registry.example/team/app"));
        assert_eq!(options.name.as_deref(), Some("demo"));
        assert_eq!(options.namespace.as_deref(), Some("staging"));
        assert_eq!(options.runtime_class, "imageless-arm");
        assert_eq!(options.output.as_deref(), Some("server"));
        assert_eq!(options.arch, "arm64");
        assert!(options.include_vcs);
        assert_eq!(options.tag.as_deref(), Some("v1"));
        assert!(options.plain_http);
        assert!(options.dry_run);
        assert_eq!(options.command, arguments(&["/bin/server", "--port=8080"]));
    }

    #[test]
    fn runtime_class_and_arch_default_when_not_given() {
        let options = parse(&["./app", "--repo", "r.example/app", "--", "/bin/true"]).unwrap();
        assert_eq!(options.runtime_class, "imageless");
        assert_eq!(options.arch, "amd64");
        assert_eq!(options.tag, None);
        assert!(!options.plain_http);
    }

    #[test]
    fn run_requires_repo_command_and_source() {
        let missing_repo = parse(&["./app", "--", "/bin/true"]).unwrap_err();
        assert!(missing_repo.contains("--repo"), "{missing_repo}");
        // A missing command is a packed-family error only once the source has
        // been stat'd — a shebang script supplies its own — so the parser
        // enforces it for the reference modes and `run_packed` for the rest.
        let pinned = format!("github:o/r/{REV}");
        let missing_command = parse(&["--external", &pinned, "--image", "x"]).unwrap_err();
        assert!(missing_command.contains("command"), "{missing_command}");
        let missing_source = parse(&["--repo", "r.example/app", "--", "/bin/true"]).unwrap_err();
        assert!(
            missing_source.contains("source directory"),
            "{missing_source}"
        );
    }

    const REV: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn external_mode_is_chosen_by_the_flag_never_by_the_argument() {
        // A reference typed without the flag is a directory that does not
        // exist — not a silent switch to node-evaluated deployment.
        let packed = parse(&["github:o/r", "--repo", "r.example/a", "--", "/x"]).unwrap();
        assert_eq!(directory(&packed), Path::new("github:o/r"));

        let reference = format!("github:o/r/{REV}");
        let options = parse(&[
            "--external",
            &reference,
            "--repo",
            "r.example/a",
            "--",
            "/x",
        ])
        .unwrap();
        assert_eq!(external(&options), reference);
    }

    #[test]
    fn an_unpinned_reference_needs_the_opt_out() {
        let error = parse(&[
            "--external",
            "github:o/r",
            "--repo",
            "r.example/a",
            "--",
            "/x",
        ])
        .unwrap_err();
        assert!(error.contains("is not pinned"), "{error}");
        assert!(error.contains("--unpinned"), "{error}");
        let options = parse(&[
            "--external",
            "github:o/r",
            "--repo",
            "r.example/a",
            "--unpinned",
            "--",
            "/x",
        ])
        .unwrap();
        assert!(options.unpinned);
    }

    #[test]
    fn external_needs_exactly_one_image_source() {
        let neither = parse(&["--external", &format!("github:o/r/{REV}"), "--", "/x"]).unwrap_err();
        assert!(neither.contains("--repo HOST/REPO to push"), "{neither}");
        let both = parse(&[
            "--external",
            &format!("github:o/r/{REV}"),
            "--repo",
            "r.example/a",
            "--image",
            "pause:3.10",
            "--",
            "/x",
        ])
        .unwrap_err();
        assert!(both.contains("not both"), "{both}");
    }

    #[test]
    fn image_mode_validates_no_repository_and_pushes_nothing() {
        let options = parse(&[
            "--external",
            &format!("github:o/r/{REV}"),
            "--image",
            "registry.k8s.io/pause:3.10",
            "--",
            "/x",
        ])
        .unwrap();
        assert_eq!(options.repo, None);
        assert_eq!(options.image.as_deref(), Some("registry.k8s.io/pause:3.10"));
    }

    #[test]
    fn mode_specific_flags_are_refused_rather_than_ignored() {
        let pinned = format!("github:o/r/{REV}");
        for (words, expected) in [
            (
                vec!["./app", "--repo", "r.example/a", "--image", "x", "--", "/x"],
                "--image applies to --external",
            ),
            // `--unpinned` is deliberately absent from this list: in the
            // packed family it is meaningful for a shebang script and
            // meaningless for a directory, and telling those apart is a stat
            // the parser does not do. `run_packed` refuses it for a directory.
            (
                vec![
                    "--external",
                    &pinned,
                    "--repo",
                    "r.example/a",
                    "--include-vcs",
                    "--",
                    "/x",
                ],
                "--include-vcs applies to a packed directory",
            ),
            (
                vec![
                    "--external",
                    &pinned,
                    "--image",
                    "x",
                    "--tag",
                    "v1",
                    "--",
                    "/x",
                ],
                "--tag applies to the manifest this command pushes",
            ),
            (
                vec![
                    "--external",
                    &pinned,
                    "--image",
                    "x",
                    "--plain-http",
                    "--",
                    "/x",
                ],
                "--plain-http applies to the manifest this command pushes",
            ),
        ] {
            let error = parse(&words).unwrap_err();
            assert!(error.contains(expected), "{words:?}: {error}");
        }
    }

    #[test]
    fn external_reports_the_missing_positional_in_its_own_words() {
        let error = parse(&["--external", "--repo", "r.example/a", "--", "/x"]).unwrap_err();
        assert!(
            error.contains("external flake reference is required"),
            "{error}"
        );
    }

    #[test]
    fn repo_must_be_a_bare_host_repo_reference() {
        for repo in [
            "https://r.example/app",
            "r.example/app@sha256:00",
            "bare-name-without-a-host",
            "",
        ] {
            let error = parse(&["./app", "--repo", repo, "--", "/bin/true"]).unwrap_err();
            assert!(error.contains("--repo"), "{error}");
        }
    }

    #[test]
    fn repo_rejects_a_tag_in_the_path_portion() {
        let error = parse(&["./app", "--repo", "r.example/app:v1", "--", "/bin/true"]).unwrap_err();
        assert!(error.contains("no scheme, tag, or digest"), "{error}");
    }

    #[test]
    fn repo_keeps_a_port_on_the_host() {
        let options = parse(&["./app", "--repo", "localhost:5001/app", "--", "/bin/true"]).unwrap();
        assert_eq!(options.repo.as_deref(), Some("localhost:5001/app"));
    }

    #[test]
    fn tags_are_validated_against_the_oci_grammar() {
        for tag in ["v1", "latest", "2026-07-29_a.b", "_hidden"] {
            assert!(
                parse(&["./app", "--repo", "r.example/app", "--tag", tag, "--", "/x"]).is_ok(),
                "{tag}"
            );
        }
        for tag in [
            ".dot",
            "has space",
            "has/slash",
            "v1:2",
            "x".repeat(129).as_str(),
        ] {
            let error =
                parse(&["./app", "--repo", "r.example/app", "--tag", tag, "--", "/x"]).unwrap_err();
            assert!(error.contains("--tag"), "{tag}: {error}");
        }
    }

    #[test]
    fn flags_reject_missing_values_and_unknown_names() {
        let dangling = parse(&["./app", "--repo"]).unwrap_err();
        assert!(dangling.contains("requires a value"), "{dangling}");
        let unknown = parse(&["./app", "--frobnicate"]).unwrap_err();
        assert!(unknown.contains("--frobnicate"), "{unknown}");
    }

    #[test]
    fn a_flag_is_never_swallowed_as_a_value() {
        let error = parse(&["./app", "--repo", "--dry-run", "--", "/bin/true"]).unwrap_err();
        assert!(error.contains("--repo requires a value"), "{error}");
    }

    #[test]
    fn run_help_is_help_not_a_usage_error() {
        assert!(matches!(
            parse_run(&arguments(&["--help"])).unwrap(),
            ParsedRun::Help
        ));
        assert!(matches!(
            parse_run(&arguments(&["./app", "-h"])).unwrap(),
            ParsedRun::Help
        ));
    }
}
