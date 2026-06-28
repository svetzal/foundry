//! Pure functional core for the post-maintenance failure triage formation.
//!
//! This module contains all analysis logic — JSONL reading, raw failure
//! extraction, classification, correlation, and streak detection — with no I/O
//! side effects beyond reading JSONL files. All functions are unit-testable
//! without a real engine.

use chrono::{DateTime, Utc};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::PreflightCompletedPayload;

mod classify;
mod correlate;
mod jsonl;
mod streaks;

pub use classify::*;
pub use correlate::*;
pub use jsonl::*;
pub use streaks::*;

/// A raw gate failure extracted from events before classification.
#[derive(Debug, Clone)]
pub struct RawFailure {
    pub project: String,
    pub gate_name: String,
    pub gate_output: String,
    pub exit_code: Option<i32>,
    pub fix_command: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

/// Extract raw failures from a set of events within a time window.
///
/// Reads `PreflightCompleted` events, extracts gate results where `passed =
/// false`, and excludes gates that were self-healed (`fix_applied = true`).
/// Only events with `occurred_at` in `[window_start, window_end]` are
/// considered.
pub fn run_failures(
    events: &[Event],
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Vec<RawFailure> {
    let mut failures = Vec::new();

    for event in events {
        if event.event_type != EventType::PreflightCompleted {
            continue;
        }
        if event.occurred_at < window_start || event.occurred_at > window_end {
            continue;
        }

        let Ok(payload) = event.parse_payload::<PreflightCompletedPayload>() else {
            continue;
        };

        for result in &payload.results {
            if result.passed || result.fix_applied {
                continue;
            }
            failures.push(RawFailure {
                project: payload.project.clone(),
                gate_name: result.name.clone(),
                gate_output: result.output.clone(),
                exit_code: Some(result.exit_code),
                fix_command: None, // gate result does not carry fix_command
                occurred_at: event.occurred_at,
            });
        }
    }

    failures
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone as _, Utc};
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::throttle::Throttle;

    use super::*;

    fn make_preflight_event(project: &str, gate: &str, passed: bool, fix_applied: bool) -> Event {
        Event::new(
            EventType::PreflightCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "workflow": "maintain",
                "all_passed": passed,
                "required_passed": passed,
                "results": [{
                    "name": gate,
                    "command": "cargo fmt --check",
                    "passed": passed,
                    "required": true,
                    "output": "some output",
                    "exit_code": i32::from(!passed),
                    "fix_applied": fix_applied,
                }]
            }),
        )
    }

    #[test]
    fn run_failures_excludes_self_healed_gates() {
        let events = vec![
            make_preflight_event("alpha", "fmt", false, true), // self-healed → excluded
            make_preflight_event("beta", "fmt", false, false), // real failure → included
            make_preflight_event("gamma", "fmt", true, false), // passed → excluded
        ];
        let window_end = Utc::now() + Duration::seconds(1);
        let window_start = window_end - Duration::hours(1);

        let failures = run_failures(&events, window_start, window_end);

        assert_eq!(failures.len(), 1, "only the real failure should be included");
        assert_eq!(failures[0].project, "beta");
    }

    #[test]
    fn run_failures_respects_window_bounds() {
        let window_end = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let window_start = window_end - Duration::hours(2);

        let mut inside = make_preflight_event("alpha", "fmt", false, false);
        inside.occurred_at = Utc.with_ymd_and_hms(2026, 6, 1, 11, 0, 0).unwrap();

        let mut before = make_preflight_event("beta", "fmt", false, false);
        before.occurred_at = Utc.with_ymd_and_hms(2026, 6, 1, 9, 0, 0).unwrap();

        let failures = run_failures(&[inside, before], window_start, window_end);

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].project, "alpha");
    }
}
