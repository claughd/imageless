//! The writable mountpoint layer over a materialized root (SPEC §4.5).
//!
//! An OCI runtime creates the destination of every mount it is asked for —
//! `/etc/hosts`, a service-account token directory, any volume — inside the
//! root before mounting over it. The materialized root is a store path shared
//! with every other container and the node, so it can take none of those
//! writes: where the store is mounted read-only the create fails, and where it
//! is not, the runtime writes into a store path and corrupts it.
//!
//! So the root handed to the runtime is an overlay: the store path as its only
//! lower layer, and a small per-container tmpfs as the upper one, which
//! receives the runtime's mountpoints and nothing else (the root is still
//! remounted read-only before the workload runs). The mounts live under a
//! private staging directory, so nothing propagates to or from other mount
//! namespaces.
//!
//! The layer stays mounted on the node for the container's whole life, because
//! runc keeps using the root's node path after `create`: every `runc exec`
//! starts with it as its working directory. Once the runtime returns, the
//! owner records the runc state file that names the layer as its root
//! ([`RootLayer::commit`]); the layer is released when that file is gone,
//! which is when runc has deleted the container. [`sweep`] does the
//! releasing, on every create and after every delete, and also reclaims a
//! layer whose owner died before committing it. A reboot clears /run.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Node-owned staging directory for root layers. Not under /run/imageless,
/// which the resolver unit's RuntimeDirectory removes when it stops.
pub const DEFAULT_ROOT_LAYER_DIRECTORY: &str = "/run/imageless-roots";
pub const ROOT_LAYER_DIRECTORY_ENV: &str = "IMAGELESS_ROOT_LAYERS";

/// Bounds the upper layer. It holds directories and empty files the runtime
/// creates as mountpoints; a workload never writes to it, because the root is
/// read-only by the time the workload runs.
/// Serializes preparing the staging directory. Not a `.lock` name, and a
/// dotfile, so [`sweep`] never mistakes it for a layer's claim.
const STAGING_GUARD: &str = ".staging-guard";

const UPPER_OPTIONS: &str = "mode=0755,size=4m,nr_inodes=16384";

/// A mounted root layer, held until [`RootLayer::commit`] or
/// [`RootLayer::detach`]. The lock marks the layer as owned: [`sweep`]
/// reclaims layers whose owner died before committing them.
#[derive(Clone)]
pub struct RootLayer {
    directory: PathBuf,
    _lock: Arc<File>,
}

impl std::fmt::Debug for RootLayer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RootLayer")
            .field("directory", &self.directory)
            .finish()
    }
}

impl PartialEq for RootLayer {
    fn eq(&self, other: &Self) -> bool {
        self.directory == other.directory
    }
}

impl Eq for RootLayer {}

impl RootLayer {
    /// Mounts a layer over `lower` under `staging`.
    pub fn mount(lower: &Path, staging: &Path) -> io::Result<Self> {
        let lower_metadata = std::fs::metadata(lower)?;
        if !lower_metadata.is_dir() {
            return Err(io::Error::other("the materialized root is not a directory"));
        }
        // Overlay options are comma separated and `:` separates lower layers;
        // no store path contains either, but a path that did must not be able
        // to name a second layer.
        let lower_text = lower
            .to_str()
            .filter(|text| !text.contains([',', ':', '\\']))
            .ok_or_else(|| {
                io::Error::other("the materialized root path cannot name an overlay layer")
            })?;
        prepare_staging(staging)?;
        sweep(staging);

        let (name, lock) = claim(staging)?;
        let directory = staging.join(&name);
        let layer = Self {
            directory,
            _lock: Arc::new(lock),
        };
        if let Err(error) = layer.assemble(lower_text, &lower_metadata) {
            layer.detach();
            return Err(error);
        }
        Ok(layer)
    }

    fn assemble(&self, lower: &str, lower_metadata: &std::fs::Metadata) -> io::Result<()> {
        std::fs::create_dir(&self.directory)?;
        mount(
            Some("tmpfs"),
            &self.directory,
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some(UPPER_OPTIONS),
        )?;
        let upper = self.directory.join("upper");
        let work = self.directory.join("work");
        for directory in [&upper, &work, &self.root()] {
            std::fs::create_dir(directory)?;
        }
        // The overlay's root directory is the upper one: give it the store
        // root's owner and mode so the container sees the root it would have.
        std::os::unix::fs::chown(
            &upper,
            Some(lower_metadata.uid()),
            Some(lower_metadata.gid()),
        )?;
        std::fs::set_permissions(
            &upper,
            std::fs::Permissions::from_mode(lower_metadata.mode() & 0o7777),
        )?;
        let options = format!(
            "lowerdir={lower},upperdir={},workdir={}",
            upper.display(),
            work.display()
        );
        mount(
            Some("overlay"),
            &self.root(),
            Some("overlay"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some(&options),
        )
    }

    /// The directory to hand the runtime as `root.path`.
    pub fn root(&self) -> PathBuf {
        self.directory.join("root")
    }

    /// Detaches the layer from the node now: for a layer no container was
    /// created over. One that was keeps it alive in its own mount namespace
    /// until it exits, but `runc exec` into it would fail.
    pub fn detach(self) {
        release(&self.directory);
    }

    /// Hands the layer to the container the runtime created over it: the runc
    /// state directory `runc_root` is searched for the container whose root is
    /// this layer, and the layer lives until that container's state is gone.
    /// Without one — the create failed, or a foreground `run` has already
    /// finished — the layer is released now.
    pub fn commit(self, runc_root: &Path) {
        let Some(state) = container_state(runc_root, &self.root()) else {
            self.detach();
            return;
        };
        let written = std::fs::write(lock_path(&self.directory), state.as_os_str().as_bytes());
        if written.is_err() {
            // Without the record a sweep would take the layer for an orphan.
            // Keep the claim instead: the lock goes with this process, and the
            // layer with the next sweep after that, which is the leak-free
            // failure; a running container loses only `runc exec`.
            eprintln!(
                "imageless: could not record the root layer's container: {}",
                self.directory.display()
            );
        }
    }
}

/// The `state.json` under `runc_root` whose container runs on `root`.
fn container_state(runc_root: &Path, root: &Path) -> Option<PathBuf> {
    let root = root.to_str()?;
    std::fs::read_dir(runc_root)
        .ok()?
        .flatten()
        .find_map(|entry| {
            let state = entry.path().join("state.json");
            let bytes = std::fs::read(&state).ok()?;
            let document: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            (document["config"]["rootfs"].as_str() == Some(root)).then_some(state)
        })
}

fn release(directory: &Path) {
    let _ = unmount_detached(&directory.join("root"));
    let _ = unmount_detached(directory);
    let _ = std::fs::remove_dir(directory);
    let _ = std::fs::remove_file(lock_path(directory));
}

/// Releases layers whose container runc has deleted, and layers whose owner
/// exited before committing them (a shim killed mid-create). A live owner
/// holds its lock, so a layer being created is skipped.
pub fn sweep(staging: &Path) {
    let Ok(entries) = std::fs::read_dir(staging) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.starts_with('.'))
        else {
            continue;
        };
        let (name, claimed) = if let Some(name) = file_name.strip_suffix(".lock") {
            (name, true)
        } else if let Some(name) = file_name.strip_suffix(".pending") {
            (name, false)
        } else {
            continue;
        };
        let Ok(lock) = File::open(&path) else {
            continue;
        };
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            continue;
        }
        if claimed {
            // A committed layer names its container's runc state file; the
            // layer is live while that file exists. An empty record is a
            // layer whose owner died before committing it.
            let record = std::fs::read(&path).unwrap_or_default();
            let live =
                !record.is_empty() && Path::new(std::ffi::OsStr::from_bytes(&record)).exists();
            if !live {
                release(&staging.join(name));
            }
        } else {
            // An owner that died between creating its claim and renaming it:
            // nothing was mounted yet.
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn lock_path(directory: &Path) -> PathBuf {
    let mut path = directory.as_os_str().to_owned();
    path.push(".lock");
    PathBuf::from(path)
}

/// Creates `<name>.lock`, locked, for a fresh name. The file is created and
/// locked under a temporary name and renamed into place, so [`sweep`] never
/// sees an unlocked lock that belongs to a live owner.
fn claim(staging: &Path) -> io::Result<(String, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    loop {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let name = format!(
            "{}-{nanos:x}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let pending = staging.join(format!("{name}.pending"));
        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&pending)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        // SAFETY: flock on the descriptor just opened.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            let error = io::Error::last_os_error();
            let _ = std::fs::remove_file(&pending);
            return Err(error);
        }
        std::fs::rename(&pending, staging.join(format!("{name}.lock")))?;
        return Ok((name, file));
    }
}

/// Makes `staging` a private mount point, so nothing mounted beneath it
/// propagates into any other mount namespace, and nothing unmounted
/// elsewhere propagates in. Serialized by a lock, because two shims creating
/// at once would otherwise stack two binds.
fn prepare_staging(staging: &Path) -> io::Result<()> {
    std::fs::create_dir_all(staging)?;
    std::fs::set_permissions(staging, std::fs::Permissions::from_mode(0o700))?;
    let guard = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(staging.join(STAGING_GUARD))?;
    // SAFETY: flock on the descriptor just opened; released when it closes.
    if unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if !is_mount_point(staging)? {
        bind_onto_itself(staging)?;
    }
    mount(None, staging, None, libc::MS_PRIVATE, None)
}

fn bind_onto_itself(staging: &Path) -> io::Result<()> {
    let source = CString::new(staging.as_os_str().as_bytes())?;
    let target = source.clone();
    // SAFETY: both strings are valid and NUL-terminated.
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether `path` is itself the mount point of an entry in this process's
/// mount table.
fn is_mount_point(path: &Path) -> io::Result<bool> {
    let canonical = std::fs::canonicalize(path)?;
    let table = std::fs::read_to_string("/proc/self/mountinfo")?;
    Ok(table.lines().any(|line| {
        line.split(' ')
            .nth(4)
            .is_some_and(|point| unescape(point) == canonical.as_os_str().as_bytes())
    }))
}

/// mountinfo escapes space, tab, newline and backslash as `\ooo`.
fn unescape(field: &str) -> Vec<u8> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let octal = bytes.get(index + 1..index + 4).filter(|digits| {
            digits
                .iter()
                .all(|digit| (b'0'..=b'3').contains(&digits[0]) && (b'0'..=b'7').contains(digit))
        });
        if let (b'\\', Some(digits)) = (bytes[index], octal) {
            let value = (digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + (digits[2] - b'0');
            out.push(value);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    out
}

fn mount(
    source: Option<&str>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let source = source.map(CString::new).transpose()?;
    let target = CString::new(target.as_os_str().as_bytes())?;
    let fstype = fstype.map(CString::new).transpose()?;
    let data = data.map(CString::new).transpose()?;
    // SAFETY: every pointer is either null or a valid NUL-terminated string
    // that outlives the call.
    let result = unsafe {
        libc::mount(
            source
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            target.as_ptr(),
            fstype
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr().cast()),
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unmount_detached(target: &Path) -> io::Result<()> {
    let target = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: a valid NUL-terminated path.
    if unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("il-layer-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// A read-only stand-in for a store path, and a staging directory; `None`
    /// where this process cannot mount (the Nix build sandbox), in which case
    /// the acceptance gates exercise the same path end to end.
    fn fixture(label: &str) -> Option<(PathBuf, PathBuf, RootLayer)> {
        let base = temporary(label);
        let lower = base.join("store-path");
        std::fs::create_dir_all(lower.join("etc")).unwrap();
        std::fs::write(lower.join("etc/os-release"), "ID=imageless\n").unwrap();
        std::fs::set_permissions(&lower, std::fs::Permissions::from_mode(0o555)).unwrap();
        let staging = base.join("staging");
        match RootLayer::mount(&lower, &staging) {
            Ok(layer) => Some((base, lower, layer)),
            Err(error) => {
                eprintln!("skipping: cannot mount a root layer here ({error})");
                None
            }
        }
    }

    fn cleanup(base: &Path) {
        let _ = unmount_detached(&base.join("staging"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn mountpoints_land_in_the_layer_and_never_in_the_store_path() {
        let Some((base, lower, layer)) = fixture("mountpoints") else {
            return;
        };
        let root = layer.root();
        assert_eq!(
            std::fs::read_to_string(root.join("etc/os-release")).unwrap(),
            "ID=imageless\n"
        );
        assert_eq!(
            std::fs::metadata(&root).unwrap().mode() & 0o7777,
            0o555,
            "the root keeps the store path's mode"
        );
        // What runc does for a service-account token and /etc/hosts.
        std::fs::create_dir_all(root.join("var/run/secrets/kubernetes.io/serviceaccount")).unwrap();
        File::create(root.join("etc/hosts")).unwrap();
        assert!(!lower.join("var").exists());
        assert!(!lower.join("etc/hosts").exists());

        // Nothing mounted under staging propagates to the node, and the
        // staging directory itself is a private mount point.
        let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        let staging = std::fs::canonicalize(base.join("staging")).unwrap();
        let line = table
            .lines()
            .find(|line| line.split(' ').nth(4) == staging.to_str())
            .unwrap();
        assert!(!line.contains("shared:"), "{line}");
        cleanup(&base);
    }

    #[test]
    fn a_detached_layer_lives_as_long_as_something_holds_it() {
        let Some((base, _lower, layer)) = fixture("lifetime") else {
            return;
        };
        let directory = layer.directory.clone();
        // A container's mount namespace holds the layer the way this
        // descriptor does.
        let held = File::open(layer.root()).unwrap();
        layer.detach();
        assert!(!directory.exists(), "the node no longer sees the layer");
        assert!(!lock_path(&directory).exists());
        let name = CString::new("mountpoint").unwrap();
        // SAFETY: a directory descriptor and a valid name.
        assert_eq!(
            unsafe { libc::mkdirat(held.as_raw_fd(), name.as_ptr(), 0o755) },
            0
        );
        cleanup(&base);
    }

    #[test]
    fn a_layer_whose_owner_died_is_swept() {
        let Some((base, lower, layer)) = fixture("sweep") else {
            return;
        };
        let staging = base.join("staging");
        let directory = layer.directory.clone();
        // The owner exits without detaching: its lock goes with it.
        drop(layer);
        let live = RootLayer::mount(&lower, &staging).unwrap();
        assert!(!directory.exists(), "the orphan was reclaimed");
        assert!(
            live.root().join("etc/os-release").exists(),
            "a live layer is not"
        );
        sweep(&staging);
        assert!(live.root().join("etc/os-release").exists());
        assert!(
            staging.join(STAGING_GUARD).exists(),
            "the staging guard is not a layer's claim"
        );
        live.detach();
        cleanup(&base);
    }

    #[test]
    fn a_committed_layer_lives_until_its_container_state_is_gone() {
        let Some((base, lower, layer)) = fixture("commit") else {
            return;
        };
        let staging = base.join("staging");
        let runc_root = base.join("runc");
        let state_directory = runc_root.join("container-1");
        std::fs::create_dir_all(&state_directory).unwrap();
        let state = state_directory.join("state.json");
        std::fs::write(
            &state,
            serde_json::to_vec(&serde_json::json!({ "config": { "rootfs": layer.root() } }))
                .unwrap(),
        )
        .unwrap();
        std::fs::write(runc_root.join("other.json"), "not a container").unwrap();
        let root = layer.root();
        layer.commit(&runc_root);

        // What runc exec needs: the root's node path, after create.
        sweep(&staging);
        assert!(
            root.join("etc/os-release").exists(),
            "a live container keeps its layer"
        );

        // runc delete removes the state; the next sweep releases the layer.
        std::fs::remove_dir_all(&state_directory).unwrap();
        sweep(&staging);
        assert!(!root.exists(), "a deleted container's layer is released");

        // A create that made no container releases its layer at once.
        let failed = RootLayer::mount(&lower, &staging).unwrap();
        let failed_root = failed.root();
        failed.commit(&runc_root);
        assert!(!failed_root.exists());
        cleanup(&base);
    }

    #[test]
    fn mountinfo_escapes_are_decoded() {
        assert_eq!(unescape("/run/a\\040b"), b"/run/a b");
        assert_eq!(unescape("/plain"), b"/plain");
        assert_eq!(unescape("/tail\\04"), b"/tail\\04");
    }
}
