// EventWriter is a public API module; it will be wired into main once the
// persistence layer is plumbed through. Suppress dead-code lint until then.
#![allow(dead_code)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;
use foundry_core::event::Event;

/// Writes events as JSONL lines to monthly files.
///
/// Each event is appended to a file named `YYYY-MM.jsonl` inside the configured
/// output directory, determined by the event's `occurred_at` timestamp. The
/// directory is created on first use if it does not already exist.
///
/// A `Mutex` serializes concurrent writes so that JSON lines from different
/// threads are never interleaved. The file is opened fresh on each write and
/// closed immediately afterwards, giving crash-safe, unbuffered persistence.
pub struct EventWriter {
    output_dir: PathBuf,
    /// Serializes writes so concurrent callers never interleave partial lines.
    write_lock: Mutex<()>,
}

impl EventWriter {
    /// Create a new `EventWriter` that writes to `output_dir`.
    ///
    /// The directory need not exist at construction time; it is created when
    /// the first event is written.
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self {
            output_dir: output_dir.into(),
            write_lock: Mutex::new(()),
        }
    }

    /// Write `event` as a single JSON line to the appropriate `YYYY-MM.jsonl` file.
    ///
    /// The target file is determined by `event.occurred_at`. The output
    /// directory and the file are both created if they do not already exist.
    /// Each call flushes and closes the file, so no data is lost on crash.
    pub fn write(&self, event: &Event) -> Result<()> {
        // Serialize before acquiring the lock — serde work is pure and cheap.
        let mut line = serde_json::to_string(event)?;
        line.push('\n');

        let month_key = event.occurred_at.format("%Y-%m").to_string();
        let file_path = self.output_dir.join(format!("{month_key}.jsonl"));

        // Hold the lock only while touching the filesystem.
        let _guard = self.write_lock.lock().expect("event writer lock poisoned");

        fs::create_dir_all(&self.output_dir)?;

        let mut file = OpenOptions::new().create(true).append(true).open(&file_path)?;

        file.write_all(line.as_bytes())?;
        file.flush()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{TimeZone, Utc};
    use foundry_core::event::{Event, EventType};
    use foundry_core::throttle::Throttle;
    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;

    fn make_event_at(year: i32, month: u32, day: u32) -> Event {
        let occurred_at =
            Utc.with_ymd_and_hms(year, month, day, 12, 0, 0).single().expect("invalid date");
        let mut event = Event::new(
            EventType::GreetRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({"msg": "hello"}),
        );
        // Override occurred_at so we control the target filename.
        event.occurred_at = occurred_at;
        event.recorded_at = occurred_at;
        event
    }

    // -------------------------------------------------------------------------
    // Directory creation
    // -------------------------------------------------------------------------

    #[test]
    fn creates_output_directory_when_absent() {
        let tmp = TempDir::new().unwrap();
        let output_dir = tmp.path().join("nested/events");
        let writer = EventWriter::new(&output_dir);

        let event = make_event_at(2026, 3, 15);
        writer.write(&event).expect("write failed");

        assert!(output_dir.exists(), "output directory should be created");
    }

    // -------------------------------------------------------------------------
    // Filename routing
    // -------------------------------------------------------------------------

    #[test]
    fn writes_to_yyyy_mm_jsonl_file() {
        let tmp = TempDir::new().unwrap();
        let writer = EventWriter::new(tmp.path());

        let event = make_event_at(2026, 3, 15);
        writer.write(&event).expect("write failed");

        let expected = tmp.path().join("2026-03.jsonl");
        assert!(expected.exists(), "2026-03.jsonl should exist");
    }

    #[test]
    fn routes_to_correct_month_file() {
        let tmp = TempDir::new().unwrap();
        let writer = EventWriter::new(tmp.path());

        writer.write(&make_event_at(2026, 1, 10)).unwrap();
        writer.write(&make_event_at(2026, 2, 20)).unwrap();
        writer.write(&make_event_at(2026, 3, 15)).unwrap();

        assert!(tmp.path().join("2026-01.jsonl").exists());
        assert!(tmp.path().join("2026-02.jsonl").exists());
        assert!(tmp.path().join("2026-03.jsonl").exists());
    }

    // -------------------------------------------------------------------------
    // JSONL content
    // -------------------------------------------------------------------------

    #[test]
    fn each_event_is_a_single_json_line() {
        let tmp = TempDir::new().unwrap();
        let writer = EventWriter::new(tmp.path());

        writer.write(&make_event_at(2026, 3, 1)).unwrap();
        writer.write(&make_event_at(2026, 3, 2)).unwrap();
        writer.write(&make_event_at(2026, 3, 3)).unwrap();

        let contents = fs::read_to_string(tmp.path().join("2026-03.jsonl")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3, "should have exactly 3 lines");
    }

    #[test]
    fn each_line_is_valid_json() {
        let tmp = TempDir::new().unwrap();
        let writer = EventWriter::new(tmp.path());

        writer.write(&make_event_at(2026, 3, 1)).unwrap();
        writer.write(&make_event_at(2026, 3, 5)).unwrap();

        let contents = fs::read_to_string(tmp.path().join("2026-03.jsonl")).unwrap();
        for line in contents.lines() {
            let v: Result<Value, _> = serde_json::from_str(line);
            assert!(v.is_ok(), "line should be valid JSON: {line}");
        }
    }

    #[test]
    fn written_json_contains_expected_fields() {
        let tmp = TempDir::new().unwrap();
        let writer = EventWriter::new(tmp.path());

        let event = make_event_at(2026, 3, 15);
        let event_id = event.id.clone();
        writer.write(&event).unwrap();

        let contents = fs::read_to_string(tmp.path().join("2026-03.jsonl")).unwrap();
        let line = contents.lines().next().unwrap();
        let v: Value = serde_json::from_str(line).unwrap();

        assert_eq!(v["id"], event_id);
        assert_eq!(v["event_type"], "greet_requested");
        assert_eq!(v["project"], "test-project");
        assert!(v["occurred_at"].is_string(), "occurred_at should be a string");
        assert!(v["recorded_at"].is_string(), "recorded_at should be a string");
        assert!(v.get("throttle").is_some(), "throttle field should be present");
        assert!(v.get("payload").is_some(), "payload field should be present");
    }

    #[test]
    fn event_with_special_characters_in_payload() {
        let tmp = TempDir::new().unwrap();
        let writer = EventWriter::new(tmp.path());

        let mut event = Event::new(
            EventType::GreetRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({"message": "hello \"world\" \n tab:\there"}),
        );
        let occurred_at = Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).single().unwrap();
        event.occurred_at = occurred_at;
        event.recorded_at = occurred_at;

        writer.write(&event).unwrap();

        let contents = fs::read_to_string(tmp.path().join("2026-03.jsonl")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1, "special chars should produce exactly one line");

        let v: Value = serde_json::from_str(lines[0]).unwrap();
        assert!(v["payload"]["message"].is_string());
    }

    // -------------------------------------------------------------------------
    // File rotation
    // -------------------------------------------------------------------------

    #[test]
    fn file_rotation_when_month_changes() {
        let tmp = TempDir::new().unwrap();
        let writer = EventWriter::new(tmp.path());

        // Write into December then January (year boundary)
        writer.write(&make_event_at(2025, 12, 31)).unwrap();
        writer.write(&make_event_at(2026, 1, 1)).unwrap();

        let dec = fs::read_to_string(tmp.path().join("2025-12.jsonl")).unwrap();
        let jan = fs::read_to_string(tmp.path().join("2026-01.jsonl")).unwrap();

        assert_eq!(dec.lines().count(), 1);
        assert_eq!(jan.lines().count(), 1);
    }

    // -------------------------------------------------------------------------
    // Concurrent writes
    // -------------------------------------------------------------------------

    #[test]
    fn concurrent_writes_do_not_corrupt_file() {
        use std::sync::Arc;
        use std::thread;

        let tmp = TempDir::new().unwrap();
        let writer = Arc::new(EventWriter::new(tmp.path()));

        let handles: Vec<_> = (0..20)
            .map(|_| {
                let w = Arc::clone(&writer);
                thread::spawn(move || {
                    let event = make_event_at(2026, 3, 15);
                    w.write(&event).expect("concurrent write failed");
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        let contents = fs::read_to_string(tmp.path().join("2026-03.jsonl")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 20, "all 20 concurrent writes should be present");

        for line in &lines {
            let v: Result<Value, _> = serde_json::from_str(line);
            assert!(v.is_ok(), "concurrent line should be valid JSON: {line}");
        }
    }

    // -------------------------------------------------------------------------
    // Integration: round-trip
    // -------------------------------------------------------------------------

    #[test]
    fn written_events_can_be_read_back() {
        let tmp = TempDir::new().unwrap();
        let writer = EventWriter::new(tmp.path());

        let events: Vec<Event> = (1..=5).map(|day| make_event_at(2026, 3, day)).collect();
        for e in &events {
            writer.write(e).unwrap();
        }

        let contents = fs::read_to_string(tmp.path().join("2026-03.jsonl")).unwrap();
        let read_back: Vec<Event> = contents
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid Event JSON"))
            .collect();

        assert_eq!(read_back.len(), events.len());
        for (original, recovered) in events.iter().zip(read_back.iter()) {
            assert_eq!(original.id, recovered.id);
            assert_eq!(original.event_type, recovered.event_type);
            assert_eq!(original.project, recovered.project);
        }
    }
}
