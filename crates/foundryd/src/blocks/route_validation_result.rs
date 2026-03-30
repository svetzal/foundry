use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Routes preflight results for the validation workflow to `ValidationCompleted`.
///
/// Observer — sinks on `PreflightCompleted`.
///
/// Self-filter: only handles `workflow == "validate"`. Events from other
/// workflows (iterate, maintain) are ignored so their own chains continue
/// unaffected.
///
/// Emits `ValidationCompleted` with per-gate results and overall success.
pub struct RouteValidationResult;

impl TaskBlock for RouteValidationResult {
    task_block_meta! {
        name: "Route Validation Result",
        kind: Observer,
        sinks_on: [PreflightCompleted],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let payload = trigger.payload.clone();

        Box::pin(async move {
            let workflow =
                payload.get("workflow").and_then(serde_json::Value::as_str).unwrap_or("unknown");

            // Self-filter: only handle validation workflow
            if workflow != "validate" {
                return Ok(TaskBlockResult::success(
                    format!("{project}: skipped (workflow={workflow}, not validate)"),
                    vec![],
                ));
            }

            let required_passed = payload
                .get("required_passed")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);

            let results = payload.get("results").cloned().unwrap_or(serde_json::json!([]));

            let event_payload = serde_json::json!({
                "project": project,
                "success": required_passed,
                "results": results,
            });

            let summary = if required_passed {
                format!("{project}: validation passed")
            } else {
                format!("{project}: validation failed")
            };

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ValidationCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
                success: required_passed,
                summary,
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use foundry_core::event::{Event, EventType};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use super::RouteValidationResult;

    fn preflight_event(
        project: &str,
        workflow: &str,
        required_passed: bool,
        all_passed: bool,
        results: &serde_json::Value,
    ) -> Event {
        Event::new(
            EventType::PreflightCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "workflow": workflow,
                "required_passed": required_passed,
                "all_passed": all_passed,
                "results": results,
            }),
        )
    }

    #[test]
    fn kind_is_observer() {
        assert_eq!(RouteValidationResult.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_preflight_completed() {
        assert_eq!(RouteValidationResult.sinks_on(), &[EventType::PreflightCompleted]);
    }

    #[tokio::test]
    async fn ignores_iterate_workflow() {
        let trigger = preflight_event("my-project", "iterate", true, true, &serde_json::json!([]));
        let result = RouteValidationResult.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty(), "should not emit for iterate workflow");
    }

    #[tokio::test]
    async fn ignores_maintain_workflow() {
        let trigger = preflight_event("my-project", "maintain", true, true, &serde_json::json!([]));
        let result = RouteValidationResult.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty(), "should not emit for maintain workflow");
    }

    #[tokio::test]
    async fn gates_pass_emits_success() {
        let results = serde_json::json!([
            {"name": "fmt", "command": "cargo fmt --check", "passed": true, "required": true, "output": "", "exit_code": 0},
            {"name": "clippy", "command": "cargo clippy", "passed": true, "required": true, "output": "", "exit_code": 0},
        ]);
        let trigger = preflight_event("my-project", "validate", true, true, &results);
        let result = RouteValidationResult.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ValidationCompleted);
        assert_eq!(result.events[0].payload["success"], true);
        assert_eq!(result.events[0].payload["project"], "my-project");
        let gate_results = result.events[0].payload["results"].as_array().unwrap();
        assert_eq!(gate_results.len(), 2);
    }

    #[tokio::test]
    async fn required_gate_fails_emits_failure() {
        let results = serde_json::json!([
            {"name": "fmt", "command": "cargo fmt --check", "passed": false, "required": true, "output": "formatting error", "exit_code": 1},
        ]);
        let trigger = preflight_event("my-project", "validate", false, false, &results);
        let result = RouteValidationResult.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ValidationCompleted);
        assert_eq!(result.events[0].payload["success"], false);
    }

    #[tokio::test]
    async fn optional_gate_fails_still_success() {
        let results = serde_json::json!([
            {"name": "fmt", "command": "cargo fmt --check", "passed": true, "required": true, "output": "", "exit_code": 0},
            {"name": "lint-optional", "command": "cargo clippy", "passed": false, "required": false, "output": "warning", "exit_code": 1},
        ]);
        let trigger = preflight_event("my-project", "validate", true, false, &results);
        let result = RouteValidationResult.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ValidationCompleted);
        assert_eq!(result.events[0].payload["success"], true);
    }
}
