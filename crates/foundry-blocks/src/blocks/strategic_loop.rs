use std::path::PathBuf;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::loop_context::forward_payload_fields;
use foundry_sdk::payload::{InnerIterationCompletedPayload, StrategicAssessmentCompletedPayload};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentGateway, AgentProvider, ModelTier, ReasoningEffort};

use super::{AgentBlockSpec, TriggerContext};

agent_block_new!(
    /// Controls the strategic iteration loop: picks the next area to improve
    /// and decides when the loop is done.
    ///
    /// Observer — sinks on `StrategicAssessmentCompleted` and `InnerIterationCompleted`.
    ///
    /// On `StrategicAssessmentCompleted`: picks the first area from the ranked
    /// list and emits `ProjectIterationRequested` with `loop_context` (entering the
    /// inner iterate formation).
    ///
    /// On `InnerIterationCompleted`: uses an AI agent to re-assess whether
    /// further improvement is warranted. If yes and iterations remain, picks
    /// the next area and emits another `ProjectIterationRequested`. If no (or max
    /// iterations reached), emits `ProjectIterationCompleted` **without**
    /// `loop_context`, which triggers `SummarizeResult` and `CommitAndPush`.
    pub struct StrategicLoopController
);

impl TaskBlock for StrategicLoopController {
    task_block_meta! {
        name: "Strategic Loop Controller",
        kind: Observer,
        sinks_on: [StrategicAssessmentCompleted, InnerIterationCompleted],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project,
            throttle,
            payload,
        } = TriggerContext::from_trigger(trigger);
        let event_type = trigger.event_type.clone();

        // Parse typed payload before entering the async block.
        let assessment_payload = if event_type == EventType::StrategicAssessmentCompleted {
            match trigger.parse_payload::<StrategicAssessmentCompletedPayload>() {
                Ok(p) => Some(p),
                Err(e) => return Box::pin(async move { Err(e.into()) }),
            }
        } else {
            None
        };
        let inner_payload = if event_type == EventType::InnerIterationCompleted {
            match trigger.parse_payload::<InnerIterationCompletedPayload>() {
                Ok(p) => Some(p),
                Err(e) => return Box::pin(async move { Err(e.into()) }),
            }
        } else {
            None
        };

        let entry = match super::read_registry(&self.registry) {
            Ok(guard) => guard.find_project(&project).cloned(),
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            match event_type {
                EventType::StrategicAssessmentCompleted => {
                    let p = assessment_payload.expect("parsed above");
                    Ok(handle_assessment_completed(&project, throttle, &payload, &p))
                }
                EventType::InnerIterationCompleted => {
                    let p = inner_payload.expect("parsed above");
                    handle_inner_completed(&project, throttle, &payload, &p, entry, agent).await
                }
                _ => Ok(TaskBlockResult::success("Skipped: unexpected event type", vec![])),
            }
        })
    }
}

/// First call: pick the first area and enter the inner loop.
fn handle_assessment_completed(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    payload: &serde_json::Value,
    p: &StrategicAssessmentCompletedPayload,
) -> TaskBlockResult {
    let areas = &p.areas;

    if areas.is_empty() {
        tracing::info!(project = %project, "no areas to improve, completing");
        return complete_loop(project, throttle, payload);
    }

    let area = &areas[0];
    let area_name = &area.area;
    tracing::info!(
        project = %project,
        area = %area_name,
        "entering inner loop for first area"
    );

    let area_value = serde_json::to_value(area).expect("AreaEntry is infallibly serializable");

    let mut updated_context = p.loop_context.clone();
    updated_context.strategic.iteration = 1;
    updated_context.strategic.current_area = Some(area_value.clone());

    let loop_ctx_value = serde_json::to_value(&updated_context)
        .expect("StrategicLoopContext is infallibly serializable");

    let mut event_payload = serde_json::json!({
        "project": project,
        "loop_context": loop_ctx_value,
        "strategic_area": area_value,
    });
    forward_payload_fields(payload, &mut event_payload, &["actions"]);

    super::single_event_result(
        format!("{project}: strategic loop iteration 1 — {area_name}"),
        EventType::ProjectIterationRequested,
        project.to_string(),
        throttle,
        event_payload,
    )
}

/// Subsequent calls: re-assess and decide whether to continue.
async fn handle_inner_completed(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    payload: &serde_json::Value,
    p: &InnerIterationCompletedPayload,
    entry: Option<foundry_sdk::registry::ProjectEntry>,
    agent: Arc<dyn AgentGateway>,
) -> anyhow::Result<TaskBlockResult> {
    let loop_context = p.loop_context.clone();
    let iteration = loop_context.strategic.iteration;
    let max = loop_context.strategic.max;
    let custom_prompt = loop_context.strategic.prompt.as_deref();

    let inner_success = p.success;

    // Max iterations reached — complete the loop
    if iteration >= max {
        tracing::info!(
            project = %project,
            iteration = iteration,
            max = max,
            "max strategic iterations reached, completing loop"
        );
        return Ok(complete_loop(project, throttle, payload));
    }

    // Inner loop failed — complete the loop (don't keep trying)
    if !inner_success {
        tracing::info!(
            project = %project,
            iteration = iteration,
            "inner iteration failed, completing strategic loop"
        );
        return Ok(complete_loop(project, throttle, payload));
    }

    // Use AI to decide whether to continue
    let provider = super::chain_agent_provider(payload);
    let should_continue =
        assess_continue(project, entry.as_ref(), &agent, custom_prompt, provider).await;

    if !should_continue {
        tracing::info!(
            project = %project,
            iteration = iteration,
            "AI assessment says no more work needed, completing loop"
        );
        return Ok(complete_loop(project, throttle, payload));
    }

    // Continue: increment iteration and re-enter the inner loop
    let next_iteration = iteration + 1;
    tracing::info!(
        project = %project,
        iteration = next_iteration,
        "continuing strategic loop"
    );

    let mut updated_context = loop_context;
    updated_context.strategic.iteration = next_iteration;

    let loop_ctx_value = serde_json::to_value(&updated_context)
        .expect("StrategicLoopContext is infallibly serializable");

    let mut event_payload = serde_json::json!({
        "project": project,
        "loop_context": loop_ctx_value,
    });
    forward_payload_fields(payload, &mut event_payload, &["actions"]);

    Ok(super::single_event_result(
        format!("{project}: strategic loop continuing — iteration {next_iteration}"),
        EventType::ProjectIterationRequested,
        project.to_string(),
        throttle,
        event_payload,
    ))
}

/// Emit `ProjectIterationCompleted` without `loop_context` to trigger
/// terminal blocks (`SummarizeResult`, `CommitAndPush`).
fn complete_loop(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    payload: &serde_json::Value,
) -> TaskBlockResult {
    let mut event_payload = serde_json::json!({
        "project": project,
        "success": true,
        "summary": "strategic loop completed",
    });
    // Forward actions but NOT loop_context — terminal blocks should fire
    forward_payload_fields(payload, &mut event_payload, &["actions"]);

    super::single_event_result(
        format!("{project}: strategic loop completed"),
        EventType::ProjectIterationCompleted,
        project.to_string(),
        throttle,
        event_payload,
    )
}

/// Ask the AI agent whether further iteration is warranted.
async fn assess_continue(
    project: &str,
    entry: Option<&foundry_sdk::registry::ProjectEntry>,
    agent: &Arc<dyn AgentGateway>,
    custom_prompt: Option<&str>,
    provider: Option<AgentProvider>,
) -> bool {
    let Some(entry) = entry else {
        return false;
    };

    let project_path = PathBuf::from(&entry.path);
    let agent_file = super::resolve_agent_file(&entry.agent);

    let default_directive = "Review the current state of the codebase and decide whether further improvement \
         iterations are warranted.\n\n\
         Consider:\n\
         - Are there remaining engineering principle violations with severity >= 4?\n\
         - Would another iteration produce meaningful improvement?\n\
         - Or has the codebase reached a plateau where further changes would be diminishing returns?";
    let directive = custom_prompt.unwrap_or(default_directive);

    // Intentionally distinct prompt shape: abbreviated schema inline, no "in this exact format"
    // sentinel — does not use json_output_prompt.
    let prompt = format!(
        "You have just completed an iteration of improvements on project '{project}'. \
         {directive}\n\n\
         Output ONLY valid JSON: {{\"continue\": true/false, \"reason\": \"<brief explanation>\"}}"
    );

    let outcome = super::invoke_agent(
        agent.as_ref(),
        AgentBlockSpec {
            prompt,
            working_dir: project_path,
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Fast,
            effort: ReasoningEffort::Low,
            agent_file,
            provider,
            env: Vec::new(),
            timeout: std::time::Duration::from_secs(120),
        },
        "strategic continue assessment",
        project,
    )
    .await;

    super::fold_agent_outcome(
        outcome,
        project,
        "strategic continue assessment",
        |stdout| {
            if let Some(parsed) = super::parse_agent_json(&stdout) {
                let should_continue =
                    parsed.get("continue").and_then(serde_json::Value::as_bool).unwrap_or(false);
                let reason =
                    parsed.get("reason").and_then(serde_json::Value::as_str).unwrap_or("no reason");
                tracing::info!(
                    project = %project,
                    should_continue = should_continue,
                    reason = %reason,
                    "strategic continue assessment"
                );
                should_continue
            } else {
                false
            }
        },
        |_ns| false,
    )
}

#[cfg(test)]
mod tests {
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::task_block::{BlockKind, TaskBlock};
    use foundry_sdk::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;

    use super::super::test_helpers;
    use super::StrategicLoopController;

    #[test]
    fn kind_is() {
        let agent = FakeAgentGateway::success();
        let block = StrategicLoopController::new(agent, test_helpers::empty_registry());
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_correct_events() {
        let agent = FakeAgentGateway::success();
        let block = StrategicLoopController::new(agent, test_helpers::empty_registry());
        let sinks = block.sinks_on();
        assert!(sinks.contains(&EventType::StrategicAssessmentCompleted));
        assert!(sinks.contains(&EventType::InnerIterationCompleted));
    }

    #[tokio::test]
    async fn assessment_completed_enters_inner_loop() {
        let agent = FakeAgentGateway::success();
        let block = StrategicLoopController::new(agent, test_helpers::empty_registry());
        let trigger = Event::new(
            EventType::StrategicAssessmentCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "areas": [
                    {"area": "test coverage", "severity": 8, "category": "testing"},
                    {"area": "error handling", "severity": 6, "category": "error-handling"},
                ],
                "loop_context": {
                    "strategic": { "iteration": 0, "max": 5, "total_areas": 2 }
                },
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationRequested);

        let payload = &result.events[0].payload;
        // loop_context should be updated to iteration 1
        assert_eq!(payload["loop_context"]["strategic"]["iteration"], 1);
        // Strategic area should be passed to inner loop
        assert!(payload.get("strategic_area").is_some());
    }

    #[tokio::test]
    async fn assessment_completed_with_no_areas_completes_loop() {
        let agent = FakeAgentGateway::success();
        let block = StrategicLoopController::new(agent, test_helpers::empty_registry());
        let trigger = Event::new(
            EventType::StrategicAssessmentCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "areas": [],
                "loop_context": {
                    "strategic": { "iteration": 0, "max": 5 }
                },
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        // No loop_context — terminal blocks should fire
        assert!(result.events[0].payload.get("loop_context").is_none());
    }

    #[tokio::test]
    async fn inner_completed_max_iterations_completes_loop() {
        let agent = FakeAgentGateway::success();
        let block = StrategicLoopController::new(agent, test_helpers::empty_registry());
        let trigger = Event::new(
            EventType::InnerIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": true,
                "summary": "",
                "workflow": "iterate",
                "loop_context": {
                    "strategic": { "iteration": 5, "max": 5 }
                },
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        assert!(result.events[0].payload.get("loop_context").is_none());
    }

    #[tokio::test]
    async fn inner_completed_failure_stops_loop() {
        let agent = FakeAgentGateway::success();
        let block = StrategicLoopController::new(agent, test_helpers::empty_registry());
        let trigger = Event::new(
            EventType::InnerIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": false,
                "summary": "",
                "workflow": "iterate",
                "loop_context": {
                    "strategic": { "iteration": 1, "max": 5 }
                },
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        assert!(result.events[0].payload.get("loop_context").is_none());
    }

    #[tokio::test]
    async fn inner_completed_continue_true_emits_next_iteration() {
        let dir = tempfile::tempdir().unwrap();
        let agent =
            FakeAgentGateway::success_with(r#"{"continue": true, "reason": "more work needed"}"#);
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = StrategicLoopController::new(agent, registry);
        let trigger = Event::new(
            EventType::InnerIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": true,
                "summary": "",
                "workflow": "iterate",
                "loop_context": {
                    "strategic": { "iteration": 1, "max": 5 }
                },
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationRequested);
        assert_eq!(result.events[0].payload["loop_context"]["strategic"]["iteration"], 2);
    }

    #[tokio::test]
    async fn inner_completed_continue_false_completes_loop() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(
            r#"{"continue": false, "reason": "quality plateau reached"}"#,
        );
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = StrategicLoopController::new(agent, registry);
        let trigger = Event::new(
            EventType::InnerIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": true,
                "summary": "",
                "workflow": "iterate",
                "loop_context": {
                    "strategic": { "iteration": 1, "max": 5 }
                },
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        assert!(result.events[0].payload.get("loop_context").is_none());
    }

    #[tokio::test]
    async fn forwards_actions_on_continue() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(r#"{"continue": true, "reason": "more work"}"#);
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = StrategicLoopController::new(agent, registry);
        let trigger = Event::new(
            EventType::InnerIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": true,
                "summary": "",
                "workflow": "iterate",
                "actions": {"maintain": true},
                "loop_context": {
                    "strategic": { "iteration": 1, "max": 5 }
                },
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events[0].payload["actions"]["maintain"], true);
    }

    #[tokio::test]
    async fn forwards_actions_on_complete() {
        let agent = FakeAgentGateway::success();
        let block = StrategicLoopController::new(agent, test_helpers::empty_registry());
        let trigger = Event::new(
            EventType::InnerIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": true,
                "summary": "",
                "workflow": "iterate",
                "actions": {"maintain": true},
                "loop_context": {
                    "strategic": { "iteration": 5, "max": 5 }
                },
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        assert_eq!(result.events[0].payload["actions"]["maintain"], true);
    }
}
