use foundry_core::event::{Event, EventType};
use foundry_core::payload::{ExecutionCompletedPayload, LoopContext, RemediationCompletedPayload};
use foundry_core::task_block::TaskBlockResult;
use foundry_core::throttle::Throttle;
use foundry_core::workflow::WorkflowType;

use crate::gateway::AgentOutcome;

/// Emit a single-event result with a serialized payload, controlling the success flag.
///
/// Core helper for blocks whose result emits exactly one event with no `raw_output`
/// or `exit_code`. Use [`emit_result`] for the always-success variant.
pub(crate) fn emit_event_result(
    summary: String,
    success: bool,
    event_type: EventType,
    project: &str,
    throttle: Throttle,
    payload: &impl serde::Serialize,
) -> anyhow::Result<TaskBlockResult> {
    let event_payload = Event::serialize_payload(payload)?;
    Ok(TaskBlockResult {
        events: vec![Event::new(
            event_type,
            project.to_string(),
            throttle,
            event_payload,
        )],
        success,
        summary,
        ..Default::default()
    })
}

/// Emit a single-event success result with a serialized payload.
///
/// Eliminates the three-line boilerplate of `serialize_payload` → `Event::new` →
/// `TaskBlockResult::success` that appears in blocks whose happy path emits
/// exactly one event with `success = true` and no `raw_output` or `exit_code`.
pub(crate) fn emit_result(
    summary: String,
    event_type: EventType,
    project: &str,
    throttle: Throttle,
    payload: &impl serde::Serialize,
) -> anyhow::Result<TaskBlockResult> {
    emit_event_result(summary, true, event_type, project, throttle, payload)
}

/// Build a `TaskBlockResult` for an agent-driven remediation, handling the
/// response match, tracing, payload serialization, and summary formatting.
///
/// `success_label` and `failure_label` are the prefix for `TaskBlockResult.summary`
/// (e.g. "Remediated CVE-2026-1234" / "Remediation of CVE-2026-1234 failed").
pub(crate) fn build_agent_remediation_result(
    project: &str,
    throttle: Throttle,
    outcome: AgentOutcome,
    cve: Option<String>,
    pipeline_fix: Option<bool>,
    success_label: &str,
    failure_label: &str,
) -> TaskBlockResult {
    let (raw_output, success, summary) = match outcome {
        AgentOutcome::Success { stdout } => {
            let out = stdout.trim().to_string();
            (Some(out), true, "remediation completed".to_string())
        }
        AgentOutcome::AgentFailed { stderr } => {
            let first_line = stderr.lines().next().unwrap_or("agent failed");
            let summary = format!("remediation failed: {first_line}");
            (Some(stderr), false, summary)
        }
        AgentOutcome::Unavailable { error } => (None, false, format!("agent unavailable: {error}")),
    };

    tracing::info!(
        project = %project,
        success = success,
        summary = %summary,
        "remediation completed"
    );

    let event_payload = Event::serialize_payload(&RemediationCompletedPayload {
        cve,
        success,
        summary: Some(summary.clone()),
        dry_run: None,
        pipeline_fix,
    })
    .expect("RemediationCompletedPayload is infallibly serializable");

    TaskBlockResult {
        events: vec![Event::new(
            EventType::RemediationCompleted,
            project.to_string(),
            throttle,
            event_payload,
        )],
        success,
        summary: if success {
            format!("{success_label}: {summary}")
        } else {
            format!("{failure_label}: {summary}")
        },
        raw_output,
        ..Default::default()
    }
}

/// Produce the single simulated-success `ExecutionCompleted` event used by
/// `dry_run_events()` across all three execution blocks.
///
/// Encapsulates `LoopContext::extract_from`, `ExecutionCompletedPayload` construction,
/// `trigger.with_payload()`, and the infallibility `.expect()`.
pub(crate) fn dry_run_execution_event(
    trigger: &Event,
    workflow: WorkflowType,
    retry_count: Option<u64>,
) -> Vec<Event> {
    let context = LoopContext::extract_from(&trigger.payload);
    vec![
        trigger
            .with_payload(
                EventType::ExecutionCompleted,
                &ExecutionCompletedPayload {
                    project: trigger.project.clone(),
                    workflow: workflow.to_string(),
                    success: true,
                    summary: String::new(),
                    execution_output: None,
                    dry_run: Some(true),
                    retry_count,
                    changes_detected: None,
                    files_changed: vec![],
                    context,
                },
            )
            .expect("ExecutionCompletedPayload is infallibly serializable"),
    ]
}

/// Build the quality-gates context paragraph included in agent prompts.
///
/// Returns a formatted section if `gates` is `Some`, otherwise an empty string.
pub(crate) fn format_gates_context(gates: Option<&serde_json::Value>) -> String {
    if let Some(gates) = gates {
        format!(
            "\n\nThe following quality gates must pass after your changes:\n{}",
            serde_json::to_string_pretty(gates).unwrap_or_default()
        )
    } else {
        String::new()
    }
}

/// Produce the single simulated-success `RemediationCompleted` event used by
/// `dry_run_events()` in `remediate` and `remediate_pipeline`.
pub(crate) fn dry_run_remediation_event(
    trigger: &Event,
    cve: Option<String>,
    pipeline_fix: Option<bool>,
) -> Vec<Event> {
    let summary = cve.as_ref().map(|_| String::new());
    vec![
        trigger
            .with_payload(
                EventType::RemediationCompleted,
                &RemediationCompletedPayload {
                    cve,
                    success: true,
                    summary,
                    dry_run: Some(true),
                    pipeline_fix,
                },
            )
            .expect("RemediationCompletedPayload is infallibly serializable"),
    ]
}

/// Emit a single-event success result with a raw JSON payload, without serialization.
///
/// Use for stub/fallback paths that already have a `serde_json::Value` payload
/// (e.g., no-repo results) and don't need typed payload serialization.
pub(crate) fn stub_event_result(
    summary: impl Into<String>,
    event_type: EventType,
    project: String,
    throttle: Throttle,
    payload: serde_json::Value,
) -> TaskBlockResult {
    TaskBlockResult::success(summary, vec![Event::new(event_type, project, throttle, payload)])
}

/// Serialize `payload` and construct a `TaskBlockResult` for a gate-run event.
///
/// Absorbs the `serialize_payload().expect(...)` boilerplate shared by every
/// gate result builder, delegating final construction to [`build_gate_block_result`].
pub(crate) fn build_gate_result_from_payload(
    project: &str,
    event_type: EventType,
    success: bool,
    label: &str,
    throttle: Throttle,
    payload: &impl serde::Serialize,
) -> TaskBlockResult {
    let event_payload =
        Event::serialize_payload(payload).expect("gate result payload is infallibly serializable");
    build_gate_block_result(project, event_type, success, label, throttle, event_payload)
}

/// Construct a `TaskBlockResult` for a gate-run event.
fn build_gate_block_result(
    project: &str,
    event_type: EventType,
    success: bool,
    label: &str,
    throttle: Throttle,
    event_payload: serde_json::Value,
) -> TaskBlockResult {
    TaskBlockResult {
        events: vec![Event::new(
            event_type,
            project.to_string(),
            throttle,
            event_payload,
        )],
        success,
        summary: if success {
            format!("{project}: {label} passed")
        } else {
            format!("{project}: {label} failed")
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use foundry_core::event::{Event, EventType};
    use foundry_core::gateway::AgentOutcome;
    use foundry_core::throttle::Throttle;

    use super::{
        build_agent_remediation_result, build_gate_result_from_payload, dry_run_execution_event,
        dry_run_remediation_event, emit_event_result, emit_result, format_gates_context,
        stub_event_result,
    };

    fn full() -> Throttle {
        Throttle::Full
    }

    fn trigger_event() -> Event {
        Event::new(
            EventType::ProjectRunStarted,
            "test-proj".to_string(),
            full(),
            serde_json::json!({}),
        )
    }

    #[test]
    fn emit_result_produces_success_event_with_correct_type_and_project() {
        let result = emit_result(
            "done".to_string(),
            EventType::ProjectRunCompleted,
            "my-proj",
            full(),
            &serde_json::json!({"success": true}),
        )
        .unwrap();

        assert!(result.success);
        assert_eq!(result.summary, "done");
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectRunCompleted);
        assert_eq!(result.events[0].project, "my-proj");
        assert!(result.raw_output.is_none());
        assert!(result.exit_code.is_none());
    }

    #[test]
    fn emit_event_result_honours_success_flag() {
        let ok = emit_event_result(
            "ok".to_string(),
            true,
            EventType::ProjectRunCompleted,
            "p",
            full(),
            &serde_json::json!({}),
        )
        .unwrap();
        assert!(ok.success);

        let fail = emit_event_result(
            "fail".to_string(),
            false,
            EventType::ProjectRunCompleted,
            "p",
            full(),
            &serde_json::json!({}),
        )
        .unwrap();
        assert!(!fail.success);
    }

    #[test]
    fn build_agent_remediation_result_success_outcome() {
        let result = build_agent_remediation_result(
            "my-proj",
            full(),
            AgentOutcome::Success {
                stdout: "  patched  ".to_string(),
            },
            Some("CVE-2024-1234".to_string()),
            None,
            "Fixed",
            "Fix failed",
        );

        assert!(result.success);
        assert!(result.summary.starts_with("Fixed:"));
        assert_eq!(result.raw_output.as_deref(), Some("patched"));
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::RemediationCompleted);
        let payload = &result.events[0].payload;
        assert_eq!(payload["success"], true);
        assert_eq!(payload["cve"], "CVE-2024-1234");
    }

    #[test]
    fn build_agent_remediation_result_agent_failed_outcome() {
        let result = build_agent_remediation_result(
            "proj",
            full(),
            AgentOutcome::AgentFailed {
                stderr: "error line\nmore".to_string(),
            },
            None,
            None,
            "Done",
            "Failed",
        );

        assert!(!result.success);
        assert!(result.summary.starts_with("Failed:"));
        assert!(result.raw_output.is_some());
        let payload = &result.events[0].payload;
        assert_eq!(payload["success"], false);
    }

    #[test]
    fn build_agent_remediation_result_unavailable_outcome() {
        let result = build_agent_remediation_result(
            "proj",
            full(),
            AgentOutcome::Unavailable {
                error: "spawn failed".to_string(),
            },
            None,
            None,
            "Done",
            "Failed",
        );

        assert!(!result.success);
        assert!(result.raw_output.is_none());
    }

    #[test]
    fn dry_run_execution_event_produces_dry_run_true_execution_completed_event() {
        let trigger = trigger_event();
        let events =
            dry_run_execution_event(&trigger, foundry_core::workflow::WorkflowType::Iterate, None);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(events[0].payload["dry_run"], true);
        assert_eq!(events[0].payload["success"], true);
    }

    #[test]
    fn dry_run_remediation_event_produces_dry_run_true_remediation_completed_event() {
        let trigger = trigger_event();
        let events =
            dry_run_remediation_event(&trigger, Some("CVE-2024-5678".to_string()), Some(true));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::RemediationCompleted);
        assert_eq!(events[0].payload["dry_run"], true);
        assert_eq!(events[0].payload["success"], true);
        assert_eq!(events[0].payload["cve"], "CVE-2024-5678");
        assert_eq!(events[0].payload["pipeline_fix"], true);
    }

    #[test]
    fn format_gates_context_returns_empty_string_for_none() {
        assert_eq!(format_gates_context(None), "");
    }

    #[test]
    fn format_gates_context_returns_prefixed_block_for_some() {
        let gates = serde_json::json!([{"name": "fmt"}]);
        let out = format_gates_context(Some(&gates));
        assert!(out.contains("quality gates"));
        assert!(out.contains("fmt"));
    }

    #[test]
    fn stub_event_result_passes_payload_through_verbatim() {
        let payload = serde_json::json!({"reason": "no-repo"});
        let result = stub_event_result(
            "skipped",
            EventType::ProjectRunCompleted,
            "p".to_string(),
            full(),
            payload.clone(),
        );

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].payload["reason"], "no-repo");
    }

    #[test]
    fn build_gate_result_from_payload_formats_summary_with_passed_or_failed() {
        #[derive(serde::Serialize)]
        struct FakePayload {
            passed: bool,
        }

        let pass_result = build_gate_result_from_payload(
            "my-proj",
            EventType::ProjectRunCompleted,
            true,
            "fmt",
            full(),
            &FakePayload { passed: true },
        );
        assert!(pass_result.success);
        assert_eq!(pass_result.summary, "my-proj: fmt passed");

        let fail_result = build_gate_result_from_payload(
            "my-proj",
            EventType::ProjectRunCompleted,
            false,
            "fmt",
            full(),
            &FakePayload { passed: false },
        );
        assert!(!fail_result.success);
        assert_eq!(fail_result.summary, "my-proj: fmt failed");
    }
}
