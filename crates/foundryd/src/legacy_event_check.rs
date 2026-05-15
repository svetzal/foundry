//! Startup guard that refuses to launch foundryd when pre-0.17.0 event names
//! are still present on disk.
//!
//! The 0.17.0 cutover renamed several event types (see
//! `scripts/migrate-event-names.sh`). If the operator forgot to run the
//! migration, downstream consumers will silently miss these events. We fail
//! fast with a clear remediation message instead.

/// Scan `~/.foundry/events/*.jsonl` for legacy event-type names that the
/// 0.17.0 cutover renames. Returns the first legacy name found, if any.
pub fn detect_legacy_event_names(events_dir: &std::path::Path) -> Option<String> {
    const LEGACY: &[&str] = &[
        "maintenance_run_started",
        "maintenance_run_completed",
        "iteration_requested",
        "maintenance_requested",
        "greet_requested",
    ];

    // Missing dir = nothing to check.
    let Ok(read_dir) = std::fs::read_dir(events_dir) else {
        return None;
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            for name in LEGACY {
                // Cheap pre-filter then exact JSON match.
                if line.contains(name) && line.contains(&format!(r#""event_type":"{name}""#)) {
                    return Some((*name).to_string());
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn detects_legacy_maintenance_run_started() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("2026-05.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(
            f,
            r#"{{"id":"evt_a","event_type":"maintenance_run_started","project":"system","occurred_at":"2026-05-01T00:00:00Z","recorded_at":"2026-05-01T00:00:00Z","throttle":"full","payload":{{}}}}"#
        ).unwrap();

        let result = detect_legacy_event_names(dir.path());
        assert_eq!(result.as_deref(), Some("maintenance_run_started"));
    }

    #[test]
    fn returns_none_for_clean_directory() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("2026-05.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(
            f,
            r#"{{"id":"evt_a","event_type":"maintenance_cycle_started","project":"system","occurred_at":"2026-05-01T00:00:00Z","recorded_at":"2026-05-01T00:00:00Z","throttle":"full","payload":{{}}}}"#
        ).unwrap();

        assert!(detect_legacy_event_names(dir.path()).is_none());
    }

    #[test]
    fn returns_none_for_missing_directory() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(detect_legacy_event_names(&missing).is_none());
    }
}
