use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gateway::AgentFailureMetadata;
use foundry_sdk::payload::{ExecutionCompletedPayload, LoopContext, RemediationCompletedPayload};
use foundry_sdk::task_block::TaskBlockResult;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::AgentOutcome;

/// Serialize `payload` and construct a single `Event`, propagating serialization errors.
///
/// Core primitive for callers that build multi-event results or return bare `Event` values
/// (e.g., multi-event `Vec<Event>` builders). Use [`emit_result`] or [`emit_event_result`]
/// when the result wraps a single event in a `TaskBlockResult`.
pub(crate) fn event_from_payload(
    event_type: EventType,
    project: &str,
    throttle: Throttle,
    payload: &impl serde::Serialize,
) -> anyhow::Result<Event> {
    let event_payload = Event::serialize_payload(payload)?;
    Ok(Event::new(event_type, project.to_string(), throttle, event_payload))
}

/// Serialize `payload` and construct a single `Event`, panicking on serialization failure.
///
/// Use for infallibly-serializable payload types where construction is unconditional.
/// All standard `*Payload` SDK types qualify. See [`event_from_payload`] for the
/// fallible variant.
pub(crate) fn event_from_infallible_payload(
    event_type: EventType,
    project: &str,
    throttle: Throttle,
    payload: &impl serde::Serialize,
) -> Event {
    #[allow(
        clippy::expect_used,
        reason = "payload is infallibly serializable (Payload Conventions, AGENTS.md)"
    )]
    let event_payload =
        Event::serialize_payload(payload).expect("payload is infallibly serializable");
    Event::new(event_type, project.to_string(), throttle, event_payload)
}

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
    let event = event_from_payload(event_type, project, throttle, payload)?;
    Ok(TaskBlockResult {
        events: vec![event],
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

/// Build a single `RemediationCompleted` event.
///
/// Single source of truth for the `EventType::RemediationCompleted` +
/// `RemediationCompletedPayload` pairing — called by both
/// [`dry_run_remediation_event`] and [`build_agent_remediation_result`].
fn remediation_completed_event(
    project: &str,
    throttle: Throttle,
    cve: Option<String>,
    pipeline_fix: Option<bool>,
    success: bool,
    summary: Option<String>,
    dry_run: Option<bool>,
) -> Event {
    event_from_infallible_payload(
        EventType::RemediationCompleted,
        project,
        throttle,
        &RemediationCompletedPayload {
            cve,
            success,
            summary,
            dry_run,
            pipeline_fix,
        },
    )
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
        AgentOutcome::AgentFailed { stderr, failure } => {
            let summary = failure
                .as_ref()
                .filter(|failure| failure.is_terminal_provider_failure())
                .map_or_else(
                    || {
                        let first_line = stderr.lines().next().unwrap_or("agent failed");
                        format!("remediation failed: {first_line}")
                    },
                    AgentFailureMetadata::execution_summary,
                );
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

    TaskBlockResult {
        events: vec![remediation_completed_event(
            project,
            throttle,
            cve,
            pipeline_fix,
            success,
            Some(summary.clone()),
            None,
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

/// Build a single-element `Vec<Event>` for use in `dry_run_events()` implementations.
///
/// Encapsulates the `trigger.with_payload(event_type, payload).expect("…")` pattern
/// that would otherwise be duplicated in every Mutator block's `dry_run_events`.
pub(crate) fn dry_run_single_event(
    trigger: &Event,
    event_type: EventType,
    payload: &impl serde::Serialize,
) -> Vec<Event> {
    #[allow(
        clippy::expect_used,
        reason = "dry-run payload is infallibly serializable (Payload Conventions, AGENTS.md)"
    )]
    let event = trigger
        .with_payload(event_type, payload)
        .expect("dry-run payload is infallibly serializable");
    vec![event]
}

/// Build a single `ExecutionCompleted` event.
///
/// Single source of truth for the `EventType::ExecutionCompleted` +
/// `ExecutionCompletedPayload` pairing — called by both
/// [`dry_run_execution_event`] and the real-execution path in
/// [`super::execution::build_agent_execution_result`].
pub(crate) fn execution_completed_event(
    project: &str,
    throttle: Throttle,
    payload: &ExecutionCompletedPayload,
) -> Event {
    event_from_infallible_payload(EventType::ExecutionCompleted, project, throttle, payload)
}

/// Produce the single simulated-success `ExecutionCompleted` event used by
/// `dry_run_events()` across all three execution blocks.
///
/// Encapsulates `LoopContext::extract_from`, `ExecutionCompletedPayload` construction,
/// and payload serialization via [`execution_completed_event`].
pub(crate) fn dry_run_execution_event(
    trigger: &Event,
    workflow: WorkflowType,
    retry_count: Option<u64>,
) -> Vec<Event> {
    let context = LoopContext::extract_from(&trigger.payload);
    vec![execution_completed_event(
        &trigger.project,
        trigger.throttle,
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
            failure: AgentFailureMetadata::default(),
            context,
        },
    )]
}

/// Build the quality-gates context paragraph included in agent prompts.
///
/// Returns a formatted section if `gates` is `Some`, otherwise an empty string.
pub(crate) fn format_gates_context(gates: Option<&serde_json::Value>) -> String {
    if let Some(gates) = gates {
        #[allow(
            clippy::expect_used,
            reason = "gate results are infallibly serializable: no non-string map keys or non-finite floats"
        )]
        let rendered =
            serde_json::to_string_pretty(gates).expect("gate results are infallibly serializable");
        format!("\n\nThe following quality gates must pass after your changes:\n{rendered}")
    } else {
        String::new()
    }
}

/// Produce the single simulated-success `RemediationCompleted` event used by
/// `dry_run_events()` in `remediate` and `remediate_pipeline`.
///
/// Delegates to [`remediation_completed_event`] so the
/// `EventType::RemediationCompleted` + `RemediationCompletedPayload` pairing
/// has a single construction site.
pub(crate) fn dry_run_remediation_event(
    trigger: &Event,
    cve: Option<String>,
    pipeline_fix: Option<bool>,
) -> Vec<Event> {
    let summary = cve.as_ref().map(|_| String::new());
    vec![remediation_completed_event(
        &trigger.project,
        trigger.throttle,
        cve,
        pipeline_fix,
        true,
        summary,
        Some(true),
    )]
}

/// Emit a single-event success result from a raw JSON payload, without serialization.
///
/// Use when the payload is already a `serde_json::Value` and typed serialization
/// is not needed — for example, when forwarding an assembled payload verbatim.
pub(crate) fn single_event_result(
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
    #[allow(
        clippy::expect_used,
        reason = "gate result payload is infallibly serializable (Payload Conventions, AGENTS.md)"
    )]
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
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::gateway::AgentOutcome;
    use foundry_sdk::throttle::Throttle;

    use super::{
        build_agent_remediation_result, build_gate_result_from_payload, dry_run_execution_event,
        dry_run_remediation_event, dry_run_single_event, emit_event_result, emit_result,
        event_from_infallible_payload, event_from_payload, execution_completed_event,
        format_gates_context, single_event_result,
    };

    fn full() -> Throttle {
        Throttle::Full
    }

    fn trigger_event() -> Event {
        test_event!(EventType::ProjectRunStarted, "test-proj", {})
    }

    #[test]
    fn event_from_payload_produces_event_with_correct_type_project_and_payload() {
        let event = event_from_payload(
            EventType::ProjectRunCompleted,
            "my-proj",
            full(),
            &serde_json::json!({"success": true}),
        )
        .unwrap();

        assert_eq!(event.event_type, EventType::ProjectRunCompleted);
        assert_eq!(event.project, "my-proj");
        assert_eq!(event.payload["success"], true);
    }

    #[test]
    fn event_from_infallible_payload_produces_event_with_correct_type_and_project() {
        let event = event_from_infallible_payload(
            EventType::ProjectRunCompleted,
            "my-proj",
            full(),
            &serde_json::json!({"status": "ok"}),
        );

        assert_eq!(event.event_type, EventType::ProjectRunCompleted);
        assert_eq!(event.project, "my-proj");
        assert_eq!(event.payload["status"], "ok");
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
                failure: None,
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
            dry_run_execution_event(&trigger, foundry_sdk::workflow::WorkflowType::Iterate, None);

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
    fn single_event_result_passes_payload_through_verbatim() {
        let payload = serde_json::json!({"reason": "no-repo"});
        let result = single_event_result(
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

    #[test]
    fn dry_run_single_event_returns_exactly_one_event_of_the_given_type() {
        let trigger = trigger_event();
        let events = dry_run_single_event(
            &trigger,
            EventType::ProjectRunCompleted,
            &serde_json::json!({"success": true, "dry_run": true}),
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::ProjectRunCompleted);
        assert_eq!(events[0].payload["dry_run"], true);
    }

    #[test]
    fn execution_completed_event_produces_event_with_correct_type_and_project() {
        use foundry_sdk::gateway::AgentFailureMetadata;
        use foundry_sdk::payload::{ExecutionCompletedPayload, LoopContext};
        let payload = ExecutionCompletedPayload {
            project: "p".to_string(),
            workflow: "iterate".to_string(),
            success: true,
            summary: String::new(),
            execution_output: None,
            dry_run: Some(true),
            retry_count: None,
            changes_detected: None,
            files_changed: vec![],
            failure: AgentFailureMetadata::default(),
            context: LoopContext::extract_from(&serde_json::json!({})),
        };
        let event = execution_completed_event("p", full(), &payload);
        assert_eq!(event.event_type, EventType::ExecutionCompleted);
        assert_eq!(event.project, "p");
        assert_eq!(event.payload["success"], true);
        assert_eq!(event.payload["dry_run"], true);
    }

    /// Verify that `dry_run_execution_event` and the real-execution path both agree
    /// on emitting `EventType::ExecutionCompleted` — structural guarantee from the
    /// single `execution_completed_event` builder.
    #[test]
    fn dry_run_execution_event_and_execution_completed_event_agree_on_type() {
        use foundry_sdk::gateway::AgentFailureMetadata;
        use foundry_sdk::payload::{ExecutionCompletedPayload, LoopContext};
        let trigger = trigger_event();
        let dry_run_events =
            dry_run_execution_event(&trigger, foundry_sdk::workflow::WorkflowType::Iterate, None);
        assert_eq!(dry_run_events[0].event_type, EventType::ExecutionCompleted);

        // Real path: build via execution_completed_event directly.
        let real_payload = ExecutionCompletedPayload {
            project: "test-proj".to_string(),
            workflow: "iterate".to_string(),
            success: true,
            summary: "done".to_string(),
            execution_output: None,
            dry_run: None,
            retry_count: None,
            changes_detected: Some(true),
            files_changed: vec![],
            failure: AgentFailureMetadata::default(),
            context: LoopContext::extract_from(&serde_json::json!({})),
        };
        let real_event = execution_completed_event("test-proj", full(), &real_payload);
        assert_eq!(real_event.event_type, EventType::ExecutionCompleted);
        assert_eq!(dry_run_events[0].event_type, real_event.event_type);
    }

    /// Verify that `dry_run_remediation_event` and `build_agent_remediation_result` both
    /// agree on emitting `EventType::RemediationCompleted` — structural guarantee from
    /// the single `remediation_completed_event` builder.
    #[test]
    fn dry_run_and_build_agent_remediation_agree_on_event_type() {
        let trigger = trigger_event();
        let dry_run_events = dry_run_remediation_event(&trigger, Some("CVE-1".to_string()), None);
        assert_eq!(dry_run_events[0].event_type, EventType::RemediationCompleted);

        let real_result = build_agent_remediation_result(
            "test-proj",
            full(),
            AgentOutcome::Success {
                stdout: "fixed".to_string(),
            },
            Some("CVE-1".to_string()),
            None,
            "Fixed",
            "Failed",
        );
        assert_eq!(real_result.events[0].event_type, EventType::RemediationCompleted);
        assert_eq!(dry_run_events[0].event_type, real_result.events[0].event_type);
    }
}
