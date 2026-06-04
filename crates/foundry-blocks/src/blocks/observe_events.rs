//! `ObserveEvents` — first block of the ops-digest formation.
//!
//! Sinks on `OpsDigestStarted`. Reads MBOS JSONL event files from the ops
//! events intake directory, filters to events newer than the current watermark,
//! and applies a pressure gate: the chain only proceeds if enough new events
//! have arrived or an anomaly is present. On gate failure emits
//! `OpsDigestCompleted{skipped: true}` immediately. On gate success emits
//! `OpsObserved` with lean per-event summaries for the downstream summariser.

use std::path::{Path, PathBuf};
use std::pin::Pin;

use chrono::{DateTime, FixedOffset};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{OpsDigestCompletedPayload, OpsEventDigest, OpsObservedPayload};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use serde::Deserialize;

use super::emit_result;

/// Minimum new-event count required to proceed to summarisation. Below this
/// threshold, only an anomaly can force the chain to run.
const OPS_DIGEST_EVENT_THRESHOLD: u64 = 25;

/// When no watermark exists (first ever run), look back this many hours so
/// the first digest isn't empty on a freshly-installed daemon.
const FIRST_RUN_LOOKBACK_HOURS: i64 = 24;

/// Reads and pressure-gates new MBOS operational events, emitting
/// `OpsObserved` when the chain should proceed or `OpsDigestCompleted{skipped}`
/// when there is not yet enough signal.
pub struct ObserveEvents {
    intake_dir: PathBuf,
    watermark_path: PathBuf,
}

impl ObserveEvents {
    pub fn new<P: Into<PathBuf>, W: Into<PathBuf>>(intake_dir: P, watermark_path: W) -> Self {
        Self {
            intake_dir: intake_dir.into(),
            watermark_path: watermark_path.into(),
        }
    }
}

impl TaskBlock for ObserveEvents {
    task_block_meta! {
        name: "Observe Events",
        kind: Observer,
        sinks_on: [OpsDigestStarted],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let intake_dir = self.intake_dir.clone();
        let watermark_path = self.watermark_path.clone();

        Box::pin(async move { observe(&project, throttle, &intake_dir, &watermark_path) })
    }
}

fn observe(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    intake_dir: &Path,
    watermark_path: &Path,
) -> anyhow::Result<TaskBlockResult> {
    let cutoff = read_cutoff(watermark_path);
    let events = read_events_since(intake_dir, cutoff);

    let new_event_count = events.len() as u64;
    let anomaly_present = events.iter().any(is_anomaly);

    if !should_proceed(new_event_count, anomaly_present) {
        tracing::info!(
            new_event_count,
            anomaly_present,
            threshold = OPS_DIGEST_EVENT_THRESHOLD,
            "ops-digest pressure gate not satisfied — skipping",
        );
        let payload = OpsDigestCompletedPayload {
            success: true,
            skipped: true,
            digest_path: None,
            event_count: new_event_count,
        };
        return emit_result(
            format!(
                "ops-digest skipped: {new_event_count} new event(s), no anomaly (threshold {OPS_DIGEST_EVENT_THRESHOLD})"
            ),
            EventType::OpsDigestCompleted,
            project,
            throttle,
            &payload,
        );
    }

    let new_watermark = events.iter().map(|e| e.occurred_at.clone()).max();

    let digests: Vec<OpsEventDigest> = events
        .into_iter()
        .map(|e| {
            let client = e.client.as_ref().and_then(|c| c.name.clone());
            let domain = domain_of(&e.event_type, client.is_some()).to_string();
            OpsEventDigest {
                id: e.id,
                event_type: e.event_type,
                occurred_at: e.occurred_at,
                domain,
                urgency: e.urgency,
                summary: e.summary,
                client,
            }
        })
        .collect();

    tracing::info!(
        new_event_count,
        anomaly_present,
        "ops-digest pressure gate satisfied — proceeding",
    );

    let payload = OpsObservedPayload {
        proceed: true,
        new_event_count,
        anomaly_present,
        new_watermark,
        events: digests,
    };

    emit_result(
        format!("Observed {new_event_count} new ops event(s) (anomaly: {anomaly_present})"),
        EventType::OpsObserved,
        project,
        throttle,
        &payload,
    )
}

/// Read the watermark from disk. Returns `None` on missing/unreadable file.
fn read_watermark(path: &Path) -> Option<DateTime<FixedOffset>> {
    let content = std::fs::read_to_string(path).ok()?;
    let ts = content.trim();
    DateTime::parse_from_rfc3339(ts).ok()
}

/// Determine the earliest `occurredAt` to include in this run.
///
/// If no watermark exists, falls back to 24 hours ago so the first run
/// is not empty on a freshly-installed daemon.
fn read_cutoff(watermark_path: &Path) -> DateTime<FixedOffset> {
    if let Some(wm) = read_watermark(watermark_path) {
        return wm;
    }
    let fallback = chrono::Utc::now() - chrono::Duration::hours(FIRST_RUN_LOOKBACK_HOURS);
    fallback.fixed_offset()
}

/// Decide whether the chain should proceed to summarisation.
fn should_proceed(count: u64, anomaly_present: bool) -> bool {
    anomaly_present || count >= OPS_DIGEST_EVENT_THRESHOLD
}

// ---------------------------------------------------------------------------
// MBOS event deserialization
// ---------------------------------------------------------------------------

/// Minimal MBOS event shape — only the fields `ObserveEvents` needs.
#[derive(Debug, Deserialize)]
struct MbosEvent {
    id: String,
    #[serde(rename = "type")]
    event_type: String,
    #[serde(rename = "occurredAt")]
    occurred_at: String,
    urgency: Option<String>,
    summary: Option<String>,
    client: Option<ClientRef>,
    // Fields used only for anomaly classification
    maintenance: Option<serde_json::Value>,
    vulnerability: Option<serde_json::Value>,
    intervention: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ClientRef {
    name: Option<String>,
}

/// Classify a single MBOS event as an anomaly.
///
/// Anomaly conditions:
/// - Urgency `P0` on any event
/// - `ci_pipeline_failure` (always)
/// - `maintenance_intervention_recorded` where `intervention.outcome == "unresolved"`
/// - `dependency_vulnerability_detected` where `vulnerability.severity` is `"high"` or `"critical"`
/// - `maintenance_run_completed` where `maintenance.repos_failed > 0`
fn is_anomaly(event: &MbosEvent) -> bool {
    if event.urgency.as_deref() == Some("P0") {
        return true;
    }
    match event.event_type.as_str() {
        "ci_pipeline_failure" => true,
        "maintenance_intervention_recorded" => event
            .intervention
            .as_ref()
            .and_then(|i| i.get("outcome"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|o| o == "unresolved"),
        "dependency_vulnerability_detected" => event
            .vulnerability
            .as_ref()
            .and_then(|v| v.get("severity"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| matches!(s, "high" | "critical")),
        "maintenance_run_completed" => event
            .maintenance
            .as_ref()
            .and_then(|m| m.get("reposFailed"))
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|n| n > 0),
        _ => false,
    }
}

/// Classify an event type into a domain bucket for the digest.
fn domain_of(event_type: &str, has_client: bool) -> &'static str {
    if has_client {
        return "clients";
    }
    match event_type {
        t if t.starts_with("email_inbound") || t.starts_with("whatsapp_inbound") => "clients",
        t if t.starts_with("ai_") || t.starts_with("hone_") => "ai",
        t if t.starts_with("ci_")
            || t.starts_with("deployment_")
            || t.starts_with("maintenance_")
            || t.starts_with("dependency_") =>
        {
            "infrastructure"
        }
        t if t.starts_with("issue_") || t.starts_with("code_review_") => "engineering",
        t if t.starts_with("monthly_") || t.starts_with("billable_") => "billing",
        t if t.starts_with("meeting_") || t.starts_with("weekly_") => "operations",
        _ => "other",
    }
}

/// Read all MBOS JSONL files in the intake directory, parse each line into a
/// `MbosEvent`, and return only events with `occurredAt` strictly after
/// `cutoff`. Malformed lines are silently skipped.
fn read_events_since(intake_dir: &Path, cutoff: DateTime<FixedOffset>) -> Vec<MbosEvent> {
    let mut events = Vec::new();

    let entries = match std::fs::read_dir(intake_dir) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(
                dir = %intake_dir.display(),
                error = %err,
                "ops-digest: cannot read intake directory"
            );
            return events;
        }
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    paths.sort();

    for path in &paths {
        read_jsonl_file(path, cutoff, &mut events);
    }

    events
}

fn read_jsonl_file(path: &Path, cutoff: DateTime<FixedOffset>, out: &mut Vec<MbosEvent>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "ops-digest: cannot read JSONL file");
            return;
        }
    };

    for (line_no, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<MbosEvent>(trimmed) {
            Ok(event) => {
                // Only include events strictly after the cutoff.
                if let Ok(ts) = DateTime::parse_from_rfc3339(&event.occurred_at)
                    && ts > cutoff
                {
                    out.push(event);
                }
                // Events with unparseable timestamps are silently skipped.
            }
            Err(err) => {
                tracing::debug!(
                    path = %path.display(),
                    line = line_no + 1,
                    error = %err,
                    "ops-digest: skipping malformed JSONL line"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use foundry_sdk::event::EventType;
    use foundry_sdk::payload::{OpsDigestCompletedPayload, OpsObservedPayload};
    use foundry_sdk::throttle::Throttle;
    use tempfile::TempDir;

    use super::*;

    // -------------------------------------------------------------------------
    // Pure-function tests
    // -------------------------------------------------------------------------

    #[test]
    fn below_threshold_no_anomaly_should_not_proceed() {
        assert!(!should_proceed(0, false));
        assert!(!should_proceed(24, false));
    }

    #[test]
    fn at_threshold_should_proceed() {
        assert!(should_proceed(25, false));
    }

    #[test]
    fn above_threshold_should_proceed() {
        assert!(should_proceed(100, false));
    }

    #[test]
    fn anomaly_overrides_threshold() {
        assert!(should_proceed(0, true));
        assert!(should_proceed(1, true));
    }

    fn make_event(event_type: &str) -> MbosEvent {
        MbosEvent {
            id: "evt_test".to_string(),
            event_type: event_type.to_string(),
            occurred_at: "2026-05-29T12:00:00Z".to_string(),
            urgency: None,
            summary: None,
            client: None,
            maintenance: None,
            vulnerability: None,
            intervention: None,
        }
    }

    #[test]
    fn is_anomaly_p0() {
        let mut e = make_event("some_event");
        e.urgency = Some("P0".to_string());
        assert!(is_anomaly(&e));
    }

    #[test]
    fn is_anomaly_p1_is_not_an_anomaly() {
        let mut e = make_event("some_event");
        e.urgency = Some("P1".to_string());
        assert!(!is_anomaly(&e));
    }

    #[test]
    fn is_anomaly_ci_pipeline_failure() {
        let e = make_event("ci_pipeline_failure");
        assert!(is_anomaly(&e));
    }

    #[test]
    fn is_anomaly_unresolved_intervention() {
        let mut e = make_event("maintenance_intervention_recorded");
        e.intervention = Some(serde_json::json!({ "outcome": "unresolved" }));
        assert!(is_anomaly(&e));
    }

    #[test]
    fn is_anomaly_resolved_intervention() {
        let mut e = make_event("maintenance_intervention_recorded");
        e.intervention = Some(serde_json::json!({ "outcome": "resolved" }));
        assert!(!is_anomaly(&e));
    }

    #[test]
    fn is_anomaly_high_vulnerability() {
        let mut e = make_event("dependency_vulnerability_detected");
        e.vulnerability = Some(serde_json::json!({ "severity": "high" }));
        assert!(is_anomaly(&e));
    }

    #[test]
    fn is_anomaly_critical_vulnerability() {
        let mut e = make_event("dependency_vulnerability_detected");
        e.vulnerability = Some(serde_json::json!({ "severity": "critical" }));
        assert!(is_anomaly(&e));
    }

    #[test]
    fn is_anomaly_low_vulnerability() {
        let mut e = make_event("dependency_vulnerability_detected");
        e.vulnerability = Some(serde_json::json!({ "severity": "low" }));
        assert!(!is_anomaly(&e));
    }

    #[test]
    fn is_anomaly_maintenance_with_failures() {
        let mut e = make_event("maintenance_run_completed");
        e.maintenance = Some(serde_json::json!({ "reposFailed": 2 }));
        assert!(is_anomaly(&e));
    }

    #[test]
    fn is_anomaly_maintenance_no_failures() {
        let mut e = make_event("maintenance_run_completed");
        e.maintenance = Some(serde_json::json!({ "reposFailed": 0 }));
        assert!(!is_anomaly(&e));
    }

    // -------------------------------------------------------------------------
    // cutoff / filtering helpers
    // -------------------------------------------------------------------------

    fn ts(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).expect("valid RFC3339 timestamp in test")
    }

    #[test]
    fn cutoff_filtering() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("2026-05.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // One before cutoff, two after.
        writeln!(f, r#"{{"id":"e1","type":"ai_session_end","occurredAt":"2026-05-01T00:00:00Z","recordedAt":"2026-05-01T00:00:00Z","category":"System-detected","urgency":"P2","status":"done","summary":"before","source":{{}}}}"#).unwrap();
        writeln!(f, r#"{{"id":"e2","type":"ai_session_end","occurredAt":"2026-05-29T10:00:00Z","recordedAt":"2026-05-29T10:00:00Z","category":"System-detected","urgency":"P2","status":"done","summary":"after1","source":{{}}}}"#).unwrap();
        writeln!(f, r#"{{"id":"e3","type":"ci_pipeline_failure","occurredAt":"2026-05-29T11:00:00Z","recordedAt":"2026-05-29T11:00:00Z","category":"System-detected","urgency":"P1","status":"new","summary":"after2","source":{{}}}}"#).unwrap();
        drop(f);

        let cutoff = ts("2026-05-29T00:00:00Z");
        let events = read_events_since(dir.path(), cutoff.fixed_offset());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, "e2");
        assert_eq!(events[1].id, "e3");
    }

    #[test]
    fn first_run_lookback() {
        // No watermark file → cutoff should be approximately 24h ago.
        let dir = TempDir::new().unwrap();
        let wm_path = dir.path().join("ops-digest.watermark");
        // File does not exist.
        let cutoff = read_cutoff(&wm_path);
        let now = chrono::Utc::now().fixed_offset();
        let diff_hours = (now - cutoff).num_hours();
        // Should be close to 24h ago (allow +/- 1h for test timing).
        assert!((23..=25).contains(&diff_hours), "expected ~24h lookback, got {diff_hours}h");
    }

    #[test]
    fn malformed_line_skipped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("2026-05.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "this is not json at all").unwrap();
        writeln!(f, r#"{{"id":"e1","type":"ai_session_end","occurredAt":"2026-05-29T10:00:00Z","recordedAt":"2026-05-29T10:00:00Z","category":"System-detected","urgency":"P2","status":"done","summary":"ok","source":{{}}}}"#).unwrap();
        drop(f);

        let cutoff = ts("2026-05-01T00:00:00Z");
        let events = read_events_since(dir.path(), cutoff.fixed_offset());
        assert_eq!(events.len(), 1, "malformed line must not abort parsing");
        assert_eq!(events[0].id, "e1");
    }

    // -------------------------------------------------------------------------
    // Block execute() integration tests
    // -------------------------------------------------------------------------

    fn trigger() -> Event {
        Event::new(
            EventType::OpsDigestStarted,
            "system".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
    }

    fn write_jsonl(dir: &TempDir, name: &str, lines: &[&str]) {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
    }

    fn fresh_event_line(id: &str, event_type: &str, ts: &str) -> String {
        format!(
            r#"{{"id":"{id}","type":"{event_type}","occurredAt":"{ts}","recordedAt":"{ts}","category":"System-detected","urgency":"P2","status":"done","summary":"summary for {id}","source":{{}}}}"#
        )
    }

    #[test]
    fn skip_branch_emits_completed_when_gate_not_satisfied() {
        let intake = TempDir::new().unwrap();
        let wm_dir = TempDir::new().unwrap();
        let wm_path = wm_dir.path().join("ops-digest.watermark");

        // Write a watermark so we don't use the 24h fallback.
        std::fs::write(&wm_path, "2026-05-01T00:00:00Z").unwrap();

        // Only 2 events — below threshold, no anomalies.
        write_jsonl(
            &intake,
            "2026-05.jsonl",
            &[
                &fresh_event_line("e1", "ai_session_end", "2026-05-29T10:00:00Z"),
                &fresh_event_line("e2", "ai_session_end", "2026-05-29T11:00:00Z"),
            ],
        );

        let block = ObserveEvents::new(intake.path(), &wm_path);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(block.execute(&trigger()))
            .unwrap();

        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::OpsDigestCompleted);
        let payload: OpsDigestCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(payload.success);
        assert!(payload.skipped);
        assert_eq!(payload.event_count, 2);
    }

    #[test]
    fn proceed_branch_emits_ops_observed_when_threshold_met() {
        let intake = TempDir::new().unwrap();
        let wm_dir = TempDir::new().unwrap();
        let wm_path = wm_dir.path().join("ops-digest.watermark");
        std::fs::write(&wm_path, "2026-05-01T00:00:00Z").unwrap();

        // 25 events — at threshold.
        let lines: Vec<String> = (0..25)
            .map(|i| {
                fresh_event_line(
                    &format!("e{i}"),
                    "ai_session_end",
                    &format!("2026-05-29T{:02}:00:00Z", i % 24),
                )
            })
            .collect();
        let lines_ref: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_jsonl(&intake, "2026-05.jsonl", &lines_ref);

        let block = ObserveEvents::new(intake.path(), &wm_path);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(block.execute(&trigger()))
            .unwrap();

        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::OpsObserved);
        let payload: OpsObservedPayload = result.events[0].parse_payload().unwrap();
        assert!(payload.proceed);
        assert_eq!(payload.new_event_count, 25);
        assert!(!payload.anomaly_present);
        assert_eq!(payload.events.len(), 25);
    }

    #[test]
    fn anomaly_overrides_threshold_in_block() {
        let intake = TempDir::new().unwrap();
        let wm_dir = TempDir::new().unwrap();
        let wm_path = wm_dir.path().join("ops-digest.watermark");
        std::fs::write(&wm_path, "2026-05-01T00:00:00Z").unwrap();

        // Only 1 event, but it's a CI failure → anomaly.
        write_jsonl(
            &intake,
            "2026-05.jsonl",
            &[&fresh_event_line(
                "e1",
                "ci_pipeline_failure",
                "2026-05-29T10:00:00Z",
            )],
        );

        let block = ObserveEvents::new(intake.path(), &wm_path);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(block.execute(&trigger()))
            .unwrap();

        assert_eq!(result.events[0].event_type, EventType::OpsObserved);
        let payload: OpsObservedPayload = result.events[0].parse_payload().unwrap();
        assert!(payload.proceed);
        assert!(payload.anomaly_present);
    }
}
