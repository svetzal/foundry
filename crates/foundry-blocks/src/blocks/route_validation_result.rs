use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{PreflightCompletedPayload, ValidationCompletedPayload};
use foundry_sdk::task_block::{BlockKind, TaskBlock};
use foundry_sdk::workflow::WorkflowType;

use super::TriggerContext;

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

fn accepts_validate(trigger: &Event) -> bool {
    trigger.parse_payload::<PreflightCompletedPayload>().ok().is_some_and(|p| {
        p.workflow.parse::<WorkflowType>().unwrap_or(WorkflowType::Unknown)
            == WorkflowType::Validate
    })
}

impl TaskBlock for RouteValidationResult {
    task_block_meta! {
        name: "Route Validation Result",
        kind: Observer,
        sinks_on: [PreflightCompleted],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        accepts_validate(trigger)
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project, throttle, ..
        } = TriggerContext::from_trigger(trigger);

        let p = parse_payload!(trigger, PreflightCompletedPayload);
        let required_passed = p.required_passed;
        let results = serde_json::json!(p.results);

        Box::pin(async move {
            super::emit_event_result(
                if required_passed {
                    format!("{project}: validation passed")
                } else {
                    format!("{project}: validation failed")
                },
                required_passed,
                EventType::ValidationCompleted,
                &project,
                throttle,
                &ValidationCompletedPayload {
                    project: project.clone(),
                    success: required_passed,
                    workflow: WorkflowType::Validate.to_string(),
                    results: Some(results),
                    ..Default::default()
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

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

    assert_block_meta!(
        RouteValidationResult,
        kind: Observer,
        sinks_on: [PreflightCompleted],
    );

    #[test]
    fn accepts_returns_false_for_iterate_workflow() {
        let trigger = preflight_event("my-project", "iterate", true, true, &serde_json::json!([]));
        assert!(!RouteValidationResult.accepts(&trigger), "should not accept iterate workflow");
    }

    #[test]
    fn accepts_returns_false_for_maintain_workflow() {
        let trigger = preflight_event("my-project", "maintain", true, true, &serde_json::json!([]));
        assert!(!RouteValidationResult.accepts(&trigger), "should not accept maintain workflow");
    }

    #[test]
    fn accepts_returns_true_for_validate_workflow() {
        let trigger = preflight_event("my-project", "validate", true, true, &serde_json::json!([]));
        assert!(RouteValidationResult.accepts(&trigger), "should accept validate workflow");
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
