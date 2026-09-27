//! Minimal privilege-dropping launcher for development-only flake evaluation.
//!
//! The root resolver validates policy and invokes this helper. This process
//! drops every group and user privilege, applies hard resource limits, clears
//! its environment, and only then replaces itself with the configured Nix
//! client. It never receives a bundle path and cannot register GC roots.
//!
//! With `--confine-root`, it first enters the evaluation confinement (while
//! still root — building a mount namespace needs the privilege this process is
//! about to drop), so no flake input can read the node's filesystem.

use std::ffi::CString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_INSTALLABLE_BYTES: usize = 4608;
const MAX_MEMORY_BYTES: libc::rlim_t = 2 * 1024 * 1024 * 1024;
const MAX_OPEN_FILES: libc::rlim_t = 256;
// RLIMIT_NPROC counts threads across the worker UID, not just this process
// tree. Development nodes may authorize an existing operator account, so keep
// a finite ceiling with enough headroom for that account and the Nix client.
// Hardened deployments should use a dedicated worker UID and service cgroup.
const MAX_PROCESSES: libc::rlim_t = 1024;

fn usage() -> ! {
    eprintln!(
        "usage: imageless-dev-resolver --user USER --nix ABSOLUTE_PATH \
         --cpu-seconds 1..3600 [--cache-home ABSOLUTE_PATH] [--confine-root ABSOLUTE_PATH] \
         [--locked] [--out-link ABSOLUTE_PATH] --installable FLAKE#OUTPUT"
    );
    std::process::exit(2);
}

fn value(args: &mut impl Iterator<Item = String>) -> String {
    args.next().unwrap_or_else(|| usage())
}

fn main() {
    let mut user = None;
    let mut nix = None;
    let mut cpu_seconds = None;
    let mut cache_home = None;
    let mut installable = None;
    let mut confine_root: Option<PathBuf> = None;
    let mut locked = false;
    let mut out_link: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--user" => user = Some(value(&mut args)),
            "--nix" => nix = Some(PathBuf::from(value(&mut args))),
            "--cpu-seconds" => {
                cpu_seconds = Some(value(&mut args).parse::<u64>().unwrap_or_else(|_| usage()))
            }
            "--cache-home" => cache_home = Some(PathBuf::from(value(&mut args))),
            "--installable" => installable = Some(value(&mut args)),
            "--confine-root" => confine_root = Some(PathBuf::from(value(&mut args))),
            "--locked" => locked = true,
            "--out-link" => out_link = Some(PathBuf::from(value(&mut args))),
            "--help" | "-h" => usage(),
            _ => usage(),
        }
    }
    let user = user.unwrap_or_else(|| usage());
    let nix = nix.unwrap_or_else(|| usage());
    let cpu_seconds = cpu_seconds
        .filter(|seconds| (1..=3600).contains(seconds))
        .unwrap_or_else(|| usage());
    let installable = installable
        .filter(|value| valid_installable(value))
        .unwrap_or_else(|| usage());
    if !nix.is_absolute() || user.is_empty() || user.as_bytes().contains(&0) {
        usage();
    }
    if [&cache_home, &confine_root, &out_link]
        .into_iter()
        .flatten()
        .any(|path| !path.is_absolute())
    {
        usage();
    }

    let (uid, gid) = lookup_user(&user).unwrap_or_else(|error| fail("resolve worker user", error));
    apply_limit(libc::RLIMIT_AS, MAX_MEMORY_BYTES, MAX_MEMORY_BYTES)
        .unwrap_or_else(|error| fail("limit address space", error));
    apply_limit(
        libc::RLIMIT_CPU,
        cpu_seconds as libc::rlim_t,
        cpu_seconds as libc::rlim_t,
    )
    .unwrap_or_else(|error| fail("limit CPU time", error));
    apply_limit(libc::RLIMIT_NOFILE, MAX_OPEN_FILES, MAX_OPEN_FILES)
        .unwrap_or_else(|error| fail("limit open files", error));
    apply_limit(libc::RLIMIT_NPROC, MAX_PROCESSES, MAX_PROCESSES)
        .unwrap_or_else(|error| fail("limit processes", error));
    if let Some(root) = &confine_root {
        // The staged source is the one node path an in-image evaluation needs;
        // the cache is the worker's scratch. Both keep their paths inside.
        let mut writable: Vec<PathBuf> = installable
            .strip_prefix("path:")
            .and_then(|value| value.rsplit_once('#'))
            .map(|(source, _)| PathBuf::from(source))
            .into_iter()
            .collect();
        writable.extend(cache_home.clone());
        let certificates: Vec<PathBuf> = option_env!("IMAGELESS_DEV_SSL_CERT_FILE")
            .map(PathBuf::from)
            .into_iter()
            .collect();
        imageless::Confinement::for_evaluation(root, &nix, &writable, &certificates)
            .and_then(|confinement| confinement.enter())
            .unwrap_or_else(|error| fail("confine evaluation", error));
    }
    drop_privileges(uid, gid).unwrap_or_else(|error| fail("drop privileges", error));

    let mut command = Command::new(nix);
    command
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "build",
            "--print-out-paths",
        ])
        // With --out-link the build registers an indirect GC root the moment
        // it finishes, closing the window before the resolver's own root.
        .args(match &out_link {
            Some(link) => vec![std::ffi::OsString::from("--out-link"), link.into()],
            None => vec![std::ffi::OsString::from("--no-link")],
        })
        .args(locked.then_some("--no-update-lock-file"))
        .arg(installable)
        .env_clear()
        .env("HOME", "/var/empty")
        // Routing through the daemon is what lets this run unprivileged, and it
        // is also why `--option require-sigs` does NOT belong on this command
        // the way it does on the release realises in `imageless::resolver`: Nix
        // binds require-sigs to the store when the store is opened, so a client
        // sending it to a daemon has it parsed and dropped. Adding it here
        // would read as enforcement while enforcing nothing.
        .env("NIX_REMOTE", "daemon")
        .env(
            "PATH",
            option_env!("IMAGELESS_DEV_PATH").unwrap_or("/usr/bin:/bin"),
        );
    // HOME stays unwritable on purpose; the resolver hands the worker a
    // dedicated cache directory instead, because flake evaluation refuses to
    // run without a writable fetcher cache.
    if let Some(cache_home) = &cache_home {
        command.env("XDG_CACHE_HOME", cache_home);
    }
    if let Some(certificates) = option_env!("IMAGELESS_DEV_SSL_CERT_FILE") {
        command.env("SSL_CERT_FILE", certificates);
        command.env("NIX_SSL_CERT_FILE", certificates);
    }
    if Path::new("/var/empty").is_dir() {
        command.current_dir("/var/empty");
    } else {
        command.current_dir("/");
    }
    let error = command.exec();
    fail("execute Nix", error)
}

fn valid_installable(value: &str) -> bool {
    if value.is_empty()
        || value.len() > MAX_INSTALLABLE_BYTES
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return false;
    }
    let Some((flake, output)) = value.rsplit_once('#') else {
        return false;
    };
    !flake.is_empty()
        && !flake.contains('#')
        && !output.is_empty()
        && output.len() <= 256
        && output
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'_' | b'.'))
}

fn lookup_user(name: &str) -> std::io::Result<(libc::uid_t, libc::gid_t)> {
    let name = CString::new(name)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid user"))?;
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    if entry.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "worker user does not exist",
        ));
    }
    let entry = unsafe { *entry };
    Ok((entry.pw_uid, entry.pw_gid))
}

fn apply_limit(
    resource: libc::__rlimit_resource_t,
    soft: libc::rlim_t,
    hard: libc::rlim_t,
) -> std::io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    if unsafe { libc::setrlimit(resource, &limit) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn drop_privileges(uid: libc::uid_t, gid: libc::gid_t) -> std::io::Result<()> {
    let current_uid = unsafe { libc::geteuid() };
    if current_uid == 0 {
        if unsafe { libc::setgroups(0, std::ptr::null()) } == -1
            || unsafe { libc::setgid(gid) } == -1
            || unsafe { libc::setuid(uid) } == -1
        {
            return Err(std::io::Error::last_os_error());
        }
    } else if current_uid != uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "worker is not root or the configured user",
        ));
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == -1
        || unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn fail(stage: &str, error: impl std::fmt::Display) -> ! {
    eprintln!("imageless-dev-resolver: {stage}: {error}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::valid_installable;

    #[test]
    fn installable_validation_is_fail_closed() {
        assert!(valid_installable("path:/source#rootfs"));
        assert!(valid_installable(
            "https://example.invalid/repo#packages.x86_64-linux.rootfs"
        ));
        assert!(!valid_installable("path:/source"));
        assert!(!valid_installable("path:/source#"));
        assert!(!valid_installable("path:/source#root fs"));
        assert!(!valid_installable("path:/source#one#two"));
    }
}
