//! Read one work item's `work_item_*` events back out of the durable event log.
//!
//! The ledger ([`crate::work_item`]) holds each item's current record; the
//! event log (`{FOUNDRY_EVENTS_DIR}/YYYY-MM.jsonl`, see
//! [`crate::paths::events_dir`]) holds the history of how it got there — one
//! `work_item_submitted`, `work_item_started`, `work_item_settled` or
//! `work_item_cancelled` event per transition, each carrying a
//! [`WorkItemEventPayload`] naming the item.
//!
//! [`read_work_item_events`] is the single selection rule both the daemon's
//! `ListWorkItemEvents` RPC and `foundry queue show --offline` apply, so the two
//! differ only in transport:
//!
//! - **Selection is by exact payload `item_id`** — never by trace id and never
//!   by project. A maintenance run's `maintenance`, `remediation` and `release`
//!   items share one trace and one project; only the payload tells them apart.
//! - **Every monthly file is read**, whatever its age. A reader bounded to a
//!   recent window would report a clean "no events" for an older item, which
//!   is a non-result dressed as a result.
//! - **Order is chronological**: `occurred_at` ascending, ties in log order
//!   (file name order, then line order).
//! - **A missing events directory is an empty history**, not an error. An I/O
//!   fault on an existing directory or file is an error, so it can never render
//!   as "no events".
//! - **A malformed line is absorbed** (see the `// Best-effort:` note in the
//!   reader): it is skipped with a `tracing::warn!`, and every well-formed line
//!   around it is still read.
//!
//! This module only reads. It never writes the log and never takes the ledger
//! write gate.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::event::{Event, EventType};
use crate::payload::WorkItemEventPayload;

/// One `work_item_*` event from the durable log, with its payload parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemEventRecord {
    /// The event's own id.
    pub event_id: String,
    /// Which of the four `work_item_*` event types this is.
    pub event_type: EventType,
    /// When the event occurred.
    pub occurred_at: DateTime<Utc>,
    /// The workflow trace the event was emitted on, when it carries one.
    pub trace_id: Option<String>,
    /// The item as it stood at this event.
    pub payload: WorkItemEventPayload,
}

/// A fault reading the event log that must not be reported as "no events".
#[derive(Debug, thiserror::Error)]
#[error("could not read the event log at {path}: {source}")]
pub struct EventLogReadError {
    /// The directory or file that could not be read.
    pub path: PathBuf,
    /// The underlying I/O error.
    #[source]
    pub source: std::io::Error,
}

/// Whether `event_type` is one of the four work-item lifecycle events.
#[must_use]
pub fn is_work_item_event(event_type: &EventType) -> bool {
    matches!(
        event_type,
        EventType::WorkItemSubmitted
            | EventType::WorkItemStarted
            | EventType::WorkItemSettled
            | EventType::WorkItemCancelled
    )
}

/// Every `work_item_*` event whose payload names `item_id`, from every
/// `*.jsonl` file in `events_dir`, in chronological order.
///
/// # Errors
///
/// Returns [`EventLogReadError`] when `events_dir` exists but cannot be listed,
/// or when one of its log files cannot be read. A missing `events_dir` is not an
/// error: it yields an empty list.
pub fn read_work_item_events(
    events_dir: &Path,
    item_id: &str,
) -> Result<Vec<WorkItemEventRecord>, EventLogReadError> {
    let mut records = Vec::new();
    for path in log_files(events_dir)? {
        let bytes = std::fs::read(&path).map_err(|source| EventLogReadError {
            path: path.clone(),
            source,
        })?;
        select_from_log(&path, &bytes, item_id, &mut records);
    }
    // A stable sort keeps log order for events that share a timestamp.
    records.sort_by_key(|record| record.occurred_at);
    Ok(records)
}

/// The log files in `events_dir`, in file-name order (`YYYY-MM.jsonl` sorts
/// chronologically). A missing directory has none.
fn log_files(events_dir: &Path) -> Result<Vec<PathBuf>, EventLogReadError> {
    let entries = match std::fs::read_dir(events_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(EventLogReadError {
                path: events_dir.to_path_buf(),
                source,
            });
        }
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| EventLogReadError {
            path: events_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Append to `out`, in line order, every event in one log file that belongs to
/// `item_id`.
fn select_from_log(path: &Path, bytes: &[u8], item_id: &str, out: &mut Vec<WorkItemEventRecord>) {
    for (index, raw) in bytes.split(|byte| *byte == b'\n').enumerate() {
        // A line that never mentions the id cannot be one of this item's
        // events, so it is not parsed at all.
        if !String::from_utf8_lossy(raw).contains(item_id) {
            continue;
        }
        let parsed = std::str::from_utf8(raw)
            .map_err(|err| err.to_string())
            .and_then(|line| parse_record(line.trim()).map_err(|err| err.to_string()));
        match parsed {
            Ok(Some(record)) if record.payload.item_id == item_id => out.push(record),
            Ok(_) => {}
            Err(error) => {
                // Best-effort: one malformed line (non-JSON, not UTF-8, or two
                // events a writer glued together) must neither fail the read
                // nor hide this item's well-formed events on the other lines.
                // It is skipped and reported, never mistaken for a clean line.
                tracing::warn!(
                    path = %path.display(),
                    line = index + 1,
                    %error,
                    "skipping malformed event log line while reading work-item events"
                );
            }
        }
    }
}

/// Parse one log line. `Ok(None)` is a well-formed event that is not a
/// work-item event.
fn parse_record(line: &str) -> Result<Option<WorkItemEventRecord>, serde_json::Error> {
    let event: Event = serde_json::from_str(line)?;
    if !is_work_item_event(&event.event_type) {
        return Ok(None);
    }
    let payload: WorkItemEventPayload = serde_json::from_value(event.payload)?;
    Ok(Some(WorkItemEventRecord {
        event_id: event.id,
        event_type: event.event_type,
        occurred_at: event.occurred_at,
        trace_id: event.trace_id,
        payload,
    }))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;
    use crate::throttle::Throttle;
    use crate::work_item::{WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkLane};

    fn item(id: &str) -> WorkItem {
        let mut item = WorkItem::submitted(
            WorkItemSpec {
                project: "alpha".to_string(),
                objective: "do the thing".to_string(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "test".to_string(),
                trace_id: Some("a".repeat(32)),
            },
            Utc::now(),
        );
        item.id = id.to_string();
        item
    }

    fn event(
        event_type: EventType,
        id: &str,
        state: WorkItemState,
        occurred: DateTime<Utc>,
    ) -> Event {
        let mut subject = item(id);
        subject.state = state;
        let mut event = Event::new(
            event_type,
            "alpha".to_string(),
            Throttle::Full,
            Event::serialize_payload(&WorkItemEventPayload::from_item(&subject))
                .expect("payload serializes"),
        );
        event.occurred_at = occurred;
        event.recorded_at = occurred;
        event.trace_id = Some("a".repeat(32));
        event
    }

    fn at(month: u32, second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, month, 1, 0, 0, second).single().expect("valid date")
    }

    fn line(event: &Event) -> String {
        serde_json::to_string(event).expect("event serializes")
    }

    fn event_ids(records: &[WorkItemEventRecord]) -> Vec<&str> {
        records.iter().map(|record| record.event_id.as_str()).collect()
    }

    #[test]
    fn a_missing_events_directory_is_an_empty_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let records = read_work_item_events(&dir.path().join("absent"), "wi_a").expect("no fault");
        assert!(records.is_empty());
    }

    #[test]
    fn events_are_selected_by_payload_item_id_and_ordered_chronologically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settled = event(EventType::WorkItemSettled, "wi_a", WorkItemState::Landed, at(2, 3));
        let submitted =
            event(EventType::WorkItemSubmitted, "wi_a", WorkItemState::Submitted, at(1, 1));
        let other = event(EventType::WorkItemSubmitted, "wi_b", WorkItemState::Submitted, at(1, 2));
        std::fs::write(dir.path().join("2026-02.jsonl"), format!("{}\n", line(&settled)))
            .expect("write");
        std::fs::write(
            dir.path().join("2026-01.jsonl"),
            format!("{}\n{}\n", line(&other), line(&submitted)),
        )
        .expect("write");

        let records = read_work_item_events(dir.path(), "wi_a").expect("read");
        assert_eq!(event_ids(&records), vec![submitted.id.as_str(), settled.id.as_str()]);
        assert_eq!(records[1].payload.state, WorkItemState::Landed);
    }

    #[test]
    fn events_sharing_a_timestamp_keep_log_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = event(EventType::WorkItemStarted, "wi_a", WorkItemState::Running, at(1, 1));
        let second = event(EventType::WorkItemSettled, "wi_a", WorkItemState::Failed, at(1, 1));
        std::fs::write(
            dir.path().join("2026-01.jsonl"),
            format!("{}\n{}\n", line(&first), line(&second)),
        )
        .expect("write");

        let records = read_work_item_events(dir.path(), "wi_a").expect("read");
        assert_eq!(event_ids(&records), vec![first.id.as_str(), second.id.as_str()]);
    }

    #[test]
    fn malformed_and_glued_lines_are_skipped_without_losing_other_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = event(EventType::WorkItemSubmitted, "wi_a", WorkItemState::Submitted, at(1, 1));
        let glued_a = event(EventType::WorkItemStarted, "wi_a", WorkItemState::Running, at(1, 2));
        let glued_b = event(EventType::WorkItemSettled, "wi_a", WorkItemState::Landed, at(1, 3));
        let after = event(EventType::WorkItemSettled, "wi_a", WorkItemState::Landed, at(1, 4));
        let content = format!(
            "not json at all wi_a\n{}\n{}{}\n{}\n",
            line(&good),
            line(&glued_a),
            line(&glued_b),
            line(&after)
        );
        std::fs::write(dir.path().join("2026-01.jsonl"), content).expect("write");

        let records = read_work_item_events(dir.path(), "wi_a").expect("read");
        assert_eq!(event_ids(&records), vec![good.id.as_str(), after.id.as_str()]);
    }

    #[test]
    fn a_non_work_item_event_mentioning_the_id_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut unrelated = Event::new(
            EventType::GreetingRequested,
            "alpha".to_string(),
            Throttle::Full,
            serde_json::json!({"item_id": "wi_a"}),
        );
        unrelated.occurred_at = at(1, 1);
        std::fs::write(dir.path().join("2026-01.jsonl"), format!("{}\n", line(&unrelated)))
            .expect("write");

        assert!(read_work_item_events(dir.path(), "wi_a").expect("read").is_empty());
    }

    #[test]
    fn an_id_that_is_a_prefix_of_another_does_not_match_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let longer =
            event(EventType::WorkItemSubmitted, "wi_abc", WorkItemState::Submitted, at(1, 1));
        std::fs::write(dir.path().join("2026-01.jsonl"), format!("{}\n", line(&longer)))
            .expect("write");

        assert!(read_work_item_events(dir.path(), "wi_ab").expect("read").is_empty());
    }

    #[test]
    fn non_jsonl_files_in_the_directory_are_not_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stray = event(EventType::WorkItemSubmitted, "wi_a", WorkItemState::Submitted, at(1, 1));
        std::fs::write(dir.path().join("notes.txt"), format!("{}\n", line(&stray))).expect("write");

        assert!(read_work_item_events(dir.path(), "wi_a").expect("read").is_empty());
    }

    #[test]
    fn an_events_path_that_is_a_file_is_an_error_not_an_empty_history() {
        let file = tempfile::NamedTempFile::new().expect("tempfile");
        let err = read_work_item_events(file.path(), "wi_a")
            .expect_err("an unlistable events directory must fail");
        assert_eq!(err.path, file.path());
    }

    #[test]
    fn only_the_four_lifecycle_types_are_work_item_events() {
        assert!(is_work_item_event(&EventType::WorkItemSubmitted));
        assert!(is_work_item_event(&EventType::WorkItemStarted));
        assert!(is_work_item_event(&EventType::WorkItemSettled));
        assert!(is_work_item_event(&EventType::WorkItemCancelled));
        assert!(!is_work_item_event(&EventType::TaskRunCompleted));
    }
}
