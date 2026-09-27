//! Filesystem confinement for node-side flake evaluation.
//!
//! Evaluating a flake runs code the image (or an allow-listed external source)
//! supplies, and Nix fetches that flake's inputs — direct or transitive,
//! including `path:` and `file://` ones — from whatever filesystem Nix can
//! see. Staging the top-level source (SPEC §2.3) bounds what the *installable*
//! names; it cannot bound what the flake's own inputs name. Confinement does:
//! the evaluator runs in a private mount namespace whose root is a fresh tmpfs
//! holding only an allowlist — `/nix`, the system's program and library trees,
//! the handful of `/etc` entries Nix needs (its config, TLS roots, name
//! resolution, account lookup), a few device nodes, a private `/proc` and `/tmp`, and the
//! caller's own paths (the staged source, a cache directory, a TLS bundle).
//! A local input anywhere in the graph resolves against that root and finds
//! nothing of the node's.
//!
//! Everything is computed by [`Confinement::prepare`] in the parent;
//! [`Confinement::enter`] performs raw syscalls only and allocates nothing, so
//! it is safe in a `pre_exec` hook of a multi-threaded process. It needs
//! `CAP_SYS_ADMIN` (in practice: root, before any privilege drop) and fails
//! closed otherwise.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// Read-only system trees and files the evaluator may need. Absent entries are
/// skipped. Nothing here is a home, spool, or service-state directory.
const SYSTEM_READ_ONLY: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/etc/nix",
    // NixOS: /etc/nix, /etc/ssl and friends are symlinks into /etc/static.
    "/etc/static",
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/passwd",
    "/etc/group",
    "/etc/localtime",
];

/// Writable because Nix writes there: the store and its database (when this
/// process owns the store rather than a daemon).
const SYSTEM_WRITABLE: &[&str] = &["/nix"];

/// The device nodes Nix and its sandbox setup open, bound one by one. Never
/// the host's whole `/dev`: that would hand a root evaluator raw disks,
/// `/dev/mem` and the kernel log.
const DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
];

/// How a bind is remounted once it is in place.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Access {
    /// Read-only, no setuid, no device nodes.
    ReadOnly,
    /// Writable, but still no setuid and no device nodes.
    Writable,
    /// A single device node: writable and openable, never setuid.
    Device,
}

enum Step {
    Tmpfs(CString),
    /// A private devpts instance: Nix allocates a pseudoterminal for every
    /// local build, and the host's ptys stay out of reach.
    Devpts(CString),
    Symlink {
        target: CString,
        link: CString,
    },
    Mkdir(CString),
    Touch(CString),
    Bind {
        source: CString,
        target: CString,
        access: Access,
    },
}

/// A prepared, allocation-free plan for entering a confined mount namespace.
pub struct Confinement {
    root: CString,
    steps: Vec<Step>,
}

impl Confinement {
    /// Plan a confinement rooted at `root`, an existing empty directory the
    /// caller owns and removes afterwards. The tmpfs is mounted over it only
    /// inside the new namespace, so the host never sees it populated.
    ///
    /// `writable` and `read_only` are the caller's own paths; each keeps its
    /// absolute path inside the confined root. Paths that do not exist are
    /// skipped, as is any path under an entry already bound.
    pub fn prepare(root: &Path, writable: &[PathBuf], read_only: &[PathBuf]) -> io::Result<Self> {
        if !root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "confinement root must be absolute",
            ));
        }
        let mut plan = Plan {
            root: root.to_path_buf(),
            steps: vec![Step::Tmpfs(c_path(root)?)],
            created: Vec::new(),
            bound: Vec::new(),
        };
        plan.directory(Path::new("/tmp"))?;
        plan.steps.push(Step::Tmpfs(c_path(&root.join("tmp"))?));
        for path in SYSTEM_WRITABLE {
            plan.bind(Path::new(path), Access::Writable)?;
        }
        for path in DEVICES {
            plan.bind(Path::new(path), Access::Device)?;
        }
        plan.directory(Path::new("/dev/pts"))?;
        plan.steps
            .push(Step::Devpts(c_path(&root.join("dev/pts"))?));
        // The links every OCI runtime puts in /dev. Builders rely on them:
        // nixpkgs' patchelf hook reads a process substitution through
        // /dev/fd, and fails every local build of a dynamic binary without it.
        for (target, link) in [
            ("pts/ptmx", "dev/ptmx"),
            ("/proc/self/fd", "dev/fd"),
            ("/proc/self/fd/0", "dev/stdin"),
            ("/proc/self/fd/1", "dev/stdout"),
            ("/proc/self/fd/2", "dev/stderr"),
        ] {
            plan.steps.push(Step::Symlink {
                target: c_path(Path::new(target))?,
                link: c_path(&root.join(link))?,
            });
        }
        // Never the host's /proc: in the host PID namespace its
        // /proc/<pid>/root links resolve against other processes' mount
        // namespaces — straight back to the host filesystem. `enter` mounts a
        // fresh procfs from inside a private PID namespace instead.
        plan.directory(Path::new("/proc"))?;
        for path in SYSTEM_READ_ONLY {
            plan.bind(Path::new(path), Access::ReadOnly)?;
        }
        for path in writable {
            plan.bind(path, Access::Writable)?;
        }
        for path in read_only {
            plan.bind(path, Access::ReadOnly)?;
        }
        Ok(Self {
            root: c_path(root)?,
            steps: plan.steps,
        })
    }

    /// The confinement node-side evaluation runs in: the system allowlist, the
    /// caller's `writable` paths (a staged source, a fetcher cache), the
    /// directory holding `nix` when it lives outside `/nix` and the system
    /// trees, and the TLS bundle the evaluator was told to trust.
    pub fn for_evaluation(
        root: &Path,
        nix: &Path,
        writable: &[PathBuf],
        certificates: &[PathBuf],
    ) -> io::Result<Self> {
        let mut read_only: Vec<PathBuf> = certificates
            .iter()
            .filter(|path| path.is_absolute())
            .cloned()
            .collect();
        if let Some(directory) = nix.parent().filter(|_| nix.is_absolute()) {
            read_only.push(directory.to_path_buf());
        }
        Self::prepare(root, writable, &read_only)
    }

    /// Enter the confinement: unshare the mount and PID namespaces, build the
    /// root, pivot into it, and fork. Syscalls only — safe between `fork` and
    /// `exec`.
    ///
    /// Only the forked child returns: it is PID 1 of the new PID namespace,
    /// with a procfs that shows that namespace alone. The calling process
    /// stays behind to wait for it and exits with its status (128 + signal for
    /// a signaled child), so a caller that spawned this process sees the
    /// evaluator's outcome as its own. When the child exits the kernel kills
    /// everything left in its namespace, so no helper outlives it.
    pub fn enter(&self) -> io::Result<()> {
        unsafe {
            check(libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID))?;
            // Nothing below may propagate back to the host's mount table.
            check(libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ))?;
            for step in &self.steps {
                match step {
                    Step::Tmpfs(target) => check(libc::mount(
                        c"tmpfs".as_ptr(),
                        target.as_ptr(),
                        c"tmpfs".as_ptr(),
                        libc::MS_NOSUID | libc::MS_NODEV,
                        c"mode=0755".as_ptr().cast(),
                    ))?,
                    Step::Devpts(target) => check(libc::mount(
                        c"devpts".as_ptr(),
                        target.as_ptr(),
                        c"devpts".as_ptr(),
                        libc::MS_NOSUID | libc::MS_NOEXEC,
                        c"newinstance,ptmxmode=0666,mode=0620".as_ptr().cast(),
                    ))?,
                    Step::Symlink { target, link } => {
                        check(libc::symlink(target.as_ptr(), link.as_ptr()))?
                    }
                    Step::Mkdir(path) => {
                        if libc::mkdir(path.as_ptr(), 0o755) == -1
                            && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
                        {
                            return Err(io::Error::last_os_error());
                        }
                    }
                    Step::Touch(path) => {
                        let fd = libc::open(
                            path.as_ptr(),
                            libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC,
                            0o644,
                        );
                        check(fd)?;
                        libc::close(fd);
                    }
                    Step::Bind {
                        source,
                        target,
                        access,
                    } => {
                        check(libc::mount(
                            source.as_ptr(),
                            target.as_ptr(),
                            std::ptr::null(),
                            libc::MS_BIND | libc::MS_REC,
                            std::ptr::null(),
                        ))?;
                        let flags = match access {
                            Access::ReadOnly => libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
                            Access::Writable => libc::MS_NOSUID | libc::MS_NODEV,
                            Access::Device => libc::MS_NOSUID,
                        };
                        check(libc::mount(
                            std::ptr::null(),
                            target.as_ptr(),
                            std::ptr::null(),
                            libc::MS_BIND | libc::MS_REMOUNT | flags,
                            std::ptr::null(),
                        ))?;
                    }
                }
            }
            check(libc::chdir(self.root.as_ptr()))?;
            check(libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) as i32)?;
            check(libc::umount2(c".".as_ptr(), libc::MNT_DETACH))?;
            check(libc::chdir(c"/".as_ptr()))?;

            // The first child forked after unsharing a PID namespace is its
            // PID 1; this process itself stays in the old one.
            let child = libc::fork();
            check(child)?;
            if child > 0 {
                wait_and_mirror(child);
            }
            // A fork clears the parent-death signal: re-arm it, so the
            // evaluator — and with it the namespace — dies with its waiter.
            check(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL))?;
            // PID 1 of the new namespace: this procfs lists its own
            // processes, so /proc/<pid>/root never leaves the confined root.
            check(libc::mount(
                c"proc".as_ptr(),
                c"/proc".as_ptr(),
                c"proc".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                std::ptr::null(),
            ))?;
            harden_proc()?;
        }
        Ok(())
    }
}

/// runc's default read-only procfs paths: a fresh procfs is still the kernel's,
/// and a root process writing `/proc/sys/kernel/core_pattern` or
/// `/proc/sysrq-trigger` acts on the host whatever its mount namespace.
const PROC_READ_ONLY: &[&std::ffi::CStr] = &[
    c"/proc/bus",
    c"/proc/fs",
    c"/proc/irq",
    c"/proc/sys",
    c"/proc/sysrq-trigger",
];

/// runc's default masked procfs files, which read kernel memory or state no
/// evaluation needs: /dev/null is bound over each.
const PROC_MASKED_FILES: &[&std::ffi::CStr] = &[
    c"/proc/kcore",
    c"/proc/keys",
    c"/proc/latency_stats",
    c"/proc/timer_list",
    c"/proc/timer_stats",
    c"/proc/sched_debug",
];

/// runc's default masked procfs directories: an empty read-only tmpfs each.
const PROC_MASKED_DIRECTORIES: &[&std::ffi::CStr] =
    &[c"/proc/acpi", c"/proc/asound", c"/proc/scsi"];

/// Apply the read-only and masked procfs paths. Paths this kernel does not
/// have are skipped. Syscalls only.
unsafe fn harden_proc() -> io::Result<()> {
    let absent = || io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT);
    for path in PROC_READ_ONLY {
        if libc::mount(
            path.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        ) == -1
        {
            if absent() {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        check(libc::mount(
            std::ptr::null(),
            path.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND
                | libc::MS_REMOUNT
                | libc::MS_RDONLY
                | libc::MS_NOSUID
                | libc::MS_NODEV
                | libc::MS_NOEXEC,
            std::ptr::null(),
        ))?;
    }
    for path in PROC_MASKED_FILES {
        if libc::mount(
            c"/dev/null".as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        ) == -1
            && !absent()
        {
            return Err(io::Error::last_os_error());
        }
    }
    for path in PROC_MASKED_DIRECTORIES {
        if libc::mount(
            c"tmpfs".as_ptr(),
            path.as_ptr(),
            c"tmpfs".as_ptr(),
            libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            std::ptr::null(),
        ) == -1
            && !absent()
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Close every descriptor above stderr. `close_range` where the kernel has it
/// (5.9+); otherwise one close per possible descriptor, up to the limit.
unsafe fn close_from_three() {
    if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) == 0 {
        return;
    }
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let highest = if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
        limit.rlim_cur.min(1 << 20) as libc::c_int
    } else {
        1024
    };
    for fd in 3..highest {
        libc::close(fd);
    }
}

/// The parent side of [`Confinement::enter`]'s fork: never returns.
///
/// It drops every descriptor above stderr first. Whoever spawned this process
/// may be waiting for EOF on a pipe only `exec` closes (Rust's spawn reports
/// exec failures that way); this side never execs, so holding its copy would
/// stall that caller for the evaluator's whole lifetime. It dies with its own
/// parent, and the evaluator — PID 1 of the namespace — dies with it through
/// the parent-death signal the spawner installs after this returns in the
/// child, taking the namespace with it.
unsafe fn wait_and_mirror(child: libc::pid_t) -> ! {
    close_from_three();
    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
    let mut status = 0;
    loop {
        if libc::waitpid(child, &mut status, 0) == child {
            break;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            libc::_exit(1);
        }
    }
    if libc::WIFEXITED(status) {
        libc::_exit(libc::WEXITSTATUS(status));
    }
    if libc::WIFSIGNALED(status) {
        libc::_exit(128 + libc::WTERMSIG(status));
    }
    libc::_exit(1)
}

struct Plan {
    root: PathBuf,
    steps: Vec<Step>,
    created: Vec<PathBuf>,
    bound: Vec<PathBuf>,
}

impl Plan {
    fn bind(&mut self, source: &Path, access: Access) -> io::Result<()> {
        if !source.is_absolute()
            || source
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "confinement paths must be absolute and canonical",
            ));
        }
        if self.bound.iter().any(|bound| source.starts_with(bound)) {
            return Ok(());
        }
        // Follow the source: a symlinked /etc/resolv.conf binds the file it
        // names, which is what the resolver inside the namespace must read.
        let metadata = match std::fs::metadata(source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if let Some(parent) = source.parent() {
            self.directory(parent)?;
        }
        let target = self.inside(source);
        if metadata.is_dir() {
            self.directory(source)?;
        } else {
            self.steps.push(Step::Touch(c_path(&target)?));
        }
        self.steps.push(Step::Bind {
            source: c_path(source)?,
            target: c_path(&target)?,
            access,
        });
        self.bound.push(source.to_path_buf());
        Ok(())
    }

    /// Create `path` and its ancestors inside the root, once each.
    fn directory(&mut self, path: &Path) -> io::Result<()> {
        let mut current = PathBuf::from("/");
        for component in path.components().skip(1) {
            current.push(component);
            if self.created.contains(&current) {
                continue;
            }
            self.steps
                .push(Step::Mkdir(c_path(&self.inside(&current))?));
            self.created.push(current.clone());
        }
        Ok(())
    }

    fn inside(&self, path: &Path) -> PathBuf {
        self.root.join(path.strip_prefix("/").unwrap_or(path))
    }
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    fn temporary(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("il-confine-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn plan_creates_each_ancestor_once_and_skips_absent_and_nested_paths() {
        let base = temporary("plan");
        let root = base.join("root");
        std::fs::create_dir(&root).unwrap();
        let source = base.join("source");
        std::fs::create_dir_all(source.join("inner")).unwrap();
        let plan = Confinement::prepare(
            &root,
            &[source.clone(), source.join("inner"), base.join("missing")],
            &[],
        )
        .unwrap();
        let binds: Vec<_> = plan
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Bind { source, .. } => Some(source.to_str().unwrap().to_string()),
                _ => None,
            })
            .collect();
        assert!(binds.contains(&source.display().to_string()));
        assert!(!binds.iter().any(|bind| bind.ends_with("/inner")));
        assert!(!binds.iter().any(|bind| bind.ends_with("/missing")));
        let mkdirs: Vec<_> = plan
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Mkdir(path) => Some(path.clone()),
                _ => None,
            })
            .collect();
        let mut unique = mkdirs.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(mkdirs.len(), unique.len());
        assert!(Confinement::prepare(Path::new("relative"), &[], &[]).is_err());
        assert!(Confinement::prepare(&root, &[base.join("a/../b")], &[]).is_err());
        std::fs::remove_dir_all(base).unwrap();
    }

    /// Runs only where mount namespaces can be created (root outside a build
    /// sandbox); elsewhere it says so and passes, because the property it
    /// checks cannot be observed without the privilege it needs. The Docker
    /// and CRI acceptance gates exercise the same path end to end.
    #[test]
    fn a_confined_process_sees_its_allowlist_and_nothing_else_of_the_host() {
        let base = temporary("enter");
        let root = base.join("root");
        let allowed = base.join("allowed");
        let hidden = base.join("hidden");
        for directory in [&root, &allowed, &hidden] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(allowed.join("present"), "ok").unwrap();
        std::fs::write(hidden.join("absent"), "host-only").unwrap();
        let confinement = Confinement::prepare(&root, std::slice::from_ref(&allowed), &[]).unwrap();

        // Beyond the allowlist: the shell is PID 1 of its own namespace, so
        // /proc/1/root is the confined root and never reaches the host file;
        // /dev holds the allowed character devices and nothing like /dev/mem.
        let script = format!(
            "test -f '{present}' && test ! -e '{absent}' && test $$ = 1 \
             && test ! -e '/proc/1/root{absent}' && test -d /proc/self \
             && test -c /dev/null && test ! -e /dev/mem && test ! -e /dev/sda \
             && test -c /dev/pts/ptmx && test -L /dev/ptmx \
             && exec 3</dev/null && test -e /dev/fd/3 \
             && test -L /dev/stdin && test -L /dev/stdout && test -L /dev/stderr \
             && ! sh -c 'echo x > /proc/sys/kernel/hostname' 2>/dev/null \
             && ! test -s /proc/kcore \
             && echo ok > /dev/null && touch '{allowed}/written'",
            present = allowed.join("present").display(),
            absent = hidden.join("absent").display(),
            allowed = allowed.display(),
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script]);
        unsafe {
            command.pre_exec(move || confinement.enter());
        }
        match command.status() {
            Ok(status) => {
                assert!(status.success(), "confined view was wrong: {status}");
                // Writable binds write through to the host path.
                assert!(allowed.join("written").exists());
                // The tmpfs lived only in the child's namespace.
                assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
            }
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EPERM) | Some(libc::EACCES) | Some(libc::EINVAL)
                ) =>
            {
                eprintln!("skipping: cannot create a mount namespace here ({error})");
            }
            Err(error) => panic!("confined spawn failed: {error}"),
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    /// The timeout's group kill must reach the forked evaluator, not just the
    /// process `spawn` returned. Root-only, like the test above.
    #[test]
    fn a_timed_out_confined_command_leaves_nothing_running() {
        let base = temporary("timeout");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let confinement = Confinement::prepare(&root, &[], &[]).unwrap();
        // A distinctive argv, so the host's view of every PID namespace can
        // be searched for survivors. setsid tries to leave the group as well.
        // Built at run time, so no other command line on the host (a shell
        // holding this source, say) can contain it.
        let marker = format!("sleep 31.{}", std::process::id());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &format!("setsid {marker} & {marker}")]);
        let started = std::time::Instant::now();
        match crate::nix::run_command_confined(
            &mut command,
            std::time::Duration::from_millis(300),
            Some(confinement),
        ) {
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EPERM) | Some(libc::EACCES) | Some(libc::EINVAL)
                ) =>
            {
                eprintln!("skipping: cannot create a mount namespace here ({error})");
                std::fs::remove_dir_all(base).unwrap();
                return;
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let mut survivors = String::new();
        for _ in 0..50 {
            survivors = String::from_utf8_lossy(
                &Command::new("pgrep")
                    .args(["-f", &marker])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .into_owned();
            if survivors.trim().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(survivors.trim().is_empty(), "left running: {survivors}");
        std::fs::remove_dir_all(base).unwrap();
    }
}
