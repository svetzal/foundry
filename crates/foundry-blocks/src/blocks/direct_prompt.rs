use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    ChainContext, LoopContext, PlanCompletedPayload, PreflightCompletedPayload,
    TaskRunStartedPayload,
};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::workflow::WorkflowType;

use super::TriggerContext;

/// Bridges user-provided task/prompt workflows from preflight directly to execution,
/// bypassing assessment, triage, and plan creation.
///
/// Observer — sinks on `PreflightCompleted`.
/// Self-filters: only runs when `workflow == "task"` or `workflow == "prompt"`,
/// and `all_passed == true`.
///
/// Takes the user-provided `prompt` from the payload and emits
/// `PlanCompleted` with the prompt as the plan. This feeds directly
/// into `ExecutePlan`, which executes the prompt with coding capability
/// and full filesystem access.
pub struct DirectPrompt;

fn accepts_direct_prompt(trigger: &Event) -> bool {
    let workflow = WorkflowType::from_payload(&trigger.payload);
    if workflow != WorkflowType::Task && workflow != WorkflowType::Prompt {
        return false;
    }
    trigger
        .parse_payload::<PreflightCompletedPayload>()
        .ok()
        .is_some_and(|p| p.all_passed)
}

impl TaskBlock for DirectPrompt {
    task_block_meta! {
        name: "Direct Prompt",
        kind: Observer,
        sinks_on: [PreflightCompleted],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        accepts_direct_prompt(trigger)
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project,
            throttle,
            payload,
            ..
        } = TriggerContext::from_trigger(trigger);

        // accepts() already filtered non-prompt and failed-preflight events.
        let p = parse_payload!(trigger, PreflightCompletedPayload);
        let workflow = WorkflowType::from_payload(&payload);

        let prompt = p
            .chain
            .prompt
            .as_ref()
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

            let chain = ChainContext::extract_from(&payload);
            let plan = super::event_from_infallible_payload(
                EventType::PlanCompleted,
                &project,
                throttle,
                &PlanCompletedPayload {
                    project: project.clone(),
                    plan: prompt.clone(),
                    // prompt workflow doesn't have a principle/category/assessment
                    principle: String::new(),
                    category: String::new(),
                    assessment: String::new(),
                    workflow: workflow.to_string(),
                    // Direct prompt always implies real work is intended.
                    correction_needed: true,
                    correction_reason: String::new(),
                    chain,
                },
            );
            let mut events = vec![plan];
            if workflow == WorkflowType::Task {
                events.insert(
                    0,
                    super::event_from_infallible_payload(
                        EventType::TaskRunStarted,
                        &project,
                        throttle,
                        &TaskRunStartedPayload {
                            project: project.clone(),
                            objective: prompt,
                            context: LoopContext::extract_from(&payload),
                        },
                    ),
                );
            }
            Ok(TaskBlockResult::success(
                format!("{project}: prompt forwarded to execution"),
                events,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use super::DirectPrompt;

    assert_block_meta!(
        DirectPrompt,
        kind: Observer,
        sinks_on: [PreflightCompleted],
    );

    #[test]
    fn accepts_returns_false_for_iterate_workflow() {
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

        assert!(!DirectPrompt.accepts(&trigger), "should not accept iterate workflow");
    }

    #[test]
    fn accepts_returns_true_for_task_with_passed_preflight() {
        let trigger = Event::new(
            EventType::PreflightCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "task",
                "all_passed": true,
                "required_passed": true,
                "results": [],
                "prompt": "do something",
            }),
        );

        assert!(
            DirectPrompt.accepts(&trigger),
            "should accept task workflow with passed preflight"
        );
    }

    #[test]
    fn accepts_returns_false_when_preflight_failed() {
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

        assert!(!DirectPrompt.accepts(&trigger), "should not accept failed preflight");
    }

    #[test]
    fn accepts_returns_true_for_prompt_with_passed_preflight() {
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
                "prompt": "do something",
            }),
        );

        assert!(
            DirectPrompt.accepts(&trigger),
            "should accept prompt workflow with passed preflight"
        );
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
    async fn emits_plan_completed_with_task_workflow() {
        let trigger = Event::new(
            EventType::PreflightCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "task",
                "all_passed": true,
                "required_passed": true,
                "results": [],
                "prompt": "Add a --quiet flag.",
            }),
        );

        let result = DirectPrompt.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].event_type, EventType::TaskRunStarted);
        let plan = result
            .events
            .iter()
            .find(|event| event.event_type == EventType::PlanCompleted)
            .unwrap();
        assert_eq!(plan.payload["plan"], "Add a --quiet flag.");
        assert_eq!(plan.payload["workflow"], "task");
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
