use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::loop_context::has_loop_context;
use foundry_core::payload::{
    ChainContext, GateVerificationCompletedPayload, MaintenanceRequestedPayload,
    ProjectCompletedPayload, RetryRequestedPayload,
};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::workflow::WorkflowType;

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

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let TriggerContext {
            project,
            throttle,
            payload,
        } = TriggerContext::from_trigger(trigger);

        let p = match trigger.parse_payload::<GateVerificationCompletedPayload>() {
            Ok(p) => p,
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        let required_passed = p.required_passed;
        let retry_count = p.retry_count;

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
                    &payload,
                    throttle,
                )
            } else {
                handle_retry_or_exhaustion(
                    &project,
                    workflow,
                    completion_event_type,
                    retry_count,
                    &payload,
                    throttle,
                )
            };

            Ok(result)
        })
    }
}

fn handle_gates_passed(
    project: &str,
    workflow: WorkflowType,
    completion_event_type: foundry_core::event::EventType,
    in_loop: bool,
    payload: &serde_json::Value,
    throttle: foundry_core::throttle::Throttle,
) -> TaskBlockResult {
    tracing::info!(project = %project, workflow = %workflow, "all required gates passed");

    // Carry loop_context forward into the completion event so downstream blocks can see it
    let loop_context = payload.get("loop_context").cloned();
    let completion_payload = Event::serialize_payload(&ProjectCompletedPayload {
        project: project.to_string(),
        success: true,
        summary: "all required gates passed".to_string(),
        workflow: workflow.to_string(),
        loop_context,
    })
    .expect("ProjectCompletedPayload is infallibly serializable");

    let mut events = vec![Event::new(
        completion_event_type,
        project.to_string(),
        throttle,
        completion_payload,
    )];

    // Chain to maintenance if actions.maintain=true and this is an iterate workflow.
    // Skip chaining when inside a nested loop — the strategic loop controller
    // handles post-loop maintenance chaining.
    if workflow == WorkflowType::Iterate && !in_loop {
        let maintain = payload
            .get("actions")
            .and_then(|a| a.get("maintain"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if maintain {
            tracing::info!(project = %project, "iterate succeeded with maintain=true, chaining to maintenance");
            let maintenance_payload = Event::serialize_payload(&MaintenanceRequestedPayload {
                project: project.to_string(),
                workflow: WorkflowType::Maintain.to_string(),
                chain: ChainContext::default(),
            })
            .expect("MaintenanceRequestedPayload is infallibly serializable");
            events.push(Event::new(
                EventType::MaintenanceRequested,
                project.to_string(),
                throttle,
                maintenance_payload,
            ));
        }
    }

    TaskBlockResult::success(format!("{project}: all required gates passed"), events)
}

fn handle_retry_or_exhaustion(
    project: &str,
    workflow: WorkflowType,
    completion_event_type: foundry_core::event::EventType,
    retry_count: u64,
    payload: &serde_json::Value,
    throttle: foundry_core::throttle::Throttle,
) -> TaskBlockResult {
    let max_retries: u64 = 3;
    if retry_count < max_retries {
        let failure_context = build_failure_context(payload);
        tracing::info!(
            project = %project,
            retry_count = retry_count,
            "gates failed, requesting retry"
        );

        let prior_execution_output = payload
            .get("execution_output")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let context = foundry_core::payload::LoopContext::extract_from(payload);
        let event_payload = Event::serialize_payload(&RetryRequestedPayload {
            project: project.to_string(),
            workflow: workflow.to_string(),
            retry_count: retry_count + 1,
            failure_context,
            prior_execution_output,
            context,
        })
        .expect("RetryRequestedPayload is infallibly serializable");

        return TaskBlockResult {
            events: vec![Event::new(
                EventType::RetryRequested,
                project.to_string(),
                throttle,
                event_payload,
            )],
            success: false,
            summary: format!(
                "{project}: gates failed, retry {}/{max_retries} requested",
                retry_count + 1
            ),
            raw_output: None,
            exit_code: None,
            audit_artifacts: vec![],
        };
    }

    // Retries exhausted
    tracing::warn!(
        project = %project,
        retry_count = retry_count,
        "gates failed, retries exhausted"
    );

    let loop_context = payload.get("loop_context").cloned();
    let event_payload = Event::serialize_payload(&ProjectCompletedPayload {
        project: project.to_string(),
        success: false,
        summary: format!("gates failed after {retry_count} retries"),
        workflow: workflow.to_string(),
        loop_context,
    })
    .expect("ProjectCompletedPayload is infallibly serializable");

    TaskBlockResult {
        events: vec![Event::new(
            completion_event_type,
            project.to_string(),
            throttle,
            event_payload,
        )],
        success: false,
        summary: format!("{project}: gates failed after {retry_count} retries"),
        raw_output: None,
        exit_code: None,
        audit_artifacts: vec![],
    }
}

/// Build a summary of gate failures from the verification payload.
fn build_failure_context(payload: &serde_json::Value) -> String {
    let Some(results) = payload.get("results").and_then(serde_json::Value::as_array) else {
        return "no gate results available".to_string();
    };

    let failures: Vec<String> = results
        .iter()
        .filter(|r| !r.get("passed").and_then(serde_json::Value::as_bool).unwrap_or(true))
        .map(|r| {
            let name = r.get("name").and_then(serde_json::Value::as_str).unwrap_or("unknown");
            let output = r.get("output").and_then(serde_json::Value::as_str).unwrap_or("");
            if output.is_empty() {
                name.to_string()
            } else {
                format!("{name}: {output}")
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
    use foundry_core::event::{Event, EventType};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use super::RouteGateResult;

    fn verification_event(
        project: &str,
        required_passed: bool,
        retry_count: u64,
        workflow: &str,
    ) -> Event {
        Event::new(
            EventType::GateVerificationCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
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
            }),
        )
    }

    #[test]
    fn kind_is_observer() {
        assert_eq!(RouteGateResult.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_gate_verification_completed() {
        assert_eq!(RouteGateResult.sinks_on(), &[EventType::GateVerificationCompleted]);
    }

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
        let trigger = Event::new(
            EventType::GateVerificationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "required_passed": true,
                "all_passed": true,
                "retry_count": 0,
                "workflow": "iterate",
                "actions": {"maintain": true},
                "results": [],
            }),
        );
        let result = RouteGateResult.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        assert_eq!(result.events[1].event_type, EventType::MaintenanceRequested);
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
        let trigger = Event::new(
            EventType::GateVerificationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "required_passed": true,
                "all_passed": true,
                "retry_count": 0,
                "workflow": "maintain",
                "actions": {"maintain": true},
                "results": [],
            }),
        );
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
        Event::new(
            EventType::GateVerificationCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
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
            }),
        )
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
        // Only one event — no MaintenanceRequested chaining inside a loop
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
