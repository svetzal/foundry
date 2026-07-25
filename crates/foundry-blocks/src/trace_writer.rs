use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;

use foundry_sdk::trace::{ProcessResult, TraceIndex};

/// Writes and reads completed process results to disk as JSON files, organised
/// by date (`YYYY-MM-DD/{event_id}.json` under `base_dir`).
pub struct TraceWriter {
    base_dir: PathBuf,
}

impl TraceWriter {
    pub fn new(base_dir: &str) -> Self {
        Self {
            base_dir: PathBuf::from(base_dir),
        }
    }

    /// Persist `result` to `{base_dir}/{date}/{event_id}.json`.
    ///
    /// Creates the date directory if it does not yet exist.  Write failures
    /// are propagated to the caller; the caller may choose to log and ignore.
    pub fn write(&self, event_id: &str, result: &ProcessResult) -> Result<()> {
        let date = Utc::now().format("%Y-%m-%d").to_string();
        let dir = self.base_dir.join(&date);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{event_id}.json"));
        let json = serde_json::to_string_pretty(result)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Search all date subdirectories under `base_dir` for a file named
    /// `{event_id}.json` and deserialise it.  Returns `None` if not found or
    /// if deserialisation fails.
    pub fn read(&self, event_id: &str) -> Option<ProcessResult> {
        let filename = format!("{event_id}.json");
        // Best-effort: the base trace directory may not exist yet (no traces
        // written so far); treat that as "not found" rather than an error.
        let Ok(entries) = std::fs::read_dir(&self.base_dir) else {
            tracing::debug!(base_dir = %self.base_dir.display(), "trace base dir unreadable while looking up trace by event id");
            return None;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                let candidate = entry.path().join(&filename);
                if candidate.exists() {
                    // Best-effort: a trace file that fails to read or parse is
                    // treated as absent rather than failing the whole lookup.
                    match std::fs::read_to_string(&candidate) {
                        Ok(content) => match serde_json::from_str(&content) {
                            Ok(result) => return Some(result),
                            Err(err) => {
                                tracing::warn!(path = %candidate.display(), error = %err, "failed to parse trace file, treating as not found");
                            }
                        },
                        Err(err) => {
                            tracing::warn!(path = %candidate.display(), error = %err, "failed to read trace file, treating as not found");
                        }
                    }
                }
            }
        }
        None
    }

    /// List all traces stored under `{base_dir}/{date}/`.
    ///
    /// Returns an empty `Vec` when the directory does not exist or cannot be
    /// read.
    pub fn list_date(&self, date: &str, project_filter: Option<&str>) -> Vec<TraceIndex> {
        let dir = self.base_dir.join(date);
        Self::read_index_from_dir(&dir, project_filter)
    }

    /// Return the traces for the most recent `days` calendar days (today
    /// inclusive), newest first.  Days with no traces are omitted.
    pub fn list_recent(
        &self,
        days: usize,
        project_filter: Option<&str>,
    ) -> Vec<(String, Vec<TraceIndex>)> {
        let mut result = Vec::new();
        let today = Utc::now().date_naive();
        for offset in 0..days {
            let date = today - chrono::Duration::days(i64::try_from(offset).unwrap_or(0));
            let date_str = date.format("%Y-%m-%d").to_string();
            let indices = self.list_date(&date_str, project_filter);
            if !indices.is_empty() {
                result.push((date_str, indices));
            }
        }
        result
    }

    fn read_index_from_dir(dir: &std::path::Path, project_filter: Option<&str>) -> Vec<TraceIndex> {
        // Best-effort: a missing date directory just means no traces for that
        // day; callers treat an empty listing the same as "not found".
        let Ok(entries) = std::fs::read_dir(dir) else {
            return vec![];
        };
        let mut indices_with_time = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // Best-effort: a trace file that can't be read or parsed is
            // skipped so one corrupt/unreadable file doesn't fail the whole
            // listing; the remaining valid traces are still returned.
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "failed to read trace file, skipping");
                    continue;
                }
            };
            let result = match serde_json::from_str::<ProcessResult>(&content) {
                Ok(result) => result,
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "failed to parse trace file, skipping");
                    continue;
                }
            };
            // Derive event_id from the filename stem.
            let event_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            // Use the root event (first in the list) for the index fields.
            let (event_type, project) = result
                .events
                .first()
                .map(|e| (e.event_type.to_string(), e.project.clone()))
                .unwrap_or_default();
            if let Some(filter) = project_filter
                && project != filter
            {
                continue;
            }
            let success = result.is_success();
            let trace_id = result.events.first().and_then(|e| e.trace_id.clone());
            let occurred_at = result
                .events
                .first()
                .map(|event| event.occurred_at.to_rfc3339())
                .unwrap_or_default();
            indices_with_time.push((
                occurred_at,
                TraceIndex {
                    event_id,
                    event_type,
                    project,
                    success,
                    total_duration_ms: result.total_duration_ms,
                    trace_id,
                },
            ));
        }
        indices_with_time.sort_by(|(left_time, left), (right_time, right)| {
            right_time.cmp(left_time).then_with(|| left.event_id.cmp(&right.event_id))
        });
        indices_with_time.into_iter().map(|(_, index)| index).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::trace::BlockExecution;

    fn sample_result(event_type: EventType, project: &str) -> ProcessResult {
        let event =
            Event::new(event_type, project.to_string(), Throttle::Full, serde_json::json!({}));
        let event_id = event.id.clone();
        ProcessResult {
            events: vec![event],
            block_executions: vec![BlockExecution {
                block_name: "TestBlock".to_string(),
                trigger_event_id: event_id,
                success: true,
                summary: "ok".to_string(),
                emitted_event_ids: vec![],
                duration_ms: 42,
                raw_output: Some("stdout content".to_string()),
                exit_code: Some(0),
                trigger_payload: serde_json::json!({"key": "value"}),
                emitted_payloads: vec![serde_json::json!({"result": true})],
                audit_artifacts: vec![],
                span_id: None,
                parent_span_id: None,
            }],
            total_duration_ms: 100,
        }
    }

    #[test]
    fn write_and_read_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = TraceWriter::new(dir.path().to_str().unwrap());
        let result = sample_result(EventType::GreetingRequested, "test-project");

        writer.write("evt_abc123", &result).expect("write");
        let loaded = writer.read("evt_abc123").expect("read should find the file");

        assert_eq!(loaded.total_duration_ms, 100);
        assert_eq!(loaded.events.len(), 1);
        assert_eq!(loaded.block_executions[0].raw_output, Some("stdout content".to_string()));
        assert_eq!(loaded.block_executions[0].exit_code, Some(0));
    }

    #[test]
    fn read_returns_none_for_unknown_event_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = TraceWriter::new(dir.path().to_str().unwrap());
        assert!(writer.read("nonexistent").is_none());
    }

    #[test]
    fn list_date_returns_index_for_written_traces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = TraceWriter::new(dir.path().to_str().unwrap());
        let result = sample_result(EventType::GreetingRequested, "proj-a");

        writer.write("evt_111", &result).expect("write");

        let today = Utc::now().format("%Y-%m-%d").to_string();
        let indices = writer.list_date(&today, None);

        assert_eq!(indices.len(), 1);
        assert_eq!(indices[0].event_id, "evt_111");
        assert_eq!(indices[0].project, "proj-a");
        assert!(indices[0].success);
        assert_eq!(indices[0].total_duration_ms, 100);
    }

    #[test]
    fn list_date_skips_corrupt_file_and_returns_remaining_valid_traces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = TraceWriter::new(dir.path().to_str().unwrap());
        let result = sample_result(EventType::GreetingRequested, "proj-a");
        writer.write("evt_good", &result).expect("write");

        let today = Utc::now().format("%Y-%m-%d").to_string();
        let date_dir = dir.path().join(&today);
        std::fs::write(date_dir.join("evt_bad.json"), "not valid json").expect("write corrupt");

        let indices = writer.list_date(&today, None);

        assert_eq!(indices.len(), 1);
        assert_eq!(indices[0].event_id, "evt_good");
    }

    #[test]
    fn list_date_returns_empty_for_missing_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = TraceWriter::new(dir.path().to_str().unwrap());
        let indices = writer.list_date("1999-01-01", None);
        assert!(indices.is_empty());
    }

    #[test]
    fn list_recent_returns_days_with_traces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = TraceWriter::new(dir.path().to_str().unwrap());
        let result = sample_result(EventType::GreetingRequested, "proj-b");

        writer.write("evt_222", &result).expect("write");

        let recent = writer.list_recent(7, None);

        // Today should appear since we just wrote a trace.
        assert!(!recent.is_empty());
        let (date, indices) = &recent[0];
        let today = Utc::now().format("%Y-%m-%d").to_string();
        assert_eq!(date, &today);
        assert_eq!(indices[0].event_id, "evt_222");
    }
}
