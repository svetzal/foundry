use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gates::GateResult;
use foundry_sdk::gateway::AgentFailureMetadata;
use foundry_sdk::loop_context::has_loop_context;
use foundry_sdk::payload::{
    ChainContext, GateVerificationCompletedPayload, LoopContext, ProjectCompletedPayload,
    ProjectMaintenanceRequestedPayload, RetryRequestedPayload,
};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::throttle::Throttle;
use foundry_sdk::workflow::WorkflowType;

use super::TriggerContext;

/// Routes gate verification results to the appropriate terminal event or retry.
///
/// Observer — sinks on `GateVerificationCompleted`.
///
/// Routing logic:
/// - All required gates passed → emit completion event with `success: true`
/// - Failed and `retry_count < 3` → emit `RetryRequested` with incremented
///   `retry_count` and failure context
/// - Failed and retries exhausted → emit completion event with `success: false`
///
/// Loop awareness: when `loop_context` is present in the payload and the
/// workflow is `iterate`, emits `InnerIterationCompleted` instead of
/// `ProjectIterationCompleted`. This allows the strategic loop controller
/// to decide whether to continue iterating. Maintenance chaining is also
/// suppressed inside a loop.
pub struct RouteGateResult;

impl TaskBlock for RouteGateResult {
    task_block_meta! {
        name: "Route Gate Result",
        kind: Observer,
        sinks_on: [GateVerificationCompleted],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        WorkflowType::from_payload(&trigger.payload) != WorkflowType::Task
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project,
            throttle,
            payload,
        } = TriggerContext::from_trigger(trigger);

        let p = parse_payload!(trigger, GateVerificationCompletedPayload);
        let required_passed = p.required_passed;
        let retry_count = p.retry_count;
        let results = p.results;
        let execution_output = p.execution_output;
        let failure = p.failure;
        let context = p.context;

        Box::pin(async move {
            let workflow = WorkflowType::from_payload(&payload);
            let in_loop = has_loop_context(&payload);

            let completion_event_type = if workflow == WorkflowType::Maintain {
                EventType::ProjectMaintenanceCompleted
            } else if in_loop {
                // Inside a nested loop — emit InnerIterationCompleted so the
                // strategic loop controller can decide whether to continue.
                EventType::InnerIterationCompleted
            } else {
                EventType::ProjectIterationCompleted
            };

            let result = if required_passed {
                handle_gates_passed(
                    &project,
                    workflow,
                    completion_event_type,
                    in_loop,
                    &context,
                    throttle,
                )
            } else {
                handle_retry_or_exhaustion(RetryDecision {
                    project: &project,
                    workflow,
                    completion_event_type,
                    retry_count,
                    results: &results,
                    execution_output,
                    failure,
                    context,
                    throttle,
                })
            };

            Ok(result)
        })
    }
}

fn handle_gates_passed(
    project: &str,
    workflow: WorkflowType,
    completion_event_type: foundry_sdk::event::EventType,
    in_loop: bool,
    context: &LoopContext,
    throttle: foundry_sdk::throttle::Throttle,
) -> TaskBlockResult {
    tracing::info!(project = %project, workflow = %workflow, "all required gates passed");

    // Carry loop_context forward into the completion event so downstream blocks can see it
    let loop_context = context.loop_context.clone();
    let mut events = vec![super::event_from_infallible_payload(
        completion_event_type,
        project,
        throttle,
        &ProjectCompletedPayload {
            project: project.to_string(),
            success: true,
            summary: "all required gates passed".to_string(),
            workflow: workflow.to_string(),
            loop_context,
            changes: None,
            failure: AgentFailureMetadata::default(),
        },
    )];

    // Chain to maintenance if actions.maintain=true and this is an iterate workflow.
    // Skip chaining when inside a nested loop — the strategic loop controller
    // handles post-loop maintenance chaining.
    if workflow == WorkflowType::Iterate && !in_loop {
        let maintain = context
            .actions
            .as_ref()
            .and_then(|a| a.get("maintain"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if maintain {
            tracing::info!(project = %project, "iterate succeeded with maintain=true, chaining to maintenance");
            events.push(super::event_from_infallible_payload(
                EventType::ProjectMaintenanceRequested,
                project,
                throttle,
                &ProjectMaintenanceRequestedPayload {
                    project: project.to_string(),
                    workflow: WorkflowType::Maintain.to_string(),
                    chain: ChainContext::default(),
                },
            ));
        }
    }

    TaskBlockResult::success(format!("{project}: all required gates passed"), events)
}

struct RetryDecision<'a> {
    project: &'a str,
    workflow: WorkflowType,
    completion_event_type: EventType,
    retry_count: u64,
    results: &'a [GateResult],
    execution_output: Option<String>,
    failure: foundry_sdk::gateway::AgentFailureMetadata,
    context: LoopContext,
    throttle: Throttle,
}

fn handle_retry_or_exhaustion(decision: RetryDecision<'_>) -> TaskBlockResult {
    let RetryDecision {
        project,
        workflow,
        completion_event_type,
        retry_count,
        results,
        execution_output,
        failure,
        context,
        throttle,
    } = decision;
    let max_retries: u64 = 3;
    if failure.is_terminal_provider_failure() {
        tracing::warn!(
            project = %project,
            retry_count = retry_count,
            summary = %failure.stop_summary(),
            "terminal agent/provider failure detected; skipping retries"
        );

        let loop_context = context.loop_context;
        return super::emit_event_result(
            format!("{project}: {}", failure.stop_summary()),
            false,
            completion_event_type,
            project,
            throttle,
            &ProjectCompletedPayload {
                project: project.to_string(),
                success: false,
                summary: failure.stop_summary(),
                workflow: workflow.to_string(),
                loop_context,
                changes: None,
                failure,
            },
        )
        .expect("ProjectCompletedPayload is infallibly serializable");
    }

    if retry_count < max_retries {
        let failure_context = build_failure_context(results);
        tracing::info!(
            project = %project,
            retry_count = retry_count,
            "gates failed, requesting retry"
        );

        return super::emit_event_result(
            format!("{project}: gates failed, retry {}/{max_retries} requested", retry_count + 1),
            false,
            EventType::RetryRequested,
            project,
            throttle,
            &RetryRequestedPayload {
                project: project.to_string(),
                workflow: workflow.to_string(),
                retry_count: retry_count + 1,
                failure_context,
                prior_execution_output: execution_output,
                context,
            },
        )
        .expect("RetryRequestedPayload is infallibly serializable");
    }

    // Retries exhausted
    tracing::warn!(
        project = %project,
        retry_count = retry_count,
        "gates failed, retries exhausted"
    );

    let loop_context = context.loop_context;
    super::emit_event_result(
        format!("{project}: gates failed after {retry_count} retries"),
        false,
        completion_event_type,
        project,
        throttle,
        &ProjectCompletedPayload {
            project: project.to_string(),
            success: false,
            summary: format!("gates failed after {retry_count} retries"),
            workflow: workflow.to_string(),
            loop_context,
            changes: None,
            failure,
        },
    )
    .expect("ProjectCompletedPayload is infallibly serializable")
}

/// Build a summary of gate failures from the gate results slice.
fn build_failure_context(results: &[foundry_sdk::gates::GateResult]) -> String {
    let failures: Vec<String> = results
        .iter()
        .filter(|r| !r.passed)
        .map(|r| {
            if r.output.is_empty() {
                r.name.clone()
            } else {
                format!("{}: {}", r.name, r.output)
            }
        })
        .collect();

    if failures.is_empty() {
        "no specific failures identified".to_string()
    } else {
        failures.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::task_block::TaskBlock;

    use super::RouteGateResult;

    fn verification_event(
        project: &str,
        required_passed: bool,
        retry_count: u64,
        workflow: &str,
    ) -> Event {
        test_event!(EventType::GateVerificationCompleted, project, {
            "project": project,
            "required_passed": required_passed,
            "all_passed": required_passed,
            "retry_count": retry_count,
            "workflow": workflow,
            "results": [
                {
                    "name": "fmt",
                    "command": "cargo fmt --check",
                    "passed": required_passed,
                    "required": true,
                    "output": if required_passed { "" } else { "formatting error" },
                    "exit_code": i32::from(!required_passed),
                }
            ],
        })
    }

    fn terminal_failure_event(project: &str, workflow: &str) -> Event {
        test_event!(EventType::GateVerificationCompleted, project, {
            "project": project,
            "required_passed": false,
            "all_passed": false,
            "retry_count": 0,
            "workflow": workflow,
            "results": [
                {
                    "name": "agent_execution",
                    "command": "",
                    "passed": false,
                    "required": true,
                    "output": "agent account limit reached",
                    "exit_code": 1,
                }
            ],
            "api_error_status": 429,
            "failure_kind": "account_limit",
            "terminal": true,
            "message": "You've hit your monthly spend limit - raise it at claude.ai/settings/usage",
        })
    }

    assert_block_meta!(
        RouteGateResult,
        kind: Observer,
        sinks_on: [GateVerificationCompleted],
    );

    // --- iterate workflow ---

    #[tokio::test]
    async fn iterate_all_passed_emits_completion() {
        let trigger = verification_event("my-project", true, 0, "iterate");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        assert_eq!(result.events[0].payload["success"], true);
    }

    #[tokio::test]
    async fn iterate_failed_with_retries_remaining_emits_retry() {
        let trigger = verification_event("my-project", false, 1, "iterate");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::RetryRequested);
        assert_eq!(result.events[0].payload["retry_count"], 2);
        assert_eq!(result.events[0].payload["workflow"], "iterate");
    }

    #[tokio::test]
    async fn iterate_failed_retries_exhausted_emits_failure() {
        let trigger = verification_event("my-project", false, 3, "iterate");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        assert_eq!(result.events[0].payload["success"], false);
    }

    #[tokio::test]
    async fn terminal_provider_failure_emits_completion_without_retry() {
        let trigger = terminal_failure_event("my-project", "iterate");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        assert_eq!(result.events[0].payload["success"], false);
        assert_eq!(result.events[0].payload["failure_kind"], "account_limit");
        assert_eq!(result.events[0].payload["terminal"], true);
        assert_eq!(
            result.events[0].payload["summary"],
            "agent account limit reached; workflow stopped"
        );
    }

    // --- maintain workflow ---

    #[tokio::test]
    async fn maintain_all_passed_emits_completion() {
        let trigger = verification_event("my-project", true, 0, "maintain");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectMaintenanceCompleted);
        assert_eq!(result.events[0].payload["success"], true);
    }

    #[tokio::test]
    async fn maintain_failed_with_retries_remaining_emits_retry() {
        let trigger = verification_event("my-project", false, 0, "maintain");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::RetryRequested);
        assert_eq!(result.events[0].payload["retry_count"], 1);
        assert_eq!(result.events[0].payload["workflow"], "maintain");
    }

    #[tokio::test]
    async fn maintain_failed_retries_exhausted_emits_failure() {
        let trigger = verification_event("my-project", false, 3, "maintain");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectMaintenanceCompleted);
        assert_eq!(result.events[0].payload["success"], false);
    }

    #[tokio::test]
    async fn iterate_success_with_maintain_true_chains_maintenance() {
        let trigger = test_event!(EventType::GateVerificationCompleted, "my-project", {
            "project": "my-project",
            "required_passed": true,
            "all_passed": true,
            "retry_count": 0,
            "workflow": "iterate",
            "actions": {"maintain": true},
            "results": [],
        });
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        assert_eq!(result.events[1].event_type, EventType::ProjectMaintenanceRequested);
    }

    #[tokio::test]
    async fn iterate_success_without_maintain_does_not_chain() {
        let trigger = verification_event("my-project", true, 0, "iterate");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
    }

    #[tokio::test]
    async fn maintain_success_does_not_chain_maintenance() {
        let trigger = test_event!(EventType::GateVerificationCompleted, "my-project", {
            "project": "my-project",
            "required_passed": true,
            "all_passed": true,
            "retry_count": 0,
            "workflow": "maintain",
            "actions": {"maintain": true},
            "results": [],
        });
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectMaintenanceCompleted);
    }

    // --- loop-aware behaviour ---

    fn verification_event_with_loop_context(
        project: &str,
        required_passed: bool,
        retry_count: u64,
        workflow: &str,
    ) -> Event {
        test_event!(EventType::GateVerificationCompleted, project, {
            "project": project,
            "required_passed": required_passed,
            "all_passed": required_passed,
            "retry_count": retry_count,
            "workflow": workflow,
            "results": [],
            "loop_context": {
                "strategic": { "iteration": 1, "max": 5 }
            },
            "actions": { "maintain": true },
        })
    }

    #[tokio::test]
    async fn iterate_with_loop_context_emits_inner_iteration_completed() {
        let trigger = verification_event_with_loop_context("my-project", true, 0, "iterate");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::InnerIterationCompleted);
        assert_eq!(result.events[0].payload["success"], true);
        // loop_context should be forwarded
        assert!(result.events[0].payload.get("loop_context").is_some());
    }

    #[tokio::test]
    async fn iterate_with_loop_context_does_not_chain_maintenance() {
        let trigger = verification_event_with_loop_context("my-project", true, 0, "iterate");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(result.success);
        // Only one event — no ProjectMaintenanceRequested chaining inside a loop
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::InnerIterationCompleted);
    }

    #[tokio::test]
    async fn iterate_with_loop_context_failed_retries_exhausted() {
        let trigger = verification_event_with_loop_context("my-project", false, 3, "iterate");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        // Even on failure with exhausted retries, should use InnerIterationCompleted
        assert_eq!(result.events[0].event_type, EventType::InnerIterationCompleted);
        assert_eq!(result.events[0].payload["success"], false);
    }

    #[tokio::test]
    async fn maintain_with_loop_context_still_uses_maintenance_completed() {
        let trigger = verification_event_with_loop_context("my-project", true, 0, "maintain");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        // Maintenance workflow is not nested — still uses ProjectMaintenanceCompleted
        assert!(result.success);
        assert_eq!(result.events[0].event_type, EventType::ProjectMaintenanceCompleted);
    }

    #[tokio::test]
    async fn failure_context_includes_gate_output() {
        let trigger = verification_event("my-project", false, 0, "iterate");
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        let context = result.events[0]
            .payload
            .get("failure_context")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(context.contains("formatting error"));
    }
}
