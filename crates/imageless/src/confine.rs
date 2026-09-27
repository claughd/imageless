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
//! resolution, account lookup), `/dev`, `/proc`, a private `/tmp`, and the
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
/// process owns the store rather than a daemon), and device nodes.
const SYSTEM_WRITABLE: &[&str] = &["/nix", "/dev"];

enum Step {
    Tmpfs(CString),
    Mkdir(CString),
    Touch(CString),
    Bind {
        source: CString,
        target: CString,
        read_only: bool,
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
            plan.bind(Path::new(path), false)?;
        }
        plan.bind(Path::new("/proc"), true)?;
        for path in SYSTEM_READ_ONLY {
            plan.bind(Path::new(path), true)?;
        }
        for path in writable {
            plan.bind(path, false)?;
        }
        for path in read_only {
            plan.bind(path, true)?;
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

    /// Enter the confinement: unshare the mount namespace, build the root, and
    /// pivot into it. Syscalls only — safe between `fork` and `exec`.
    pub fn enter(&self) -> io::Result<()> {
        unsafe {
            check(libc::unshare(libc::CLONE_NEWNS))?;
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
                        read_only,
                    } => {
                        check(libc::mount(
                            source.as_ptr(),
                            target.as_ptr(),
                            std::ptr::null(),
                            libc::MS_BIND | libc::MS_REC,
                            std::ptr::null(),
                        ))?;
                        if *read_only {
                            check(libc::mount(
                                std::ptr::null(),
                                target.as_ptr(),
                                std::ptr::null(),
                                libc::MS_BIND
                                    | libc::MS_REMOUNT
                                    | libc::MS_RDONLY
                                    | libc::MS_NOSUID
                                    | libc::MS_NODEV,
                                std::ptr::null(),
                            ))?;
                        }
                    }
                }
            }
            check(libc::chdir(self.root.as_ptr()))?;
            check(libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) as i32)?;
            check(libc::umount2(c".".as_ptr(), libc::MNT_DETACH))?;
            check(libc::chdir(c"/".as_ptr()))?;
        }
        Ok(())
    }
}

struct Plan {
    root: PathBuf,
    steps: Vec<Step>,
    created: Vec<PathBuf>,
    bound: Vec<PathBuf>,
}

impl Plan {
    fn bind(&mut self, source: &Path, read_only: bool) -> io::Result<()> {
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
            read_only,
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

        let script = format!(
            "test -f '{}' && test ! -e '{}' && test ! -e /root/. -o -z \"$(ls -A /root 2>/dev/null)\" \
             && touch '{}/written'",
            allowed.join("present").display(),
            hidden.join("absent").display(),
            allowed.display(),
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
}
