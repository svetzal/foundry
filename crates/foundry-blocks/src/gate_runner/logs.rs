//! Complete stdout and stderr captures of failed gates.
//!
//! `GateResult::output` carries only a bounded view. When a gate fails, every
//! command it ran (the gate command, and the fix and re-check when self-heal
//! was attempted) is written in full under the gate-log root, one directory
//! per `run_gates` call, and the file paths are recorded in
//! `GateResult::logs`. The tree is pruned after every write so it stays small:
//! see [`GATE_LOG_MAX_AGE`] and [`GATE_LOG_MAX_TOTAL_BYTES`].
//!
//! The pure parts (stream cap, file naming, prune selection) are separate
//! functions; [`GateLogWriter`] and [`prune_gate_logs`] are the thin
//! filesystem shell around them.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Largest capture kept for one stream of one command. A larger stream keeps
/// its last 8 MiB, behind a one-line note giving the original size: failures
/// explain themselves at the end of the output.
pub const GATE_LOG_STREAM_CAP_BYTES: usize = 8 * 1024 * 1024;

/// Gate-log run directories older than this are deleted after each write.
/// Fourteen days covers a weekly review with margin.
pub const GATE_LOG_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Ceiling on the whole gate-log tree. After the age prune, the oldest run
/// directories are deleted until the tree is at or under this size. The ops
/// host's root filesystem is shared with every worktree and build cache, so
/// this tree must stay small.
pub const GATE_LOG_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

/// Where failed-gate logs go: `~/.foundry/evidence/gate-logs`.
///
/// Test builds (`cfg(test)` or the `test-support` feature) use a directory
/// under the system temporary directory instead, so test suites that drive
/// failing gates never write into a real Foundry home.
pub fn default_gate_log_root() -> PathBuf {
    if cfg!(any(test, feature = "test-support")) {
        std::env::temp_dir().join("foundry-test-gate-logs")
    } else {
        foundry_sdk::paths::foundry_home().join("evidence").join("gate-logs")
    }
}

/// How much of the gate-log tree survives a prune.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    /// Run directories last modified longer ago than this are deleted.
    pub max_age: Duration,
    /// After the age prune, oldest run directories are deleted until the
    /// tree is at or under this many bytes.
    pub max_total_bytes: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age: GATE_LOG_MAX_AGE,
            max_total_bytes: GATE_LOG_MAX_TOTAL_BYTES,
        }
    }
}

/// Which command of a gate a capture came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunRole {
    /// The gate command's first run.
    Initial,
    /// The gate's `fix_command`.
    Fix,
    /// The gate command re-run after a successful fix.
    Recheck,
}

impl RunRole {
    fn suffix(self) -> &'static str {
        match self {
            Self::Initial => "",
            Self::Fix => ".fix",
            Self::Recheck => ".recheck",
        }
    }
}

/// The complete output of one command run.
#[derive(Debug, Clone)]
pub(crate) struct CommandCapture {
    pub stdout: String,
    pub stderr: String,
}

/// A gate-log run directory as the prune sees it.
#[derive(Debug, Clone)]
pub(crate) struct RunDir {
    pub name: String,
    pub modified: SystemTime,
    pub bytes: u64,
}

/// Keep at most the last `cap` bytes of `text`, rounded to a char boundary,
/// behind a one-line note giving the original size.
pub(crate) fn cap_stream(text: &str, cap: usize) -> Cow<'_, str> {
    if text.len() <= cap {
        return Cow::Borrowed(text);
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    Cow::Owned(format!(
        "[foundry: output truncated; kept the last {} of {} bytes]\n{}",
        text.len() - start,
        text.len(),
        &text[start..]
    ))
}

/// A file-name-safe form of a gate name: ASCII letters, digits, `-` and `_`
/// are kept, anything else becomes `_`, at most 64 characters.
pub(crate) fn sanitize_gate_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if safe.is_empty() {
        "gate".to_string()
    } else {
        safe
    }
}

/// The run directory name: a UTC timestamp first, so lexical order is
/// chronological, then a uuid so concurrent runs never collide.
pub(crate) fn run_dir_name(now: chrono::DateTime<chrono::Utc>, id: uuid::Uuid) -> String {
    format!("{}-{id}", now.format("%Y%m%dT%H%M%SZ"))
}

/// The files to write for one failed gate: `(file name, content)` for every
/// non-empty stream of every run, each capped at `cap` bytes.
pub(crate) fn log_files<'a>(
    stem: &str,
    runs: &'a [(RunRole, &'a CommandCapture)],
    cap: usize,
) -> Vec<(String, Cow<'a, str>)> {
    runs.iter()
        .flat_map(|(role, capture)| {
            [
                ("stdout", capture.stdout.as_str()),
                ("stderr", capture.stderr.as_str()),
            ]
            .into_iter()
            .filter(|(_, text)| !text.is_empty())
            .map(move |(stream, text)| {
                (format!("{stem}{}.{stream}.log", role.suffix()), cap_stream(text, cap))
            })
        })
        .collect()
}

/// The run directories a prune deletes, oldest first.
///
/// First every directory whose age exceeds `policy.max_age`; then, while the
/// survivors together exceed `policy.max_total_bytes`, the oldest survivor by
/// name (names start with a UTC timestamp). `keep` (the directory just
/// written) is never selected.
pub(crate) fn select_for_pruning(
    entries: &[RunDir],
    now: SystemTime,
    policy: &RetentionPolicy,
    keep: &str,
) -> Vec<String> {
    let mut sorted: Vec<&RunDir> = entries.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let expired =
        |entry: &RunDir| now.duration_since(entry.modified).is_ok_and(|age| age > policy.max_age);
    let (doomed, survivors): (Vec<&RunDir>, Vec<&RunDir>) =
        sorted.into_iter().partition(|entry| entry.name != keep && expired(entry));
    let mut total: u64 = survivors.iter().map(|entry| entry.bytes).sum();
    let mut selected: Vec<String> = doomed.into_iter().map(|entry| entry.name.clone()).collect();
    for entry in survivors {
        if total <= policy.max_total_bytes {
            break;
        }
        if entry.name == keep {
            continue;
        }
        total -= entry.bytes;
        selected.push(entry.name.clone());
    }
    selected
}

/// Writes the logs of failed gates for one `run_gates` call.
///
/// The run directory is created on the first failed gate, so a run where
/// every gate passes leaves nothing on disk. Failures to write are logged and
/// yield no paths; they never change a gate's outcome.
pub(crate) struct GateLogWriter<'a> {
    root: &'a Path,
    policy: RetentionPolicy,
    run_dir: Option<PathBuf>,
    used_stems: HashSet<String>,
}

impl<'a> GateLogWriter<'a> {
    pub(crate) fn new(root: &'a Path, policy: RetentionPolicy) -> Self {
        Self {
            root,
            policy,
            run_dir: None,
            used_stems: HashSet::new(),
        }
    }

    /// Write every capture of a failed gate and return the absolute paths
    /// written, then prune the tree. Empty when nothing was captured or the
    /// write failed.
    pub(crate) fn write_failed_gate(
        &mut self,
        gate_name: &str,
        runs: &[(RunRole, &CommandCapture)],
    ) -> Vec<String> {
        let paths = match self.try_write(gate_name, runs) {
            Ok(paths) => paths,
            Err(error) => {
                tracing::warn!(gate = %gate_name, root = %self.root.display(), %error, "could not write failed gate logs");
                return Vec::new();
            }
        };
        if let Some(keep) = self.run_dir.as_deref().and_then(Path::file_name) {
            let keep = keep.to_string_lossy();
            if let Err(error) = prune_gate_logs(self.root, &keep, SystemTime::now(), &self.policy) {
                tracing::warn!(root = %self.root.display(), %error, "could not prune gate logs");
            }
        }
        paths
    }

    fn try_write(
        &mut self,
        gate_name: &str,
        runs: &[(RunRole, &CommandCapture)],
    ) -> std::io::Result<Vec<String>> {
        let stem = self.unique_stem(gate_name);
        let files = log_files(&stem, runs, GATE_LOG_STREAM_CAP_BYTES);
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let dir = self.run_dir()?;
        let mut paths = Vec::with_capacity(files.len());
        for (name, content) in files {
            let path = dir.join(name);
            std::fs::write(&path, content.as_bytes())?;
            let path = std::path::absolute(&path).unwrap_or(path);
            paths.push(path.display().to_string());
        }
        Ok(paths)
    }

    fn run_dir(&mut self) -> std::io::Result<PathBuf> {
        if let Some(dir) = &self.run_dir {
            return Ok(dir.clone());
        }
        let dir = self.root.join(run_dir_name(chrono::Utc::now(), uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        self.run_dir = Some(dir.clone());
        Ok(dir)
    }

    /// The sanitized gate name, made unique within this run so two gates whose
    /// names sanitize alike never overwrite each other's logs.
    fn unique_stem(&mut self, gate_name: &str) -> String {
        let base = sanitize_gate_name(gate_name);
        let mut stem = base.clone();
        let mut n = 2;
        while !self.used_stems.insert(stem.clone()) {
            stem = format!("{base}-{n}");
            n += 1;
        }
        stem
    }
}

/// Apply `policy` to the run directories directly under `root`, never
/// deleting `keep`.
///
/// Only real directories that are direct children of `root` are candidates:
/// symlinks and files are left alone, so nothing outside `root` can be
/// reached. A directory that cannot be measured or removed is skipped with a
/// warning.
pub(crate) fn prune_gate_logs(
    root: &Path,
    keep: &str,
    now: SystemTime,
    policy: &RetentionPolicy,
) -> std::io::Result<()> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let measured = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .and_then(|modified| Ok((modified, tree_bytes(&entry.path())?)));
        match measured {
            Ok((modified, bytes)) => entries.push(RunDir {
                name,
                modified,
                bytes,
            }),
            Err(error) => {
                tracing::warn!(dir = %entry.path().display(), %error, "could not measure gate-log directory");
            }
        }
    }
    for name in select_for_pruning(&entries, now, policy, keep) {
        let path = root.join(&name);
        if let Err(error) = std::fs::remove_dir_all(&path) {
            tracing::warn!(dir = %path.display(), %error, "could not prune gate-log directory");
        }
    }
    Ok(())
}

/// Total size of the files under `path`, without following symlinks.
fn tree_bytes(path: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let meta = std::fs::symlink_metadata(entry.path())?;
        total += if meta.is_dir() {
            tree_bytes(&entry.path())?
        } else {
            meta.len()
        };
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn run_dir(name: &str, age_days: u64, bytes: u64, now: SystemTime) -> RunDir {
        RunDir {
            name: name.to_string(),
            modified: now - DAY * u32::try_from(age_days).unwrap(),
            bytes,
        }
    }

    #[test]
    fn a_small_stream_is_kept_whole() {
        assert_eq!(cap_stream("all of it", 100), "all of it");
    }

    #[test]
    fn a_large_stream_keeps_its_tail_behind_a_note() {
        let text = format!("{}END", "x".repeat(1_000));
        let capped = cap_stream(&text, 10);
        let (note, tail) = capped.split_once('\n').unwrap();
        assert_eq!(note, "[foundry: output truncated; kept the last 10 of 1003 bytes]");
        assert_eq!(tail, "xxxxxxxEND");
    }

    #[test]
    fn the_stream_cap_respects_char_boundaries() {
        let text = "é".repeat(100);
        let capped = cap_stream(&text, 5);
        assert!(capped.ends_with("éé"));
        assert!(capped.contains("kept the last 4 of 200 bytes"));
    }

    #[test]
    fn gate_names_become_safe_file_stems() {
        assert_eq!(sanitize_gate_name("test"), "test");
        assert_eq!(sanitize_gate_name("cargo test --workspace"), "cargo_test_--workspace");
        assert_eq!(sanitize_gate_name("../../etc"), "______etc");
        assert_eq!(sanitize_gate_name(""), "gate");
        assert_eq!(sanitize_gate_name(&"a".repeat(100)).len(), 64);
    }

    #[test]
    fn run_directory_names_sort_chronologically() {
        use chrono::TimeZone as _;
        let id = uuid::Uuid::nil();
        let earlier =
            run_dir_name(chrono::Utc.with_ymd_and_hms(2026, 9, 30, 23, 0, 0).unwrap(), id);
        let later =
            run_dir_name(chrono::Utc.with_ymd_and_hms(2026, 10, 16, 22, 15, 0).unwrap(), id);
        assert!(later.starts_with("20261016T221500Z-"));
        assert!(earlier < later);
    }

    #[test]
    fn log_files_are_named_by_gate_role_and_stream() {
        let initial = CommandCapture {
            stdout: "out".into(),
            stderr: "err".into(),
        };
        let fix = CommandCapture {
            stdout: String::new(),
            stderr: "fix err".into(),
        };
        let runs = [
            (RunRole::Initial, &initial),
            (RunRole::Fix, &fix),
            (RunRole::Recheck, &initial),
        ];

        let names: Vec<String> =
            log_files("lint", &runs, 100).into_iter().map(|(name, _)| name).collect();

        assert_eq!(
            names,
            vec![
                "lint.stdout.log",
                "lint.stderr.log",
                "lint.fix.stderr.log",
                "lint.recheck.stdout.log",
                "lint.recheck.stderr.log",
            ]
        );
    }

    #[test]
    fn directories_older_than_the_max_age_are_selected() {
        let now = SystemTime::now();
        let entries = [
            run_dir("20260901T000000Z-a", 20, 10, now),
            run_dir("20261010T000000Z-b", 6, 10, now),
            run_dir("20261001T000000Z-c", 15, 10, now),
        ];

        let selected = select_for_pruning(&entries, now, &RetentionPolicy::default(), "none");

        assert_eq!(selected, vec!["20260901T000000Z-a", "20261001T000000Z-c"]);
    }

    #[test]
    fn the_size_cap_deletes_oldest_first_until_under() {
        let now = SystemTime::now();
        let policy = RetentionPolicy {
            max_age: GATE_LOG_MAX_AGE,
            max_total_bytes: 250,
        };
        let entries = [
            run_dir("20261003T000000Z-c", 1, 100, now),
            run_dir("20261001T000000Z-a", 1, 100, now),
            run_dir("20261002T000000Z-b", 1, 100, now),
            run_dir("20261004T000000Z-d", 1, 100, now),
        ];

        let selected = select_for_pruning(&entries, now, &policy, "none");

        assert_eq!(selected, vec!["20261001T000000Z-a", "20261002T000000Z-b"]);
    }

    #[test]
    fn the_directory_just_written_is_never_selected() {
        let now = SystemTime::now();
        let policy = RetentionPolicy {
            max_age: Duration::ZERO,
            max_total_bytes: 0,
        };
        let entries = [
            run_dir("20261001T000000Z-a", 1, 100, now),
            run_dir("20261002T000000Z-b", 1, 100, now),
        ];

        let selected = select_for_pruning(&entries, now, &policy, "20261001T000000Z-a");

        assert_eq!(selected, vec!["20261002T000000Z-b"]);
    }

    #[test]
    fn nothing_is_selected_inside_the_limits() {
        let now = SystemTime::now();
        let entries = [run_dir("20261001T000000Z-a", 1, 100, now)];
        let pruned = select_for_pruning(&entries, now, &RetentionPolicy::default(), "x");
        assert!(pruned.is_empty(), "{pruned:?}");
    }

    fn make_run(root: &Path, name: &str, bytes: usize) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("gate.stdout.log"), "x".repeat(bytes)).unwrap();
    }

    #[test]
    fn prune_removes_expired_run_directories() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("gate-logs");
        make_run(&root, "20261001T000000Z-old", 10);
        make_run(&root, "20261016T000000Z-new", 10);
        let later = SystemTime::now() + DAY * 15;

        prune_gate_logs(&root, "20261016T000000Z-new", later, &RetentionPolicy::default()).unwrap();

        assert!(!root.join("20261001T000000Z-old").exists());
        assert!(root.join("20261016T000000Z-new").exists(), "the kept directory survives");
    }

    #[test]
    fn prune_enforces_the_size_cap_oldest_first() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("gate-logs");
        make_run(&root, "20261001T000000Z-a", 100);
        make_run(&root, "20261002T000000Z-b", 100);
        make_run(&root, "20261003T000000Z-c", 100);
        let policy = RetentionPolicy {
            max_age: GATE_LOG_MAX_AGE,
            max_total_bytes: 150,
        };

        prune_gate_logs(&root, "20261003T000000Z-c", SystemTime::now(), &policy).unwrap();

        assert!(!root.join("20261001T000000Z-a").exists());
        assert!(!root.join("20261002T000000Z-b").exists());
        assert!(root.join("20261003T000000Z-c").exists());
    }

    #[cfg(unix)]
    #[test]
    fn prune_never_deletes_outside_the_gate_log_root() {
        let home = tempfile::tempdir().unwrap();
        let evidence = home.path().join("evidence");
        let root = evidence.join("gate-logs");
        make_run(&root, "20261001T000000Z-a", 100);
        make_run(&evidence, "trace-1", 100);
        std::os::unix::fs::symlink(evidence.join("trace-1"), root.join("20260101T000000Z-link"))
            .unwrap();
        std::fs::write(root.join("20260101T000000Z-file"), "x").unwrap();
        let policy = RetentionPolicy {
            max_age: Duration::ZERO,
            max_total_bytes: 0,
        };

        prune_gate_logs(&root, "none", SystemTime::now() + DAY, &policy).unwrap();

        assert!(!root.join("20261001T000000Z-a").exists());
        assert!(evidence.join("trace-1").join("gate.stdout.log").exists());
        assert!(root.join("20260101T000000Z-link").exists(), "symlinks are not candidates");
        assert!(root.join("20260101T000000Z-file").exists(), "files are not candidates");
    }

    #[test]
    fn a_missing_root_is_an_error_not_a_panic() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("absent");
        assert!(
            prune_gate_logs(&root, "x", SystemTime::now(), &RetentionPolicy::default()).is_err()
        );
    }

    #[test]
    fn same_named_gates_get_distinct_stems() {
        let home = tempfile::tempdir().unwrap();
        let mut writer = GateLogWriter::new(home.path(), RetentionPolicy::default());
        assert_eq!(writer.unique_stem("unit test"), "unit_test");
        assert_eq!(writer.unique_stem("unit:test"), "unit_test-2");
        assert_eq!(writer.unique_stem("unit test"), "unit_test-3");
    }

    #[test]
    fn a_write_failure_yields_no_paths() {
        let home = tempfile::tempdir().unwrap();
        let blocker = home.path().join("not-a-dir");
        std::fs::write(&blocker, "file in the way").unwrap();
        let mut writer = GateLogWriter::new(&blocker, RetentionPolicy::default());
        let capture = CommandCapture {
            stdout: "out".into(),
            stderr: String::new(),
        };

        let paths = writer.write_failed_gate("test", &[(RunRole::Initial, &capture)]);

        assert!(paths.is_empty(), "{paths:?}");
    }
}
