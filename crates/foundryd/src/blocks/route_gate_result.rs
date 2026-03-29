use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Routes gate verification results to the appropriate terminal event or retry.
///
/// Observer — sinks on `GateVerificationCompleted`.
///
/// Routing logic:
/// - All required gates passed → emit `ProjectIterationCompleted` or
///   `ProjectMaintenanceCompleted` with `success: true`
/// - Failed and `retry_count < 3` → emit `RetryRequested` with incremented
///   `retry_count` and failure context
/// - Failed and retries exhausted → emit completion event with `success: false`
pub struct RouteGateResult;

impl TaskBlock for RouteGateResult {
    task_block_meta! {
        name: "Route Gate Result",
        kind: Observer,
        sinks_on: [GateVerificationCompleted],
    }

    #[allow(clippy::too_many_lines)]
    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let payload = trigger.payload.clone();

        Box::pin(async move {
            let required_passed = payload
                .get("required_passed")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);

            let retry_count =
                payload.get("retry_count").and_then(serde_json::Value::as_u64).unwrap_or(0);

            let workflow = payload
                .get("workflow")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("iterate")
                .to_string();

            let completion_event_type = if workflow == "maintain" {
                EventType::ProjectMaintenanceCompleted
            } else {
                EventType::ProjectIterationCompleted
            };

            if required_passed {
                tracing::info!(project = %project, workflow = %workflow, "all required gates passed");
                let mut event_payload = serde_json::json!({
                    "project": project,
                    "success": true,
                    "summary": "all required gates passed",
                });
                if let Some(actions) = payload.get("actions") {
                    event_payload["actions"] = actions.clone();
                }

                let mut events = vec![Event::new(
                    completion_event_type,
                    project.clone(),
                    throttle,
                    event_payload,
                )];

                // Chain to maintenance if actions.maintain=true and this is an iterate workflow
                if workflow == "iterate" {
                    let maintain = payload
                        .get("actions")
                        .and_then(|a| a.get("maintain"))
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    if maintain {
                        tracing::info!(project = %project, "iterate succeeded with maintain=true, chaining to maintenance");
                        events.push(Event::new(
                            EventType::MaintenanceRequested,
                            project.clone(),
                            throttle,
                            serde_json::json!({ "project": project }),
                        ));
                    }
                }

                return Ok(TaskBlockResult {
                    events,
                    success: true,
                    summary: format!("{project}: all required gates passed"),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                });
            }

            // Required gates failed
            let max_retries: u64 = 3;
            if retry_count < max_retries {
                let failure_context = build_failure_context(&payload);
                tracing::info!(
                    project = %project,
                    retry_count = retry_count,
                    "gates failed, requesting retry"
                );

                let mut event_payload = serde_json::json!({
                    "project": project,
                    "workflow": workflow,
                    "retry_count": retry_count + 1,
                    "failure_context": failure_context,
                });
                if let Some(actions) = payload.get("actions") {
                    event_payload["actions"] = actions.clone();
                }

                return Ok(TaskBlockResult {
                    events: vec![Event::new(
                        EventType::RetryRequested,
                        project.clone(),
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
                });
            }

            // Retries exhausted
            tracing::warn!(
                project = %project,
                retry_count = retry_count,
                "gates failed, retries exhausted"
            );

            let mut event_payload = serde_json::json!({
                "project": project,
                "success": false,
                "summary": format!("gates failed after {retry_count} retries"),
            });
            if let Some(actions) = payload.get("actions") {
                event_payload["actions"] = actions.clone();
            }

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    completion_event_type,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
                success: false,
                summary: format!("{project}: gates failed after {retry_count} retries"),
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
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
