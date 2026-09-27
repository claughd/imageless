//! PID 1 of an imageless microVM.
//!
//! The kernel mounts the VM's closure image (erofs, read-only) as `/`: a tree
//! holding nothing but `/nix/store` with the release's closure, this binary's
//! closure, and empty mountpoints. The host passes the rest on a small config
//! drive named by `imageless.config=` on the kernel command line: the release
//! root (a store path) and its process.
//!
//! Init then assembles the workload's root the way the container runtime does
//! (SPEC §4.5): an overlay whose only lower layer is the release root and
//! whose upper layer is a tmpfs, with the image's `/nix/store` bound in
//! read-only. It pivots into that root, starts the process, and stays PID 1
//! to reap. When the process exits, init reports its status on the console
//! and reboots, which ends the VM under Firecracker and `qemu -no-reboot`.

use serde::Deserialize;
use std::ffi::{CString, OsStr};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The config drive's contents: JSON, padded with NULs to the device size.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct GuestConfig {
    /// The release root: a store path present in the closure image.
    root: String,
    /// argv, `args[0]` resolved inside the release root.
    args: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default = "default_cwd")]
    cwd: String,
    #[serde(default)]
    hostname: Option<String>,
}

fn default_cwd() -> String {
    "/".to_string()
}

const CONFIG_PARAMETER: &str = "imageless.config=";
/// Bounds the config drive read; a process spec is small.
const MAX_CONFIG_BYTES: usize = 1 << 20;

fn main() {
    if std::process::id() != 1 {
        eprintln!("imageless-vm-init: must run as PID 1 of a VM");
        std::process::exit(2);
    }
    let status = match boot() {
        Ok(status) => status,
        Err(error) => {
            console(&format!("imageless-vm-init: {error}"));
            1
        }
    };
    console(&format!(
        "imageless-vm: workload exited with status {status}"
    ));
    // SAFETY: sync and reboot take no pointers; reboot does not return.
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_AUTOBOOT);
    }
    std::process::exit(status);
}

fn boot() -> io::Result<i32> {
    for (source, target, fstype) in [
        ("proc", "/proc", "proc"),
        ("sysfs", "/sys", "sysfs"),
        ("devtmpfs", "/dev", "devtmpfs"),
        ("tmpfs", "/run", "tmpfs"),
    ] {
        mount(Some(source), target, Some(fstype), 0, None)?;
    }
    let cmdline = std::fs::read_to_string("/proc/cmdline")?;
    let device = config_device(&cmdline)
        .ok_or_else(|| io::Error::other("no imageless.config= on the kernel command line"))?;
    let config = read_config(Path::new(device))?;
    if let Some(hostname) = &config.hostname {
        // SAFETY: a valid buffer and its length.
        unsafe { libc::sethostname(hostname.as_ptr().cast(), hostname.len()) };
    }

    // The workload's root: the release root under a tmpfs upper layer, so
    // mountpoints and scratch writes never touch the image (SPEC §4.5).
    for directory in ["/run/upper", "/run/work", "/run/root"] {
        std::fs::create_dir(directory)?;
    }
    mount(
        Some("overlay"),
        "/run/root",
        Some("overlay"),
        0,
        Some(&format!(
            "lowerdir={},upperdir=/run/upper,workdir=/run/work",
            config.root
        )),
    )?;
    for directory in ["nix/store", "proc", "sys", "dev", "tmp"] {
        std::fs::create_dir_all(Path::new("/run/root").join(directory))?;
    }
    mount(
        Some("/nix/store"),
        "/run/root/nix/store",
        None,
        libc::MS_BIND | libc::MS_REC,
        None,
    )?;
    mount(
        None,
        "/run/root/nix/store",
        None,
        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
        None,
    )?;
    for (target, source) in [
        ("/run/root/proc", "/proc"),
        ("/run/root/sys", "/sys"),
        ("/run/root/dev", "/dev"),
    ] {
        mount(Some(source), target, None, libc::MS_MOVE, None)?;
    }
    mount(
        Some("tmpfs"),
        "/run/root/tmp",
        Some("tmpfs"),
        0,
        Some("mode=1777"),
    )?;
    std::fs::create_dir_all("/run/root/dev/pts")?;
    mount(
        Some("devpts"),
        "/run/root/dev/pts",
        Some("devpts"),
        0,
        Some("newinstance,ptmxmode=0666"),
    )?;

    std::env::set_current_dir("/run/root")?;
    pivot_into_current_directory()?;
    std::env::set_current_dir(&config.cwd)?;

    let program = config
        .args
        .first()
        .ok_or_else(|| io::Error::other("the process has no argv"))?
        .clone();
    console(&format!("imageless-vm: starting {program}"));
    let child = std::process::Command::new(&program)
        .args(&config.args[1..])
        .env_clear()
        .envs(config.env.iter().filter_map(|entry| entry.split_once('=')))
        .spawn()?;
    Ok(reap_until(child.id() as libc::pid_t))
}

/// `pivot_root(".", ".")`, then detach the old root stacked beneath: the
/// new root needs no directory to hold the old one.
fn pivot_into_current_directory() -> io::Result<()> {
    let dot = CString::new(".").unwrap();
    // SAFETY: valid paths; the syscall has no libc wrapper.
    if unsafe { libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a valid path.
    if unsafe { libc::umount2(dot.as_ptr(), libc::MNT_DETACH) } != 0 {
        return Err(io::Error::last_os_error());
    }
    std::env::set_current_dir("/")
}

/// Reaps every child, as PID 1 must, until `workload` exits; returns its
/// status in shell convention.
fn reap_until(workload: libc::pid_t) -> i32 {
    loop {
        let mut status = 0;
        // SAFETY: a valid out-pointer.
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid == -1 {
            return 1;
        }
        if pid == workload {
            return if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                128 + libc::WTERMSIG(status)
            };
        }
    }
}

fn config_device(cmdline: &str) -> Option<&str> {
    cmdline
        .split_ascii_whitespace()
        .find_map(|parameter| parameter.strip_prefix(CONFIG_PARAMETER))
        .filter(|device| !device.is_empty())
}

fn read_config(device: &Path) -> io::Result<GuestConfig> {
    let mut bytes = Vec::new();
    std::fs::File::open(device)?
        .take(MAX_CONFIG_BYTES as u64)
        .read_to_end(&mut bytes)?;
    parse_config(&bytes)
}

fn parse_config(bytes: &[u8]) -> io::Result<GuestConfig> {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    let config: GuestConfig = serde_json::from_slice(&bytes[..end])
        .map_err(|error| io::Error::other(format!("invalid config drive: {error}")))?;
    if !config.root.starts_with("/nix/store/") || config.root.contains([',', ':', '\\']) {
        return Err(io::Error::other("the release root must be a store path"));
    }
    Ok(config)
}

fn console(line: &str) {
    let mut stderr = io::stderr();
    let _ = writeln!(stderr, "{line}");
}

fn mount(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let c = |value: &str| CString::new(OsStr::new(value).as_bytes()).map_err(io::Error::other);
    let source = source.map(c).transpose()?;
    let target_c = c(target)?;
    let fstype = fstype.map(c).transpose()?;
    let data = data.map(c).transpose()?;
    // SAFETY: each pointer is null or a valid NUL-terminated string.
    let result = unsafe {
        libc::mount(
            source
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            target_c.as_ptr(),
            fstype
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr().cast()),
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("mount {target}: {error}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_device_comes_from_the_command_line() {
        assert_eq!(
            config_device("console=ttyS0 imageless.config=/dev/vdb reboot=k"),
            Some("/dev/vdb")
        );
        assert_eq!(config_device("console=ttyS0"), None);
        assert_eq!(config_device("imageless.config="), None);
    }

    #[test]
    fn the_config_drive_is_json_padded_with_nuls() {
        let mut bytes = br#"{"root":"/nix/store/00000000000000000000000000000000-rootfs","args":["/bin/app","serve"],"env":["A=1"]}"#.to_vec();
        bytes.resize(4096, 0);
        let config = parse_config(&bytes).unwrap();
        assert_eq!(config.args, ["/bin/app", "serve"]);
        assert_eq!(config.cwd, "/");
        assert!(parse_config(br#"{"root":"/etc","args":["x"]}"#).is_err());
        assert!(parse_config(br#"{"root":"/nix/store/a,b","args":["x"]}"#).is_err());
        assert!(parse_config(br#"{"root":"/nix/store/a","args":["x"],"extra":1}"#).is_err());
    }
}
