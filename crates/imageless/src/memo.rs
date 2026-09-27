//! Memoized evaluation of embedded seeds (SPEC §2.4).
//!
//! An in-image flake evaluated against the lock it ships is a pure function
//! of its staged tree, the output attribute, the node's system and the Nix
//! that evaluates it. Every restart of the same container, and every replica
//! of it on a node, paid that evaluation again: most of a second for a small
//! seed, even with Nix's own evaluation cache warm. The memo maps
//! `(tree, output, system, nix)` to the realised path, so a repeat skips Nix
//! entirely.
//!
//! An entry is a symlink named by the key, under a directory of
//! /nix/var/nix/gcroots: the entry is its own GC root, so the path it names
//! stays valid for as long as the entry exists. Entries are bounded and
//! evicted least recently used first; evicting one only costs the next
//! create an evaluation. A seed that may lock its inputs at evaluation time
//! (`allow_unlocked_inputs`) is never memoized, and neither is an external
//! reference: neither is a function of the bytes the node staged.

use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const DEFAULT_EVALUATION_MEMO_DIRECTORY: &str = "/nix/var/nix/gcroots/imageless-memo";
pub const EVALUATION_MEMO_ENV: &str = "IMAGELESS_EVALUATION_MEMO";

/// Entries kept. Each pins one realised root (and its closure) against GC, so
/// the bound is on what a node keeps alive for seeds no container runs.
const CAPACITY: usize = 64;

/// Bumped whenever what the key covers changes, so older entries stop
/// matching instead of answering for something they did not cover.
const KEY_SCHEMA: &str = "imageless.evaluation-memo.v1";

#[derive(Clone, Debug)]
pub(crate) struct EvaluationMemo {
    directory: PathBuf,
}

/// `IMAGELESS_EVALUATION_MEMO`: unset for the default directory, `none` to
/// evaluate every time, or another absolute directory under the GC roots.
/// Unit tests default to none, so no test shares the node's real memo.
pub fn evaluation_memo_from_environment() -> Option<PathBuf> {
    match std::env::var_os(EVALUATION_MEMO_ENV) {
        None if cfg!(test) => None,
        None => Some(PathBuf::from(DEFAULT_EVALUATION_MEMO_DIRECTORY)),
        Some(value) if value.is_empty() => Some(PathBuf::from(DEFAULT_EVALUATION_MEMO_DIRECTORY)),
        Some(value) if value == "none" => None,
        Some(value) => Some(PathBuf::from(value)),
    }
}

impl EvaluationMemo {
    /// Opens the memo, creating its directory. `None` when the directory is
    /// not one only this user can write: an entry names what a container
    /// runs, so a memo anyone else could write would be a way to choose it.
    pub(crate) fn open(directory: &Path) -> Option<Self> {
        if !directory.is_absolute() {
            return None;
        }
        if let Err(error) = std::fs::create_dir_all(directory) {
            eprintln!(
                "imageless: evaluation memo disabled: {}: {error}",
                directory.display()
            );
            return None;
        }
        let metadata = std::fs::symlink_metadata(directory).ok()?;
        // SAFETY: geteuid has no preconditions.
        let owner = unsafe { libc::geteuid() };
        if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o022 != 0 {
            eprintln!(
                "imageless: evaluation memo disabled: {} must be a directory owned by uid {owner} and not group- or world-writable",
                directory.display()
            );
            return None;
        }
        let _ = std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755));
        Some(Self {
            directory: directory.to_path_buf(),
        })
    }

    /// The realised path recorded for `key`. A hit is marked as used, for
    /// eviction. Whether the path is still valid is for the caller to find
    /// out the way it registers any root, and to [`Self::forget`] it if not.
    pub(crate) fn lookup(&self, key: &str) -> Option<String> {
        let entry = self.directory.join(key);
        let target = std::fs::read_link(&entry).ok()?;
        let target = target.to_str()?.to_string();
        if crate::spec::validate_store_path("memo entry", &target).is_err() {
            self.forget(key);
            return None;
        }
        touch(&entry);
        Some(target)
    }

    /// Records `store_path` for `key`, then evicts beyond the capacity.
    pub(crate) fn record(&self, key: &str, store_path: &str) {
        let entry = self.directory.join(key);
        let pending = self
            .directory
            .join(format!(".{key}.{}.pending", std::process::id()));
        let _ = std::fs::remove_file(&pending);
        let recorded = std::os::unix::fs::symlink(store_path, &pending)
            .and_then(|()| std::fs::rename(&pending, &entry));
        if recorded.is_err() {
            let _ = std::fs::remove_file(&pending);
            return;
        }
        self.evict();
    }

    pub(crate) fn forget(&self, key: &str) {
        let _ = std::fs::remove_file(self.directory.join(key));
    }

    fn evict(&self) {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return;
        };
        let mut entries: Vec<(i64, i64, PathBuf)> = entries
            .flatten()
            .filter(|entry| !entry.file_name().as_bytes().starts_with(b"."))
            .filter_map(|entry| {
                let metadata = entry.path().symlink_metadata().ok()?;
                metadata
                    .file_type()
                    .is_symlink()
                    .then(|| (metadata.mtime(), metadata.mtime_nsec(), entry.path()))
            })
            .collect();
        if entries.len() <= CAPACITY {
            return;
        }
        entries.sort();
        for (_, _, path) in &entries[..entries.len() - CAPACITY] {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The memo key for evaluating output `output` of the flake staged at
/// `tree`, for `system`, with the Nix at `nix`.
pub(crate) fn key(tree: &Path, output: &str, system: &str, nix: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    for field in [KEY_SCHEMA.as_bytes(), output.as_bytes(), system.as_bytes()] {
        framed(&mut hasher, field);
    }
    // The Nix binary's store path names its version: a different evaluator
    // may evaluate differently, and must not be answered for.
    let nix = std::fs::canonicalize(nix).unwrap_or_else(|_| nix.to_path_buf());
    framed(&mut hasher, nix.as_os_str().as_bytes());
    hash_tree(&mut hasher, tree, Path::new(""))?;
    Ok(hex::encode(hasher.finalize()))
}

fn framed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Hashes a staged tree: every entry's relative path, kind, executable bit and
/// content, in a fixed order. Staging admits only regular files and
/// directories (SPEC §2.3), so anything else is an error rather than a guess.
fn hash_tree(hasher: &mut Sha256, root: &Path, relative: &Path) -> io::Result<()> {
    let directory = root.join(relative);
    let mut names: Vec<_> = std::fs::read_dir(&directory)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<_, _>>()?;
    names.sort();
    for name in names {
        let path = relative.join(&name);
        let metadata = std::fs::symlink_metadata(root.join(&path))?;
        if metadata.is_dir() {
            framed(hasher, b"directory");
            framed(hasher, path.as_os_str().as_bytes());
            hash_tree(hasher, root, &path)?;
        } else if metadata.is_file() {
            framed(hasher, b"file");
            framed(hasher, path.as_os_str().as_bytes());
            framed(hasher, &[u8::from(metadata.mode() & 0o111 != 0)]);
            framed(hasher, &Sha256::digest(std::fs::read(root.join(&path))?));
        } else {
            return Err(io::Error::other(
                "a staged seed holds something other than files and directories",
            ));
        }
    }
    Ok(())
}

/// Marks a memo entry as used without following it.
fn touch(entry: &Path) {
    let Ok(path) = CString::new(entry.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: a valid path; a null times pointer means "now".
    unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            std::ptr::null(),
            libc::AT_SYMLINK_NOFOLLOW,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("il-memo-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn seed(base: &Path) -> PathBuf {
        let tree = base.join("seed");
        std::fs::create_dir_all(tree.join("lib")).unwrap();
        std::fs::write(tree.join("flake.nix"), "{ outputs = _: { }; }").unwrap();
        std::fs::write(tree.join("flake.lock"), "{}").unwrap();
        std::fs::write(tree.join("lib/a.nix"), "1").unwrap();
        tree
    }

    #[test]
    fn the_key_covers_the_tree_output_system_and_evaluator() {
        let base = temporary("key");
        let tree = seed(&base);
        let nix = Path::new("/nix/store/00000000000000000000000000000000-nix/bin/nix");
        let key = |tree: &Path| super::key(tree, "rootfs", "x86_64-linux", nix).unwrap();
        let original = key(&tree);
        assert_eq!(original, key(&tree), "stable");

        // Identical bytes staged elsewhere are the same seed.
        let copy = base.join("copy");
        std::fs::create_dir_all(copy.join("lib")).unwrap();
        for file in ["flake.nix", "flake.lock", "lib/a.nix"] {
            std::fs::copy(tree.join(file), copy.join(file)).unwrap();
        }
        assert_eq!(original, key(&copy));

        assert_ne!(
            original,
            super::key(&tree, "other", "x86_64-linux", nix).unwrap()
        );
        assert_ne!(
            original,
            super::key(&tree, "rootfs", "aarch64-linux", nix).unwrap()
        );
        assert_ne!(
            original,
            super::key(
                &tree,
                "rootfs",
                "x86_64-linux",
                Path::new("/nix/store/11111111111111111111111111111111-nix/bin/nix")
            )
            .unwrap()
        );
        std::fs::write(tree.join("flake.lock"), "{ }").unwrap();
        assert_ne!(original, key(&tree), "the lock is part of the seed");
        std::fs::write(tree.join("flake.lock"), "{}").unwrap();
        std::fs::set_permissions(
            tree.join("lib/a.nix"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_ne!(original, key(&tree), "so is the executable bit");
        std::fs::set_permissions(
            tree.join("lib/a.nix"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::rename(tree.join("lib/a.nix"), tree.join("lib/b.nix")).unwrap();
        assert_ne!(original, key(&tree), "and every name");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn entries_round_trip_and_malformed_ones_are_forgotten() {
        let base = temporary("entries");
        let memo = EvaluationMemo::open(&base.join("memo")).unwrap();
        let store_path = "/nix/store/00000000000000000000000000000000-rootfs";
        assert_eq!(memo.lookup("k"), None);
        memo.record("k", store_path);
        assert_eq!(memo.lookup("k").as_deref(), Some(store_path));
        memo.forget("k");
        assert_eq!(memo.lookup("k"), None);

        // Only a store path is an answer.
        std::os::unix::fs::symlink("/etc", base.join("memo/k")).unwrap();
        assert_eq!(memo.lookup("k"), None);
        assert!(
            base.join("memo/k").symlink_metadata().is_err(),
            "and it is dropped"
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_memo_others_can_write_is_not_used() {
        let base = temporary("writable");
        let directory = base.join("memo");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(EvaluationMemo::open(&directory).is_none());
        assert!(EvaluationMemo::open(Path::new("relative")).is_none());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn eviction_keeps_the_most_recently_used() {
        let base = temporary("evict");
        let memo = EvaluationMemo::open(&base.join("memo")).unwrap();
        for index in 0..CAPACITY + 3 {
            // Targets need not exist for eviction; lookups are not made here.
            memo.record(
                &format!("k{index:03}"),
                "/nix/store/00000000000000000000000000000000-x",
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let remaining = std::fs::read_dir(base.join("memo")).unwrap().count();
        assert_eq!(remaining, CAPACITY);
        assert!(
            base.join("memo/k000").symlink_metadata().is_err(),
            "the oldest went first"
        );
        assert!(base
            .join(format!("memo/k{:03}", CAPACITY + 2))
            .symlink_metadata()
            .is_ok());
        std::fs::remove_dir_all(base).unwrap();
    }
}
