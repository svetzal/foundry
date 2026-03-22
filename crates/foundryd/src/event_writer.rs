use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;
use foundry_core::event::Event;

/// Writes events to JSONL files on disk, one event per line.
///
/// Files are named by date (`YYYY-MM-DD.jsonl`) and stored in the configured directory.
/// The output format is compatible with the evt-cli schema.
pub struct EventWriter {
    dir: PathBuf,
}

impl EventWriter {
    /// Create a new writer that stores JSONL files in `dir`.
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// Append `event` as a single JSON line to today's log file.
    pub fn write(&self, event: &Event) -> Result<()> {
        fs::create_dir_all(&self.dir)?;

        let date = Utc::now().format("%Y-%m-%d");
        let file_path = self.dir.join(format!("{date}.jsonl"));

        let line = serde_json::to_string(event)?;

        let mut file = OpenOptions::new().create(true).append(true).open(&file_path)?;
        writeln!(file, "{line}")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use foundry_core::event::{Event, EventType};
    use foundry_core::throttle::Throttle;

    use super::*;

    #[test]
    fn written_event_has_all_evt_cli_required_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = EventWriter::new(tmp.path());

        let event = Event::new(
            EventType::VulnerabilityDetected,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({"cve": "CVE-2026-1234"}),
        );
        writer.write(&event).unwrap();

        // Read back and check fields
        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(files.len(), 1, "expected exactly one JSONL file");

        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert!(parsed["id"].is_string(), "id must be a string");
        assert!(parsed["event_type"].is_string(), "event_type must be a string");
        assert_eq!(parsed["event_type"], "vulnerability_detected");
        assert!(parsed["project"].is_string(), "project must be a string");
        assert!(parsed["occurred_at"].is_string(), "occurred_at must be a string");
        assert!(parsed["recorded_at"].is_string(), "recorded_at must be a string");
        assert!(parsed["payload"].is_object(), "payload must be an object");

        // Verify timestamps are RFC3339 parseable
        let occurred_at = parsed["occurred_at"].as_str().unwrap();
        chrono::DateTime::parse_from_rfc3339(occurred_at)
            .expect("occurred_at should be RFC3339 formatted");

        let recorded_at = parsed["recorded_at"].as_str().unwrap();
        chrono::DateTime::parse_from_rfc3339(recorded_at)
            .expect("recorded_at should be RFC3339 formatted");
    }

    #[test]
    fn round_trip_event_through_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = EventWriter::new(tmp.path());

        let original = Event::new(
            EventType::GreetRequested,
            "my-project".to_string(),
            Throttle::DryRun,
            serde_json::json!({"greeting": "hello"}),
        );
        let original_id = original.id.clone();
        writer.write(&original).unwrap();

        // Read back
        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let recovered: Event = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(recovered.id, original_id);
        assert_eq!(recovered.event_type, EventType::GreetRequested);
        assert_eq!(recovered.project, "my-project");
        assert_eq!(recovered.payload["greeting"], "hello");
    }

    #[test]
    fn multiple_events_each_on_own_line() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = EventWriter::new(tmp.path());

        for _ in 0..3 {
            let event = Event::new(
                EventType::ScanRequested,
                "proj".to_string(),
                Throttle::Full,
                serde_json::json!({}),
            );
            writer.write(&event).unwrap();
        }

        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3, "each event should occupy one line");

        // Every line must be valid JSON
        for line in &lines {
            serde_json::from_str::<serde_json::Value>(line).expect("each line must be valid JSON");
        }
    }
}
