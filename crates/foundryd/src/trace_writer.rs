use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;

use foundry_core::trace::ProcessResult;
#[cfg(test)]
use foundry_core::trace::TraceIndex;

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
        let Ok(entries) = std::fs::read_dir(&self.base_dir) else {
            return None;
        };
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let candidate = entry.path().join(&filename);
                if candidate.exists() {
                    if let Ok(content) = std::fs::read_to_string(&candidate) {
                        return serde_json::from_str(&content).ok();
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
impl TraceWriter {
    /// List all traces stored under `{base_dir}/{date}/`.
    ///
    /// Returns an empty `Vec` when the directory does not exist or cannot be
    /// read.
    pub fn list_date(&self, date: &str) -> Vec<TraceIndex> {
        let dir = self.base_dir.join(date);
        Self::read_index_from_dir(&dir)
    }

    /// Return the traces for the most recent `days` calendar days (today
    /// inclusive), newest first.  Days with no traces are omitted.
    pub fn list_recent(&self, days: usize) -> Vec<(String, Vec<TraceIndex>)> {
        let mut result = Vec::new();
        let today = Utc::now().date_naive();
        for offset in 0..days {
            let date = today - chrono::Duration::days(i64::try_from(offset).unwrap_or(0));
            let date_str = date.format("%Y-%m-%d").to_string();
            let indices = self.list_date(&date_str);
            if !indices.is_empty() {
                result.push((date_str, indices));
            }
        }
        result
    }

    fn read_index_from_dir(dir: &std::path::Path) -> Vec<TraceIndex> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return vec![];
        };
        let mut indices = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(result) = serde_json::from_str::<ProcessResult>(&content) else {
                continue;
            };
            // Derive event_id from the filename stem.
            let event_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            // Use the root event (first in the list) for the index fields.
            let (event_type, project) = result
                .events
                .first()
                .map(|e| (e.event_type.to_string(), e.project.clone()))
                .unwrap_or_default();
            let success = result.block_executions.iter().all(|b| b.success);
            indices.push(TraceIndex {
                event_id,
                event_type,
                project,
                success,
                total_duration_ms: result.total_duration_ms,
            });
        }
        indices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::event::{Event, EventType};
    use foundry_core::throttle::Throttle;
    use foundry_core::trace::BlockExecution;

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
            }],
            total_duration_ms: 100,
        }
    }

    #[test]
    fn write_and_read_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = TraceWriter::new(dir.path().to_str().unwrap());
        let result = sample_result(EventType::GreetRequested, "test-project");

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
        let result = sample_result(EventType::GreetRequested, "proj-a");

        writer.write("evt_111", &result).expect("write");

        let today = Utc::now().format("%Y-%m-%d").to_string();
        let indices = writer.list_date(&today);

        assert_eq!(indices.len(), 1);
        assert_eq!(indices[0].event_id, "evt_111");
        assert_eq!(indices[0].project, "proj-a");
        assert!(indices[0].success);
        assert_eq!(indices[0].total_duration_ms, 100);
    }

    #[test]
    fn list_date_returns_empty_for_missing_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = TraceWriter::new(dir.path().to_str().unwrap());
        let indices = writer.list_date("1999-01-01");
        assert!(indices.is_empty());
    }

    #[test]
    fn list_recent_returns_days_with_traces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = TraceWriter::new(dir.path().to_str().unwrap());
        let result = sample_result(EventType::GreetRequested, "proj-b");

        writer.write("evt_222", &result).expect("write");

        let recent = writer.list_recent(7);

        // Today should appear since we just wrote a trace.
        assert!(!recent.is_empty());
        let (date, indices) = &recent[0];
        let today = Utc::now().format("%Y-%m-%d").to_string();
        assert_eq!(date, &today);
        assert_eq!(indices[0].event_id, "evt_222");
    }
}
