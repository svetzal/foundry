use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::loop_context::forward_loop_context;
use foundry_core::payload::PreflightCompletedPayload;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::workflow::WorkflowType;

use super::TriggerContext;

/// Bridges the prompt workflow from preflight directly to execution,
/// bypassing assessment, triage, and plan creation.
///
/// Observer — sinks on `PreflightCompleted`.
/// Self-filters: only runs when `workflow == "prompt"` and `all_passed == true`.
///
/// Takes the user-provided `prompt` from the payload and emits
/// `PlanCompleted` with the prompt as the plan. This feeds directly
/// into `ExecutePlan`, which executes the prompt with coding capability
/// and full filesystem access.
pub struct DirectPrompt;

impl TaskBlock for DirectPrompt {
    task_block_meta! {
        name: "Direct Prompt",
        kind: Observer,
        sinks_on: [PreflightCompleted],
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

        let workflow = WorkflowType::from_payload(&payload);

        // Self-filter: only run for prompt workflow
        if workflow != WorkflowType::Prompt {
            return Box::pin(async {
                Ok(TaskBlockResult::success("Skipped: not a prompt workflow", vec![]))
            });
        }

        let p = match trigger.parse_payload::<PreflightCompletedPayload>() {
            Ok(p) => p,
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        let all_passed = p.all_passed;

        if !all_passed {
            return Box::pin(async {
                Ok(TaskBlockResult::success("Skipped: preflight gates did not pass", vec![]))
            });
        }

        let prompt = payload
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();

        if prompt.is_empty() {
            return Box::pin(async {
                Ok(TaskBlockResult::failure("No prompt provided in payload"))
            });
        }

        Box::pin(async move {
            tracing::info!(
                project = %project,
                prompt_len = prompt.len(),
                "forwarding prompt directly to execution"
            );

            let mut event_payload = serde_json::json!({
                "project": project,
                "plan": prompt,
                "workflow": WorkflowType::Prompt,
            });
            if let Some(actions) = payload.get("actions") {
                event_payload["actions"] = actions.clone();
            }
            if let Some(gates) = payload.get("gates") {
                event_payload["gates"] = gates.clone();
            }
            forward_loop_context(&payload, &mut event_payload);

            Ok(TaskBlockResult::success(
                format!("{project}: prompt forwarded to execution"),
                vec![Event::new(
                    EventType::PlanCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use foundry_core::event::{Event, EventType};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use super::DirectPrompt;

    #[test]
    fn kind_is_observer() {
        assert_eq!(DirectPrompt.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_preflight_completed() {
        assert_eq!(DirectPrompt.sinks_on(), &[EventType::PreflightCompleted]);
    }

    #[tokio::test]
    async fn skips_when_workflow_is_iterate() {
        let trigger = Event::new(
            EventType::PreflightCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "iterate",
                "all_passed": true,
                "required_passed": true,
                "results": [],
            }),
        );

        let result = DirectPrompt.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn skips_when_preflight_failed() {
        let trigger = Event::new(
            EventType::PreflightCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "prompt",
                "all_passed": false,
                "required_passed": false,
                "results": [],
                "prompt": "do something",
            }),
        );

        let result = DirectPrompt.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn fails_when_prompt_is_empty() {
        let trigger = Event::new(
            EventType::PreflightCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "prompt",
                "all_passed": true,
                "required_passed": true,
                "results": [],
            }),
        );

        let result = DirectPrompt.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn emits_plan_completed_with_prompt() {
        let trigger = Event::new(
            EventType::PreflightCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "prompt",
                "all_passed": true,
                "required_passed": true,
                "results": [],
                "prompt": "Pick the highest priority interaction from et and implement it.",
                "gates": [{"name": "fmt", "command": "cargo fmt --check", "required": true}],
            }),
        );

        let result = DirectPrompt.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::PlanCompleted);
        assert_eq!(
            result.events[0].payload["plan"],
            "Pick the highest priority interaction from et and implement it."
        );
        assert_eq!(result.events[0].payload["workflow"], "prompt");
        // Gates should be forwarded
        assert!(result.events[0].payload.get("gates").is_some());
    }

    #[tokio::test]
    async fn forwards_actions() {
        let trigger = Event::new(
            EventType::PreflightCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "prompt",
                "all_passed": true,
                "required_passed": true,
                "results": [],
                "prompt": "do the thing",
                "actions": {"push": true},
            }),
        );

        let result = DirectPrompt.execute(&trigger).await.unwrap();

        assert_eq!(result.events[0].payload["actions"]["push"], true);
    }
}
