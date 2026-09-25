//! Free disk space checks.
//!
//! A task or maintenance run that starts on a full disk fails late and badly:
//! a half-written worktree, a build that dies in the middle, a finalize step
//! that cannot commit ("Out of diskspace"). Checking first turns that into a
//! clear "insufficient disk" reason before anything is written.
//!
//! A filesystem counts as low when its free space is under **both** limits:
//! `FOUNDRY_MIN_FREE_DISK_GB` (default 15) and `FOUNDRY_MIN_FREE_DISK_PERCENT`
//! (default 10). Requiring both keeps a large, well-used disk with plenty of
//! gigabytes left from tripping on percentage alone. Set either to `0` to turn
//! the check off.

use std::path::{Path, PathBuf};

/// Default absolute free-space floor, in gigabytes.
pub const DEFAULT_MIN_FREE_GB: u64 = 15;
/// Default free-space floor, as a percentage of the filesystem.
pub const DEFAULT_MIN_FREE_PERCENT: u64 = 10;

const GB: u64 = 1_000_000_000;

/// The free-space limits a filesystem must stay above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskThreshold {
    pub min_free_bytes: u64,
    pub min_free_percent: u64,
}

impl Default for DiskThreshold {
    fn default() -> Self {
        Self {
            min_free_bytes: DEFAULT_MIN_FREE_GB * GB,
            min_free_percent: DEFAULT_MIN_FREE_PERCENT,
        }
    }
}

impl DiskThreshold {
    /// Limits from the environment, falling back to the defaults for an unset
    /// or unparseable variable.
    #[must_use]
    pub fn from_env() -> Self {
        let read = |name: &str, default: u64| {
            std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
        };
        Self {
            min_free_bytes: read("FOUNDRY_MIN_FREE_DISK_GB", DEFAULT_MIN_FREE_GB) * GB,
            min_free_percent: read("FOUNDRY_MIN_FREE_DISK_PERCENT", DEFAULT_MIN_FREE_PERCENT),
        }
    }
}

/// Free space on the filesystem holding `path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskSpace {
    pub path: PathBuf,
    pub available: u64,
    pub total: u64,
    /// The filesystem's device id, where the platform has one; two paths on
    /// one filesystem share it.
    pub device: Option<u64>,
}

impl DiskSpace {
    /// Free space as a whole percentage of the filesystem.
    #[must_use]
    pub fn free_percent(&self) -> u64 {
        if self.total == 0 {
            return 100;
        }
        u64::try_from(u128::from(self.available) * 100 / u128::from(self.total)).unwrap_or(100)
    }

    /// Whether this is under both limits.
    #[must_use]
    pub fn is_low(&self, threshold: DiskThreshold) -> bool {
        self.available < threshold.min_free_bytes
            && self.free_percent() < threshold.min_free_percent
    }

    /// "4.2 GB free (2%) on the filesystem holding /path".
    #[must_use]
    pub fn describe(&self) -> String {
        #[allow(clippy::cast_precision_loss, reason = "a one-decimal display value")]
        let gb = self.available as f64 / GB as f64;
        format!(
            "{gb:.1} GB free ({}%) on the filesystem holding {}",
            self.free_percent(),
            self.path.display()
        )
    }
}

/// Measure the filesystem holding `path`, walking up to the nearest existing
/// ancestor (a worktree directory may not exist yet).
///
/// # Errors
///
/// Returns the I/O error when no ancestor can be measured.
pub fn measure(path: &Path) -> std::io::Result<DiskSpace> {
    let mut probe = path;
    while !probe.exists() {
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break,
        }
    }
    Ok(DiskSpace {
        path: path.to_path_buf(),
        available: fs2::available_space(probe)?,
        total: fs2::total_space(probe)?,
        device: device_of(probe),
    })
}

#[cfg(unix)]
fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).ok().map(|m| m.dev())
}

#[cfg(not(unix))]
fn device_of(_path: &Path) -> Option<u64> {
    None
}

/// Every low filesystem among `paths`, one entry per filesystem.
#[must_use]
pub fn low_filesystems(paths: &[PathBuf], threshold: DiskThreshold) -> Vec<DiskSpace> {
    let mut low: Vec<DiskSpace> = Vec::new();
    for path in paths {
        match measure(path) {
            Ok(space) if space.is_low(threshold) => {
                // Two paths on one filesystem report the same totals.
                let same = |l: &DiskSpace| match (l.device, space.device) {
                    (Some(a), Some(b)) => a == b,
                    _ => l.total == space.total,
                };
                if !low.iter().any(same) {
                    low.push(space);
                }
            }
            Ok(_) => {}
            Err(e) => {
                // Best-effort: an unmeasurable path is not evidence of a full
                // disk; the run proceeds and fails on its own if it must.
                tracing::warn!(path = %path.display(), error = %e, "could not measure free disk space");
            }
        }
    }
    low
}

/// Refuse to start work when a filesystem it writes to is low.
///
/// # Errors
///
/// Returns "insufficient disk: …" naming each low filesystem and the limits.
pub fn ensure_room(paths: &[PathBuf], threshold: DiskThreshold) -> Result<(), String> {
    let low = low_filesystems(paths, threshold);
    if low.is_empty() {
        return Ok(());
    }
    let places: Vec<String> = low.iter().map(DiskSpace::describe).collect();
    Err(format!(
        "insufficient disk: {} (needs at least {} GB or {}% free)",
        places.join("; "),
        threshold.min_free_bytes / GB,
        threshold.min_free_percent
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(available_gb: u64, total_gb: u64) -> DiskSpace {
        DiskSpace {
            path: PathBuf::from("/srv"),
            available: available_gb * GB,
            total: total_gb * GB,
            device: None,
        }
    }

    #[test]
    fn low_means_under_both_limits() {
        let t = DiskThreshold::default();
        assert!(space(4, 194).is_low(t), "ops-01 at 98% full");
        assert!(!space(98, 1948).is_low(t), "a big disk at 5% free but 98 GB left is fine");
        assert!(!space(12, 50).is_low(t), "12 GB is 24% of a small disk");
        assert!(!space(59, 194).is_low(t));
    }

    #[test]
    fn a_zero_limit_turns_the_check_off() {
        let off = DiskThreshold {
            min_free_bytes: 0,
            min_free_percent: 10,
        };
        assert!(!space(0, 194).is_low(off));
    }

    #[test]
    fn descriptions_name_the_space_and_path() {
        assert_eq!(space(4, 194).describe(), "4.0 GB free (2%) on the filesystem holding /srv");
    }

    #[test]
    fn measuring_a_missing_path_uses_its_nearest_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let space = measure(&dir.path().join("not/yet/created")).unwrap();
        assert!(space.total > 0);
    }

    #[test]
    fn ensure_room_passes_or_names_the_low_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![dir.path().to_path_buf()];
        assert!(
            ensure_room(
                &paths,
                DiskThreshold {
                    min_free_bytes: 0,
                    min_free_percent: 0
                }
            )
            .is_ok()
        );
        let impossible = DiskThreshold {
            min_free_bytes: u64::MAX,
            min_free_percent: 101,
        };
        let err =
            ensure_room(&[dir.path().to_path_buf(), dir.path().join("x")], impossible).unwrap_err();
        assert!(err.starts_with("insufficient disk: "), "{err}");
        assert_eq!(
            err.matches("on the filesystem holding").count(),
            1,
            "one entry per filesystem: {err}"
        );
    }
}
