//! runc-compatible imageless interposer for Docker and other OCI callers.

use imageless::{export_timing_events, prepare_bundle, remove_bundle_gc_roots, PrepareBundle};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

const DELEGATE_ENV: &str = "IMAGELESS_RUNC";
const DELEGATE_BAKED: &str = match option_env!("IMAGELESS_RUNC") {
    Some(path) => path,
    None => "runc",
};
const TIMEOUT_ENV: &str = "IMAGELESS_REALIZATION_TIMEOUT_SECONDS";
const TIMEOUT_BAKED: &str = match option_env!("IMAGELESS_REALIZATION_TIMEOUT_SECONDS") {
    Some(value) => value,
    None => "300",
};
const OUTPUT_ENV: &str = "IMAGELESS_DEFAULT_OUTPUT";
const OUTPUT_BAKED: &str = match option_env!("IMAGELESS_DEFAULT_OUTPUT") {
    Some(value) => value,
    None => "rootfs",
};
const TELEMETRY_ENV: &str = "IMAGELESS_TELEMETRY_PATH";

fn nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn delegate() -> String {
    nonempty(DELEGATE_ENV).unwrap_or_else(|| DELEGATE_BAKED.to_string())
}

fn timeout_seconds() -> Result<u64, String> {
    let raw = nonempty(TIMEOUT_ENV).unwrap_or_else(|| TIMEOUT_BAKED.to_string());
    let seconds = raw
        .parse::<u64>()
        .map_err(|_| format!("{TIMEOUT_ENV} must be an integer number of seconds"))?;
    if !(1..=3600).contains(&seconds) {
        return Err(format!("{TIMEOUT_ENV} must be between 1 and 3600"));
    }
    Ok(seconds)
}

fn default_output() -> String {
    nonempty(OUTPUT_ENV).unwrap_or_else(|| OUTPUT_BAKED.to_string())
}

/// runc global flags that consume a following value when written as
/// `--flag value` (the `--flag=value` form carries its own value). Every other
/// pre-subcommand token starting with `-` is a boolean switch (`--debug`,
/// `--systemd-cgroup`, ...). This table exists only to walk past the global
/// prefix and land on the subcommand token. Names are bare: see [`flag`].
const GLOBAL_VALUE_FLAGS: &[&str] = &["root", "log", "log-format", "criu", "rootless"];

/// The subcommands that build a container from a bundle's config.json, and so
/// must see the materialized root: `run` is create + start, and `restore`
/// creates the container a checkpoint is restored into.
const BUNDLE_SUBCOMMANDS: &[&str] = &["create", "run", "restore"];

/// runc parses flags with urfave/cli v1 over Go's `flag` package, which takes
/// `-name` and `--name` alike, each as `name value` or `name=value`. Split a
/// token into its bare name and inline value; `None` for a non-flag or `--`.
fn flag(argument: &str) -> Option<(&str, Option<&str>)> {
    let body = argument
        .strip_prefix("--")
        .or_else(|| argument.strip_prefix('-'))
        .filter(|body| !body.is_empty() && !body.starts_with('-'))?;
    Some(match body.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (body, None),
    })
}

/// Tokens a flag occupies: its own, plus the next when it takes a value and
/// did not carry one inline.
fn flag_width(argument: &str, value_flags: &[&str]) -> usize {
    match flag(argument) {
        Some((name, None)) if value_flags.contains(&name) => 2,
        _ => 1,
    }
}

/// Index of the runc subcommand: the first argv element that is neither a
/// global flag nor the value of one. Searching argv for the literal string
/// `create` instead would mis-locate the subcommand whenever a container ID,
/// bundle path, or log path is itself named `create` — and then every later
/// token, including a real `--bundle`, is parsed against the wrong offset.
fn subcommand_index(arguments: &[String]) -> Option<usize> {
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        if argument == "--" {
            return (index + 1 < arguments.len()).then_some(index + 1);
        }
        if !argument.starts_with('-') {
            return Some(index);
        }
        index += flag_width(argument, GLOBAL_VALUE_FLAGS);
    }
    None
}

/// Value of a global flag — one that precedes the subcommand — in either
/// spelling runc accepts. The search stops at the subcommand so that a
/// `create --log-format ...` never answers for the global one, and so that a
/// container ID that happens to spell a flag is never read as one.
fn global_flag<'a>(arguments: &'a [String], wanted: &str) -> Option<&'a str> {
    let end = subcommand_index(arguments).unwrap_or(arguments.len());
    let mut index = 0;
    while index < end {
        let argument = arguments[index].as_str();
        match flag(argument) {
            Some((name, Some(value))) if name == wanted => return Some(value),
            Some((name, None)) if name == wanted => {
                return arguments.get(index + 1).map(String::as_str)
            }
            _ => {}
        }
        index += flag_width(argument, GLOBAL_VALUE_FLAGS);
    }
    None
}

/// The runtime log to relay a create failure into, if the caller established
/// one this shim can safely append to.
///
/// Both conditions are the caller's, not ours: containerd passes `--log` with
/// `--log-format json` on every invocation, while a human running `runc` by
/// hand gets runc's default text format. Writing a JSON record into a text log
/// would corrupt the operator's file to no one's benefit, so the absent format
/// flag means silence rather than a guess.
fn runtime_log(arguments: &[String]) -> Option<PathBuf> {
    if global_flag(arguments, "log-format") != Some("json") {
        return None;
    }
    global_flag(arguments, "log")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn create_bundle(arguments: &[String]) -> Result<Option<PathBuf>, String> {
    let Some(create_index) = subcommand_index(arguments) else {
        return Ok(None);
    };
    if !BUNDLE_SUBCOMMANDS.contains(&arguments[create_index].as_str()) {
        return Ok(None);
    }
    let command_arguments = &arguments[create_index + 1..];
    for (index, argument) in command_arguments.iter().enumerate() {
        if argument == "--" {
            break;
        }
        match flag(argument) {
            Some(("bundle" | "b", None)) => {
                return command_arguments
                    .get(index + 1)
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .map(Some)
                    .ok_or_else(|| format!("{argument} requires a path"));
            }
            Some(("bundle" | "b", Some(value))) => {
                if value.is_empty() {
                    return Err(format!("{argument} requires a path"));
                }
                return Ok(Some(PathBuf::from(value)));
            }
            _ => {}
        }
    }
    Ok(Some(PathBuf::from(".")))
}

/// Put back the config.json the engine wrote, so a create retried on the same
/// bundle plans against the image again instead of against our rewrite (whose
/// /nix/store mount would collide with the one the retry adds). Temp + rename
/// in the bundle directory, mode preserved: never a half-written spec.
fn restore_config(config: &Path, original: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let permissions = std::fs::metadata(config)?.permissions();
    let temporary = config.with_file_name(format!(
        ".config.json.imageless-restore-{}",
        std::process::id()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let written = file
        .write_all(original)
        .and_then(|()| file.set_permissions(permissions))
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&temporary, config));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

fn canonical_bundle(arguments: &[String]) -> Result<Option<PathBuf>, String> {
    create_bundle(arguments)?
        .map(|bundle| {
            std::fs::canonicalize(&bundle)
                .map_err(|error| format!("canonicalize bundle {}: {error}", bundle.display()))
        })
        .transpose()
}

fn run_delegate(program: &str, arguments: &[String]) -> std::io::Result<ExitStatus> {
    Command::new(program)
        .args(arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
}

fn exit_like(status: ExitStatus) -> ! {
    if let Some(code) = status.code() {
        std::process::exit(code);
    }
    std::process::exit(128 + status.signal().unwrap_or(1));
}

fn fail(message: impl std::fmt::Display, code: i32) -> ! {
    eprintln!("imageless-runc: {message}");
    std::process::exit(code);
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let bundle = canonical_bundle(&arguments).unwrap_or_else(|error| fail(error, 1));
    let mut applied = None;
    let mut original_config = None;

    if let Some(bundle) = &bundle {
        let mut prepare = PrepareBundle::new(bundle.join("config.json"), bundle);
        prepare.default_output = default_output();
        prepare.timeout_seconds = timeout_seconds().unwrap_or_else(|error| fail(error, 1));
        // Parsed here rather than inside the library: argv is this binary's
        // contract with the runtime that invoked it, and the library serves
        // embedders that have no argv at all.
        prepare.runtime_log = runtime_log(&arguments);
        // Read before preparation so a failed delegate can be undone; a bundle
        // without a readable config fails in preparation with its own error.
        original_config = std::fs::read(bundle.join("config.json")).ok();
        let prepare_started = Instant::now();
        let resolution = prepare_bundle(&prepare).unwrap_or_else(|error| {
            // The sink recorded successes only: every failure exited here,
            // before the export below. A node that never starts a pod is
            // exactly the node an operator needs a record from, and the
            // duration is the useful half — it separates a fast refusal
            // (policy, a contract error) from a create that burned its whole
            // deadline. No release identity exists yet, so the event is named
            // for the stage rather than for a release never selected.
            if let Some(path) = nonempty(TELEMETRY_ENV) {
                let _ = export_timing_events(
                    Path::new(&path),
                    "unresolved",
                    &[("preparation", elapsed_us(prepare_started))],
                    Some("error"),
                );
            }
            // "selection" named one stage of several: a substitution timeout
            // read as a selection problem. Nothing in the tree or in CI greps
            // this string.
            fail(format_args!("bundle preparation failed: {error}"), 1)
        });
        if let Some(resolution) = resolution {
            if let Some(path) = nonempty(TELEMETRY_ENV) {
                let timings = &resolution.timings;
                let _ = export_timing_events(
                    Path::new(&path),
                    &resolution.resolution.identity,
                    // The four original stages keep their names, their order,
                    // and their spans; the rest are carved out of two of them,
                    // so `selection + policy_verification + substitution +
                    // rewrite` still totals the create and the finer stages
                    // explain where two of those went rather than adding to
                    // them. Anything summing these must pick one set.
                    &[
                        ("selection", timings.selection_us),
                        ("policy_verification", timings.policy_verification_us),
                        ("substitution", timings.substitution_us),
                        ("rewrite", timings.rewrite_us),
                        ("manifest_fetch", timings.manifest_fetch_us),
                        ("staging", timings.staging_us),
                        ("evaluation", timings.evaluation_us),
                        ("root_registration", timings.root_registration_us),
                    ],
                    Some("success"),
                );
            }
            applied = Some(resolution);
        }
    }

    let delegate = delegate();
    let delegate_started = Instant::now();
    let status = match run_delegate(&delegate, &arguments) {
        Ok(status) => status,
        Err(error) => {
            if let Some(bundle) = &bundle {
                if applied.is_some() {
                    undo(bundle, original_config.as_deref());
                }
            }
            fail(format_args!("execute {delegate}: {error}"), 127)
        }
    };
    if let (Some(path), Some(resolution)) = (nonempty(TELEMETRY_ENV), &applied) {
        let _ = export_timing_events(
            Path::new(&path),
            &resolution.resolution.identity,
            &[("delegate_startup", elapsed_us(delegate_started))],
            Some(if status.success() { "success" } else { "error" }),
        );
    }
    // `run` without --detach returns when the container exits, and its status
    // is the workload's: the container is gone either way, so the undo holds.
    if !status.success() {
        if let Some(bundle) = &bundle {
            if applied.is_some() {
                undo(bundle, original_config.as_deref());
            }
        }
    }
    exit_like(status);
}

/// Reverse a preparation the delegate never turned into a container: release
/// its GC roots and restore the engine's config.json.
fn undo(bundle: &Path, original_config: Option<&[u8]>) {
    let _ = remove_bundle_gc_roots(bundle);
    if let Some(original) = original_config {
        if let Err(error) = restore_config(&bundle.join("config.json"), original) {
            eprintln!(
                "imageless-runc: could not restore config.json after a failed create: {error}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{create_bundle, runtime_log};
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    /// containerd's actual invocation, which is the only one that should turn
    /// the relay on.
    #[test]
    fn a_json_runtime_log_is_taken_in_either_spelling() {
        let expected = Some(PathBuf::from(
            "/run/containerd/io.containerd.runtime/log.json",
        ));
        assert_eq!(
            runtime_log(&args(&[
                "--root",
                "/run/runc",
                "--log",
                "/run/containerd/io.containerd.runtime/log.json",
                "--log-format",
                "json",
                "create",
                "--bundle",
                "/b",
                "id",
            ])),
            expected
        );
        assert_eq!(
            runtime_log(&args(&[
                "--log=/run/containerd/io.containerd.runtime/log.json",
                "--log-format=json",
                "create",
                "id",
            ])),
            expected
        );
    }

    /// runc's default format is text, and a JSON record written into a text log
    /// corrupts an operator's file to no one's benefit.
    #[test]
    fn a_log_without_the_json_format_is_not_relayed_into() {
        assert_eq!(
            runtime_log(&args(&["--log", "/var/log/runc.log", "create", "id"])),
            None
        );
        assert_eq!(
            runtime_log(&args(&[
                "--log",
                "/var/log/runc.log",
                "--log-format",
                "text",
                "create",
                "id"
            ])),
            None
        );
        // The flag with no path behind it is a malformed invocation, not a
        // reason to write somewhere.
        assert_eq!(runtime_log(&args(&["--log-format", "json", "--log"])), None);
        assert_eq!(
            runtime_log(&args(&[
                "--log",
                "",
                "--log-format",
                "json",
                "create",
                "id"
            ])),
            None
        );
    }

    /// The same positional reasoning `subcommand_index` exists for: a container
    /// ID or bundle path that spells a global flag belongs to the subcommand,
    /// and reading it as the runtime log would point the relay at a path the
    /// runtime never established.
    #[test]
    fn a_subcommand_argument_that_spells_a_global_flag_is_not_the_runtime_log() {
        assert_eq!(
            runtime_log(&args(&[
                "--log-format",
                "json",
                "create",
                "--bundle",
                "/b",
                "--log",
                "/attacker/owned",
            ])),
            None
        );
        // And a global flag still wins when a later subcommand argument repeats
        // it, because the search stops at the subcommand.
        assert_eq!(
            runtime_log(&args(&[
                "--log",
                "/run/real.json",
                "--log-format",
                "json",
                "create",
                "--log",
                "/decoy",
            ])),
            Some(PathBuf::from("/run/real.json"))
        );
    }

    #[test]
    fn only_create_selects_a_bundle() {
        assert_eq!(create_bundle(&args(&["start", "id"])).unwrap(), None);
        // `run` and `restore` build a container from the bundle just as
        // `create` does; passing them through would start the seed unbuilt.
        for subcommand in ["run", "restore"] {
            assert_eq!(
                create_bundle(&args(&[subcommand, "-b", "/bundle", "id"])).unwrap(),
                Some(PathBuf::from("/bundle"))
            );
        }
        assert_eq!(
            create_bundle(&args(&["--root", "/run/runc", "create", "id"])).unwrap(),
            Some(PathBuf::from("."))
        );
    }

    #[test]
    fn parses_runc_bundle_flag_forms() {
        for arguments in [
            args(&["create", "--bundle", "/bundle", "id"]),
            args(&["create", "-b", "/bundle", "id"]),
            args(&["create", "--bundle=/bundle", "id"]),
            args(&["create", "-b=/bundle", "id"]),
            // Go's flag package: one dash or two, for either name.
            args(&["create", "-bundle", "/bundle", "id"]),
            args(&["create", "--b", "/bundle", "id"]),
            args(&["create", "-bundle=/bundle", "id"]),
        ] {
            assert_eq!(
                create_bundle(&arguments).unwrap(),
                Some(PathBuf::from("/bundle"))
            );
        }
    }

    #[test]
    fn a_container_id_named_create_is_not_the_subcommand() {
        // `runc start create` starts a container whose ID is "create". The
        // shim must not treat it as a create, and must not misread the
        // remaining argv against that offset.
        assert_eq!(create_bundle(&args(&["start", "create"])).unwrap(), None);
        assert_eq!(create_bundle(&args(&["delete", "create"])).unwrap(), None);
        assert_eq!(
            create_bundle(&args(&["exec", "create", "--bundle", "/not-a-bundle"])).unwrap(),
            None
        );
        // A global flag whose value is "create" likewise names no subcommand.
        assert_eq!(
            create_bundle(&args(&["--root", "create", "state", "id"])).unwrap(),
            None
        );
        // ...and a real create with a bundle path named "create" still parses.
        assert_eq!(
            create_bundle(&args(&[
                "--root", "create", "create", "--bundle", "/b", "create"
            ]))
            .unwrap(),
            Some(PathBuf::from("/b"))
        );
    }

    #[test]
    fn walks_past_global_flag_forms() {
        for arguments in [
            args(&[
                "--debug",
                "--systemd-cgroup",
                "create",
                "-b",
                "/bundle",
                "id",
            ]),
            args(&["--log=/tmp/log", "create", "-b", "/bundle", "id"]),
            args(&[
                "--log", "/tmp/log", "--debug", "create", "-b", "/bundle", "id",
            ]),
            args(&["--rootless", "true", "create", "-b", "/bundle", "id"]),
            // Single-dash globals: `-root /r` must consume its value, or `/r`
            // reads as the subcommand and the create passes through unbuilt.
            args(&["-root", "/r", "create", "-b", "/bundle", "id"]),
            args(&[
                "-log",
                "/l",
                "-log-format",
                "json",
                "create",
                "-b",
                "/bundle",
                "id",
            ]),
        ] {
            assert_eq!(
                create_bundle(&arguments).unwrap(),
                Some(PathBuf::from("/bundle"))
            );
        }
        // Global flags with no subcommand at all.
        assert_eq!(create_bundle(&args(&["--version"])).unwrap(), None);
        assert_eq!(create_bundle(&args(&[])).unwrap(), None);
        assert_eq!(create_bundle(&args(&["--root"])).unwrap(), None);
    }

    #[test]
    fn single_dash_globals_answer_for_the_runtime_log() {
        assert_eq!(
            runtime_log(&args(&[
                "-log",
                "/run/log.json",
                "-log-format=json",
                "create",
                "id"
            ])),
            Some(PathBuf::from("/run/log.json"))
        );
    }

    #[test]
    fn a_bundle_flag_after_the_argument_terminator_is_an_argument() {
        assert_eq!(
            create_bundle(&args(&["create", "--", "-b", "/not-a-flag"])).unwrap(),
            Some(PathBuf::from("."))
        );
    }

    #[test]
    fn a_failed_create_puts_the_engine_config_back_with_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("il-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.json");
        std::fs::write(&config, "rewritten").unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o640)).unwrap();
        super::restore_config(&config, b"original").unwrap();
        assert_eq!(std::fs::read(&config).unwrap(), b"original");
        assert_eq!(
            std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o640
        );
        // Nothing but the config is left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_empty_bundle_flag() {
        assert!(create_bundle(&args(&["create", "--bundle"])).is_err());
        assert!(create_bundle(&args(&["create", "--bundle="])).is_err());
    }
}
