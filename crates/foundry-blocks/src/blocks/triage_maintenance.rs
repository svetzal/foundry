//! `TriageMaintenance` — entry block of the post-maintenance triage formation.
//!
//! Sinks on `MaintenanceSummaryRequested`. Reads Foundry JSONL events for the
//! maintenance run window, extracts gate failures, classifies them, correlates
//! infra noise, and emits `MaintenanceTriageCompleted` with a full
//! `MaintenanceTriageCompletedPayload`.
//!
//! This block is **propose-only** — it applies nothing and commits nothing to
//! other projects.

use std::path::PathBuf;

use chrono::Duration;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{MaintenanceSummaryRequestedPayload, MaintenanceTriageCompletedPayload};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::triage::Decision;

use super::{emit_result, triage_core};

/// Block that triggers the triage formation after a maintenance summary is
/// requested. Reads JSONL events, extracts failures, classifies them, and
/// emits the triage completed event.
pub struct TriageMaintenance {
    events_dir: PathBuf,
    streak_lookback_days: u32,
}

impl TriageMaintenance {
    pub fn new<P: Into<PathBuf>>(events_dir: P, streak_lookback_days: u32) -> Self {
        Self {
            events_dir: events_dir.into(),
            streak_lookback_days,
        }
    }
}

impl TaskBlock for TriageMaintenance {
    task_block_meta! {
        name: "Triage Maintenance",
        kind: Observer,
        sinks_on: [MaintenanceSummaryRequested],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let p = parse_payload!(trigger, MaintenanceSummaryRequestedPayload);
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let trigger_occurred_at = trigger.occurred_at;
        let events_dir = self.events_dir.clone();
        let streak_lookback_days = self.streak_lookback_days;

        Box::pin(async move {
            triage(&project, throttle, &p, trigger_occurred_at, &events_dir, streak_lookback_days)
        })
    }
}

fn triage(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    payload: &MaintenanceSummaryRequestedPayload,
    trigger_occurred_at: chrono::DateTime<chrono::Utc>,
    events_dir: &std::path::Path,
    streak_lookback_days: u32,
) -> anyhow::Result<TaskBlockResult> {
    // Compute the run window.
    // The maintenance run started roughly `total_duration_ms + 60s margin` before
    // the summary was requested.
    let total_duration_ms = payload.total_duration_ms;
    let margin = Duration::seconds(60);
    let run_duration = Duration::milliseconds(i64::try_from(total_duration_ms).unwrap_or(0));
    let window_start = trigger_occurred_at - run_duration - margin;
    let window_end = trigger_occurred_at;

    // Read JSONL events covering the run window.
    let since = window_start - Duration::minutes(5); // small buffer for clock skew
    let all_events = triage_core::read_events_jsonl(events_dir, since);

    // Extract raw failures from the window.
    let (raw_failures, unparsed_from_failures) =
        triage_core::run_failures(&all_events, window_start, window_end);
    let total_failures = raw_failures.len() as u64;

    if total_failures == 0 {
        tracing::info!("triage: no gate failures in maintenance window — skipping");
        let triage_payload = MaintenanceTriageCompletedPayload {
            success: true,
            skipped: true,
            digest_path: None,
            verdicts: vec![],
            infra_incidents: vec![],
            total_failures: 0,
            suppressed_count: 0,
            auto_fixable_count: 0,
            policy_count: 0,
            investigation_count: 0,
            escalation_count: 0,
            unparsed_events: unparsed_from_failures,
        };
        return emit_result(
            "triage: no failures to classify".to_string(),
            EventType::MaintenanceTriageCompleted,
            project,
            throttle,
            &triage_payload,
        );
    }

    // Build verdicts and incidents.
    let (verdicts, infra_incidents, unparsed_from_streaks) =
        triage_core::build_verdicts(raw_failures, &all_events, streak_lookback_days);
    let unparsed_events = unparsed_from_failures + unparsed_from_streaks;

    // Count by decision.
    let suppressed_count = infra_incidents.len() as u64;
    let auto_fixable_count =
        verdicts.iter().filter(|v| v.decision == Decision::AutoFixable).count() as u64;
    let policy_count =
        verdicts.iter().filter(|v| v.decision == Decision::PolicyCall).count() as u64;
    let investigation_count =
        verdicts.iter().filter(|v| v.decision == Decision::NeedsInvestigation).count() as u64;
    let escalation_count =
        verdicts.iter().filter(|v| v.decision == Decision::Escalate).count() as u64;

    tracing::info!(
        total_failures,
        suppressed = suppressed_count,
        auto_fixable = auto_fixable_count,
        policy = policy_count,
        investigation = investigation_count,
        escalation = escalation_count,
        unparsed_events,
        "triage: classification complete"
    );

    let triage_payload = MaintenanceTriageCompletedPayload {
        success: true,
        skipped: false,
        digest_path: None, // filled in by WriteTriageDigest
        verdicts,
        infra_incidents,
        total_failures,
        suppressed_count,
        auto_fixable_count,
        policy_count,
        investigation_count,
        escalation_count,
        unparsed_events,
    };

    emit_result(
        format!(
            "triage: {total_failures} failure(s) classified \
             (auto-fix={auto_fixable_count}, policy={policy_count}, \
             investigation={investigation_count}, escalation={escalation_count}, \
             suppressed={suppressed_count})"
        ),
        EventType::MaintenanceTriageCompleted,
        project,
        throttle,
        &triage_payload,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::payload::MaintenanceTriageCompletedPayload;
    use foundry_sdk::throttle::Throttle;

    use super::super::test_helpers;
    use super::*;

    fn make_trigger(total_duration_ms: u64) -> Event {
        test_helpers::make_trigger(
            EventType::MaintenanceSummaryRequested,
            "system",
            serde_json::json!({
                "project_trace_ids": {},
                "skipped_projects": [],
                "total_duration_ms": total_duration_ms,
            }),
        )
    }

    assert_block_meta!(
        TriageMaintenance::new(std::path::PathBuf::from("/tmp"), 14),
        kind: Observer,
        sinks_on: [MaintenanceSummaryRequested],
    );

    #[tokio::test]
    async fn emits_triage_completed_with_skipped_when_no_failures() {
        let dir = tempfile::tempdir().unwrap();
        let block = TriageMaintenance::new(dir.path(), 14);

        // Empty events dir → no failures.
        let trigger = make_trigger(5000);
        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::MaintenanceTriageCompleted);

        let payload: MaintenanceTriageCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(payload.success);
        assert!(payload.skipped);
        assert_eq!(payload.total_failures, 0);
    }

    #[tokio::test]
    async fn emits_triage_completed_with_verdicts_when_failures_present() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();

        // Write a JSONL file with a preflight failure inside the run window.
        let now = chrono::Utc::now();
        let month = now.format("%Y-%m").to_string();
        let path = dir.path().join(format!("{month}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();

        let failure_event = Event::new(
            EventType::PreflightCompleted,
            "system".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "alpha",
                "workflow": "maintain",
                "all_passed": false,
                "required_passed": false,
                "results": [{
                    "name": "fmt",
                    "command": "cargo fmt --check",
                    "passed": false,
                    "required": true,
                    "output": "rustfmt diff detected",
                    "exit_code": 1,
                }]
            }),
        );
        writeln!(f, "{}", serde_json::to_string(&failure_event).unwrap()).unwrap();
        drop(f);

        // Trigger with a short duration so the window includes the event.
        let block = TriageMaintenance::new(dir.path(), 14);
        let trigger = make_trigger(1000);
        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let payload: MaintenanceTriageCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(payload.success);
        assert!(!payload.skipped);
        assert!(payload.total_failures >= 1);
    }
}
