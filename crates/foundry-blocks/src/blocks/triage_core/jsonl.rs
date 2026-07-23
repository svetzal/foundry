use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike as _, Utc};
use foundry_sdk::event::Event;

/// Read Foundry events from JSONL files since a given timestamp.
///
/// Reads files for the current and prior month from `events_dir`
/// (files are named `YYYY-MM.jsonl`). Malformed lines are skipped and
/// logged at `debug!` (expected, high-cardinality) — never silently
/// discarded; an unreadable file is logged at `warn!`.
pub fn read_events_jsonl(events_dir: &Path, since: DateTime<Utc>) -> Vec<Event> {
    let mut events = Vec::new();

    let now = Utc::now();
    let months: Vec<String> = {
        let mut v = Vec::new();
        // Prior month
        let prior = if now.month0() == 0 {
            chrono::NaiveDate::from_ymd_opt(now.year() - 1, 12, 1)
        } else {
            chrono::NaiveDate::from_ymd_opt(now.year(), now.month() - 1, 1)
        };
        if let Some(d) = prior {
            v.push(d.format("%Y-%m").to_string());
        }
        // Current month
        v.push(now.format("%Y-%m").to_string());
        v
    };

    for month in &months {
        let path = events_dir.join(format!("{month}.jsonl"));
        read_jsonl_file(&path, since, &mut events);
    }

    events
}

fn read_jsonl_file(path: &Path, since: DateTime<Utc>, out: &mut Vec<Event>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) => {
            // Unreadable file is operationally meaningful (permissions,
            // missing intake directory) — warn!, not debug!.
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "triage-core: cannot read JSONL file (skipping)"
            );
            return;
        }
    };

    for (line_no, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(trimmed) {
            Ok(event) if event.occurred_at > since => {
                out.push(event);
            }
            Ok(_) => {}
            Err(err) => {
                tracing::debug!(
                    path = %path.display(),
                    line = line_no + 1,
                    error = %err,
                    "triage-core: skipping malformed JSONL line"
                );
            }
        }
    }
}

/// Return the paths for the current and prior month JSONL files.
///
/// Exposed for callers that want to know which files will be read before
/// calling `read_events_jsonl`.
#[allow(dead_code)]
pub fn month_paths(events_dir: &Path) -> Vec<PathBuf> {
    let now = Utc::now();
    let mut paths = Vec::new();

    let prior = if now.month0() == 0 {
        chrono::NaiveDate::from_ymd_opt(now.year() - 1, 12, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(now.year(), now.month() - 1, 1)
    };
    if let Some(d) = prior {
        paths.push(events_dir.join(format!("{}.jsonl", d.format("%Y-%m"))));
    }
    paths.push(events_dir.join(format!("{}.jsonl", now.format("%Y-%m"))));
    paths
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use chrono::{Duration, Utc};
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::throttle::Throttle;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn read_events_jsonl_reads_events_after_since() {
        let dir = TempDir::new().unwrap();

        let event1 = Event::new(
            EventType::PreflightCompleted,
            "proj".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "proj", "workflow": "maintain", "all_passed": true, "required_passed": true, "results": []}),
        );
        let event2 = Event::new(
            EventType::PreflightCompleted,
            "proj2".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "proj2", "workflow": "maintain", "all_passed": false, "required_passed": false, "results": []}),
        );

        let now = Utc::now();
        let month = now.format("%Y-%m").to_string();
        let path = dir.path().join(format!("{month}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", serde_json::to_string(&event1).unwrap()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&event2).unwrap()).unwrap();
        drop(f);

        let since = now - Duration::hours(1);
        let loaded = read_events_jsonl(dir.path(), since);

        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn read_events_jsonl_skips_malformed_lines() {
        let dir = TempDir::new().unwrap();
        let now = Utc::now();
        let month = now.format("%Y-%m").to_string();
        let path = dir.path().join(format!("{month}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "this is not json at all").unwrap();

        let good = Event::new(
            EventType::PreflightCompleted,
            "proj".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "proj", "workflow": "maintain", "all_passed": true, "required_passed": true, "results": []}),
        );
        writeln!(f, "{}", serde_json::to_string(&good).unwrap()).unwrap();
        drop(f);

        let since = now - Duration::hours(1);
        let events = read_events_jsonl(dir.path(), since);
        assert_eq!(events.len(), 1, "malformed line must not abort parsing");
    }
}
