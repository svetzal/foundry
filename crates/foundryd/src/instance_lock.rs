//! Single-instance guard for `foundryd`.
//!
//! Every recovery sweep `foundryd` runs at start assumes it is the only daemon
//! on this Foundry home: "nothing can be running, so every `running` item was
//! interrupted". A second process started while the real daemon is alive
//! breaks that assumption. On 2026-09-30 an agent ran `foundryd --version`
//! next to a live daemon; the second process ran its restart sweep against
//! the shared ledger and settled a still-running task `failed` with the
//! reason `daemon restarted` before it failed to bind and exited.
//!
//! The guard is an exclusive advisory lock (`flock`) on one file, held for the
//! life of the process. The kernel releases it when the process exits, however
//! it exits, so a crashed daemon never leaves a stale lock behind. The holder
//! writes its pid into the file so a refused start can name it.

use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

// Called through the trait path: newer std has inherent `File` lock methods
// with different signatures, and the workspace MSRV predates them.
use fs2::FileExt;

/// The held single-instance lock. Dropping it releases the lock.
#[derive(Debug)]
pub struct InstanceLock {
    // Held only for its lock; the kernel releases the `flock` when this
    // descriptor closes.
    _file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// The lock file this guard holds.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Why the lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds the lock. `pid` is the one it recorded, when the
    /// file names one.
    Held { pid: Option<u32> },
    /// The lock file could not be opened, locked or written.
    Io(io::Error),
}

impl From<io::Error> for LockError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Take the single-instance lock at `path`, or report who holds it.
///
/// The file is opened without truncation and only rewritten once the lock is
/// held, so a refused start leaves the holder's pid (and every other byte on
/// disk) exactly as it found them.
pub fn acquire(path: &Path) -> Result<InstanceLock, LockError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    if let Err(error) = FileExt::try_lock_exclusive(&file) {
        if is_contended(&error) {
            return Err(LockError::Held {
                pid: read_pid(&mut file),
            });
        }
        return Err(LockError::Io(error));
    }
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())?;
    file.sync_all()?;
    Ok(InstanceLock {
        _file: file,
        path: path.to_path_buf(),
    })
}

/// The pid of the process holding the lock at `path`, if one holds it.
///
/// A read-only probe: it never creates the file, and it reports a pid only
/// while that pid's lock is live — a pid left behind by a daemon that has
/// since exited is not reported.
pub fn running_holder(path: &Path) -> Option<u32> {
    let mut file = File::open(path).ok()?;
    match FileExt::try_lock_shared(&file) {
        Ok(()) => {
            // Best-effort: the probe's shared lock is released when `file`
            // drops anyway; an explicit unlock failure changes nothing.
            if let Err(error) = FileExt::unlock(&file) {
                tracing::debug!(error = %error, "releasing the instance-lock probe failed");
            }
            None
        }
        Err(error) if is_contended(&error) => read_pid(&mut file),
        Err(_) => None,
    }
}

fn is_contended(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

fn read_pid(file: &mut File) -> Option<u32> {
    let mut contents = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut contents).ok()?;
    contents.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_records_this_process_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foundryd.lock");

        let lock = acquire(&path).unwrap();

        assert_eq!(lock.path(), path);
        let recorded = std::fs::read_to_string(&path).unwrap();
        assert_eq!(recorded.trim(), std::process::id().to_string());
    }

    #[test]
    fn acquire_creates_a_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("foundryd.lock");

        let _lock = acquire(&path).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn a_second_acquire_is_refused_with_the_holder_pid_and_leaves_the_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foundryd.lock");
        let _held = acquire(&path).unwrap();
        let before = std::fs::read(&path).unwrap();

        let refused = acquire(&path).unwrap_err();

        match refused {
            LockError::Held { pid } => assert_eq!(pid, Some(std::process::id())),
            LockError::Io(error) => panic!("expected Held, got Io({error})"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn a_held_lock_with_no_pid_is_refused_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foundryd.lock");
        let holder = File::create(&path).unwrap();
        FileExt::lock_exclusive(&holder).unwrap();

        let refused = acquire(&path).unwrap_err();

        assert!(matches!(refused, LockError::Held { pid: None }));
    }

    #[test]
    fn the_lock_is_free_again_once_the_holder_drops_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foundryd.lock");
        drop(acquire(&path).unwrap());

        assert!(acquire(&path).is_ok());
    }

    #[test]
    fn running_holder_names_the_live_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foundryd.lock");
        let _held = acquire(&path).unwrap();

        assert_eq!(running_holder(&path), Some(std::process::id()));
    }

    #[test]
    fn running_holder_ignores_a_pid_left_by_an_exited_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foundryd.lock");
        std::fs::write(&path, "4242\n").unwrap();

        assert_eq!(running_holder(&path), None);
    }

    #[test]
    fn running_holder_never_creates_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foundryd.lock");

        assert_eq!(running_holder(&path), None);
        assert!(!path.exists());
    }
}
