//! Age-based pruning of the evidence store.
//!
//! Finalize Task writes one directory per run under the evidence root,
//! `{evidence_dir}/{trace_id}/{attempt}/`, and nothing used to delete them:
//! on the ops host 2,893 trace directories had built up, 1,338 of them older
//! than 30 days. The daemon now deletes a trace directory once it has not
//! been written for `FOUNDRY_EVIDENCE_RETENTION_DAYS` (default 30; `0` turns
//! pruning off).
//!
//! What is never deleted:
//!
//! - `gate-logs/`. Failed-gate logs keep their own, tighter policy (14 days,
//!   1 GiB in total), applied by the gate runner after every write; see
//!   [`crate::gate_runner::RetentionPolicy`].
//! - A trace that work in the ledger still holds: an item that is waiting to
//!   start, running, or still finishing its cleanup. A resumed task reuses
//!   its original trace, so its old evidence stays while the resumption is in
//!   flight. When the ledger cannot be read, nothing is deleted.
//! - Anything that is not a real directory directly under the root.
//!
//! A trace's age is that of its newest write: the latest modification time
//! of the trace directory and of each attempt directory in it. A new attempt
//! makes the trace young again. The age is checked a second time just before
//! deletion, so a trace written during the prune survives.
//!
//! Task results keep naming the deleted directories (`proof_evidence`,
//! `uncommitted_artifacts`); after the retention period those paths no
//! longer exist. The work item, its branch or bundle, and the trace record
//! remain.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

use foundry_sdk::work_item::{WorkItem, WorkItemStore};

/// Environment variable that sets the retention period, in days.
pub const EVIDENCE_RETENTION_DAYS_ENV: &str = "FOUNDRY_EVIDENCE_RETENTION_DAYS";

/// Default retention period, in days.
pub const DEFAULT_EVIDENCE_RETENTION_DAYS: u64 = 30;

/// How often the daemon prunes. The first prune runs at start-up.
pub const EVIDENCE_PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// The gate-log tree inside the evidence root, which this prune leaves alone.
pub const GATE_LOGS_DIR: &str = "gate-logs";

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// The retention period from `FOUNDRY_EVIDENCE_RETENTION_DAYS`, or `None`
/// when pruning is off.
#[must_use]
pub fn retention_from_env() -> Option<Duration> {
    parse_retention(std::env::var(EVIDENCE_RETENTION_DAYS_ENV).ok().as_deref())
}

/// Unset or unparseable means the default, like the disk thresholds; `0`
/// turns pruning off.
fn parse_retention(value: Option<&str>) -> Option<Duration> {
    let days = value
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(DEFAULT_EVIDENCE_RETENTION_DAYS, u64::from);
    (days > 0).then(|| DAY * u32::try_from(days).unwrap_or(u32::MAX))
}

/// A trace directory as the prune sees it.
#[derive(Debug, Clone)]
pub(crate) struct TraceDir {
    pub(crate) name: String,
    /// The newest modification time of the directory and its attempts.
    pub(crate) newest_write: SystemTime,
}

fn is_expired(newest_write: SystemTime, now: SystemTime, max_age: Duration) -> bool {
    // A write in the future (clock skew) is young, not expired.
    now.duration_since(newest_write).is_ok_and(|age| age > max_age)
}

/// The trace directories a prune deletes: older than `max_age`, not the
/// gate-log tree, and not held by in-flight work.
pub(crate) fn select_expired(
    entries: &[TraceDir],
    now: SystemTime,
    max_age: Duration,
    in_flight: &HashSet<String>,
) -> Vec<String> {
    let mut selected: Vec<String> = entries
        .iter()
        .filter(|entry| entry.name != GATE_LOGS_DIR)
        .filter(|entry| !in_flight.contains(&entry.name))
        .filter(|entry| is_expired(entry.newest_write, now, max_age))
        .map(|entry| entry.name.clone())
        .collect();
    selected.sort();
    selected
}

/// Every trace id that ledger work still holds: items waiting to start, and
/// items occupying a slot (running, or settled with cleanup unfinished).
/// Finalize Task names its evidence by the trigger's trace id, or the event
/// id when it has none, so both ids of each admitted root count.
#[must_use]
pub fn in_flight_traces(store: &WorkItemStore) -> HashSet<String> {
    store
        .items
        .iter()
        .filter(|item| item.state.is_waiting() || item.occupies_slot())
        .flat_map(held_ids)
        .collect()
}

fn held_ids(item: &WorkItem) -> Vec<String> {
    let mut ids: Vec<String> = item.trace_id.iter().cloned().collect();
    for root in [&item.pending_root, &item.started_root].into_iter().flatten() {
        ids.extend(root.trace_id.iter().cloned());
        ids.push(root.id.clone());
    }
    ids
}

/// What one prune did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PruneReport {
    /// Trace directories deleted.
    pub removed: usize,
    /// Bytes they held.
    pub bytes: u64,
}

/// Prune the evidence store at `root`, protecting the traces the ledger at
/// `ledger_path` still holds. A missing root is an empty store.
///
/// # Errors
///
/// When the ledger cannot be read (in-flight work is then unknown, so
/// nothing is deleted) or the root cannot be listed. A single directory that
/// cannot be measured or removed is skipped with a warning.
pub fn prune_evidence_store(
    root: &Path,
    ledger_path: &Path,
    max_age: Duration,
    now: SystemTime,
) -> anyhow::Result<PruneReport> {
    let store = WorkItemStore::load(ledger_path).map_err(|error| {
        anyhow::anyhow!("work-item ledger unreadable; evidence left in place: {error}")
    })?;
    let in_flight = in_flight_traces(&store);
    Ok(prune_evidence(root, now, max_age, &in_flight)?)
}

/// Apply the retention rules to the trace directories directly under `root`.
pub(crate) fn prune_evidence(
    root: &Path,
    now: SystemTime,
    max_age: Duration,
    in_flight: &HashSet<String>,
) -> std::io::Result<PruneReport> {
    let listing = match std::fs::read_dir(root) {
        Ok(listing) => listing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PruneReport::default());
        }
        Err(error) => return Err(error),
    };
    let mut entries = Vec::new();
    for entry in listing {
        let entry = entry?;
        // `DirEntry::file_type` does not follow symlinks: a link is skipped.
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        match newest_write(&entry.path()) {
            Ok(newest_write) => entries.push(TraceDir { name, newest_write }),
            Err(error) => {
                tracing::warn!(dir = %entry.path().display(), %error, "could not measure evidence directory");
            }
        }
    }
    let mut report = PruneReport::default();
    for name in select_expired(&entries, now, max_age, in_flight) {
        let path = root.join(&name);
        // Re-check just before deleting: a finalization may have written here
        // since the listing.
        if !newest_write(&path).is_ok_and(|newest| is_expired(newest, now, max_age)) {
            continue;
        }
        let bytes = crate::gate_runner::tree_bytes(&path).unwrap_or(0);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                report.removed += 1;
                report.bytes += bytes;
            }
            Err(error) => {
                tracing::warn!(dir = %path.display(), %error, "could not prune evidence directory");
            }
        }
    }
    Ok(report)
}

/// The newest modification time of `trace` and of the directories directly
/// in it (its attempts).
fn newest_write(trace: &Path) -> std::io::Result<SystemTime> {
    let mut newest = std::fs::symlink_metadata(trace)?.modified()?;
    for entry in std::fs::read_dir(trace)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            newest = newest.max(entry.metadata()?.modified()?);
        }
    }
    Ok(newest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_sdk::time::UtcDateTime;
    use foundry_sdk::work_item::{WorkItemKind, WorkItemSpec, WorkLane};

    const NOW_SECS: u64 = 2_000_000_000;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_SECS)
    }

    fn trace(name: &str, age_days: u32) -> TraceDir {
        TraceDir {
            name: name.to_string(),
            newest_write: now() - DAY * age_days,
        }
    }

    fn item(trace_id: &str) -> WorkItem {
        WorkItem::submitted(
            WorkItemSpec {
                project: "p".to_string(),
                objective: "o".to_string(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "test".to_string(),
                trace_id: Some(trace_id.to_string()),
            },
            UtcDateTime::from_timestamp(1_900_000_000, 0).unwrap(),
        )
    }

    #[test]
    fn retention_defaults_to_thirty_days_and_zero_turns_it_off() {
        assert_eq!(parse_retention(None), Some(DAY * 30));
        assert_eq!(parse_retention(Some("not a number")), Some(DAY * 30));
        assert_eq!(parse_retention(Some(" 7 ")), Some(DAY * 7));
        assert_eq!(parse_retention(Some("0")), None);
    }

    #[test]
    fn only_traces_older_than_the_retention_are_selected() {
        let entries = [trace("old", 31), trace("young", 29), trace("older", 400)];
        assert_eq!(select_expired(&entries, now(), DAY * 30, &HashSet::new()), ["old", "older"]);
    }

    #[test]
    fn gate_logs_and_in_flight_traces_are_never_selected() {
        let entries = [
            trace(GATE_LOGS_DIR, 90),
            trace("resumed", 90),
            trace("done", 90),
        ];
        let in_flight = HashSet::from(["resumed".to_string()]);
        assert_eq!(select_expired(&entries, now(), DAY * 30, &in_flight), ["done"]);
    }

    #[test]
    fn a_write_in_the_future_is_young() {
        let entries = [TraceDir {
            name: "skewed".to_string(),
            newest_write: now() + DAY,
        }];
        assert!(select_expired(&entries, now(), DAY * 30, &HashSet::new()).is_empty());
    }

    #[test]
    fn waiting_running_and_unfinished_items_hold_their_traces() {
        let waiting = item("t-waiting");
        let mut running = item("t-running");
        running.start(UtcDateTime::from_timestamp(1_900_000_100, 0).unwrap());
        let mut settled = item("t-settled");
        settled.start(UtcDateTime::from_timestamp(1_900_000_100, 0).unwrap());
        settled.execution_completed = true;
        settled.settle_failed("boom", UtcDateTime::from_timestamp(1_900_000_200, 0).unwrap());
        let store = WorkItemStore {
            items: vec![waiting, running, settled],
            ..WorkItemStore::default()
        };
        let held = in_flight_traces(&store);
        assert!(held.contains("t-waiting"));
        assert!(held.contains("t-running"));
        assert!(!held.contains("t-settled"));
    }

    /// The filesystem shell: expired traces go, with their bytes counted;
    /// gate logs, held traces, young traces and stray files stay. Ages are
    /// measured from the directories' own modification times, so no clock is
    /// read.
    #[test]
    fn prune_deletes_expired_trace_directories_only() {
        let root = tempfile::tempdir().unwrap();
        for name in ["expired", "held", GATE_LOGS_DIR] {
            let attempt = root.path().join(name).join("attempt");
            std::fs::create_dir_all(&attempt).unwrap();
            std::fs::write(attempt.join("proof.json"), "{}").unwrap();
        }
        std::fs::write(root.path().join("stray-file"), "x").unwrap();
        let written = newest_write(&root.path().join("expired")).unwrap();
        let later = written + DAY * 31;

        let in_flight = HashSet::from(["held".to_string()]);
        let report = prune_evidence(root.path(), later, DAY * 30, &in_flight).unwrap();

        assert_eq!(
            report,
            PruneReport {
                removed: 1,
                bytes: 2
            }
        );
        assert!(!root.path().join("expired").exists());
        assert!(root.path().join("held").exists());
        assert!(root.path().join(GATE_LOGS_DIR).exists());
        assert!(root.path().join("stray-file").exists());

        // Within the retention nothing goes.
        let report = prune_evidence(root.path(), written, DAY * 30, &HashSet::new()).unwrap();
        assert_eq!(report, PruneReport::default());
        assert!(root.path().join("held").exists());
    }

    #[test]
    fn a_missing_evidence_root_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let report =
            prune_evidence(&dir.path().join("absent"), now(), DAY, &HashSet::new()).unwrap();
        assert_eq!(report, PruneReport::default());
    }

    #[test]
    fn an_unreadable_ledger_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("evidence");
        std::fs::create_dir_all(root.join("expired")).unwrap();
        let ledger = dir.path().join("work-items.json");
        std::fs::write(&ledger, "not json").unwrap();
        let later = newest_write(&root.join("expired")).unwrap() + DAY * 365;

        let error = prune_evidence_store(&root, &ledger, DAY, later).unwrap_err();

        assert!(error.to_string().contains("ledger unreadable"), "{error}");
        assert!(root.join("expired").exists());
    }

    #[test]
    fn the_ledger_protects_a_trace_with_work_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("evidence");
        for name in ["t-queued", "t-old"] {
            std::fs::create_dir_all(root.join(name).join("attempt")).unwrap();
        }
        let ledger = dir.path().join("work-items.json");
        WorkItemStore {
            items: vec![item("t-queued")],
            ..WorkItemStore::default()
        }
        .save(&ledger)
        .unwrap();
        let later = newest_write(&root.join("t-old")).unwrap() + DAY * 365;

        let report = prune_evidence_store(&root, &ledger, DAY, later).unwrap();

        assert_eq!(report.removed, 1);
        assert!(root.join("t-queued").exists());
        assert!(!root.join("t-old").exists());
    }
}
