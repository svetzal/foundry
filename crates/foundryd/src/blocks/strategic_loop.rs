use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType, PayloadExt};
use foundry_core::loop_context::forward_payload_fields;
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

/// Controls the strategic iteration loop: picks the next area to improve
/// and decides when the loop is done.
///
/// Observer — sinks on `StrategicAssessmentCompleted` and `InnerIterationCompleted`.
///
/// On `StrategicAssessmentCompleted`: picks the first area from the ranked
/// list and emits `IterationRequested` with `loop_context` (entering the
/// inner iterate formation).
///
/// On `InnerIterationCompleted`: uses an AI agent to re-assess whether
/// further improvement is warranted. If yes and iterations remain, picks
/// the next area and emits another `IterationRequested`. If no (or max
/// iterations reached), emits `ProjectIterationCompleted` **without**
/// `loop_context`, which triggers `SummarizeResult` and `CommitAndPush`.
pub struct StrategicLoopController {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl StrategicLoopController {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, agent }
    }
}

impl TaskBlock for StrategicLoopController {
    task_block_meta! {
        name: "Strategic Loop Controller",
        kind: Observer,
        sinks_on: [StrategicAssessmentCompleted, InnerIterationCompleted],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let payload = trigger.payload.clone();
        let event_type = trigger.event_type.clone();

        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            match event_type {
                EventType::StrategicAssessmentCompleted => {
                    Ok(handle_assessment_completed(&project, throttle, &payload))
                }
                EventType::InnerIterationCompleted => {
                    handle_inner_completed(&project, throttle, &payload, entry, agent).await
                }
                _ => Ok(TaskBlockResult::success("Skipped: unexpected event type", vec![])),
            }
        })
    }
}

/// First call: pick the first area and enter the inner loop.
fn handle_assessment_completed(
    project: &str,
    throttle: foundry_core::throttle::Throttle,
    payload: &serde_json::Value,
) -> TaskBlockResult {
    let areas = payload
        .get("areas")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    if areas.is_empty() {
        tracing::info!(project = %project, "no areas to improve, completing");
        return complete_loop(project, throttle, payload);
    }

    let loop_context = payload
        .get("loop_context")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"strategic": {"iteration": 0, "max": 5}}));

    let area = &areas[0];
    tracing::info!(
        project = %project,
        area = %area.str_or("area", "unknown"),
        "entering inner loop for first area"
    );

    let mut updated_context = loop_context.clone();
    updated_context["strategic"]["iteration"] = serde_json::json!(1);
    updated_context["strategic"]["current_area"] = area.clone();

    let mut event_payload = serde_json::json!({
        "project": project,
        "loop_context": updated_context,
        "strategic_area": area,
    });
    forward_payload_fields(payload, &mut event_payload, &["actions"]);

    TaskBlockResult::success(
        format!("{project}: strategic loop iteration 1 — {}", area.str_or("area", "unknown")),
        vec![Event::new(
            EventType::IterationRequested,
            project.to_string(),
            throttle,
            event_payload,
        )],
    )
}

/// Subsequent calls: re-assess and decide whether to continue.
async fn handle_inner_completed(
    project: &str,
    throttle: foundry_core::throttle::Throttle,
    payload: &serde_json::Value,
    entry: Option<foundry_core::registry::ProjectEntry>,
    agent: Arc<dyn AgentGateway>,
) -> anyhow::Result<TaskBlockResult> {
    let loop_context = payload
        .get("loop_context")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"strategic": {"iteration": 1, "max": 5}}));

    let iteration = loop_context["strategic"].u64_or("iteration", 1);
    let max = loop_context["strategic"].u64_or("max", 5);

    let inner_success = payload.bool_or("success", false);

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
    let custom_prompt = loop_context["strategic"].get("prompt").and_then(serde_json::Value::as_str);
    let should_continue = assess_continue(project, entry.as_ref(), &agent, custom_prompt).await;

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
    updated_context["strategic"]["iteration"] = serde_json::json!(next_iteration);

    let mut event_payload = serde_json::json!({
        "project": project,
        "loop_context": updated_context,
    });
    forward_payload_fields(payload, &mut event_payload, &["actions"]);

    Ok(TaskBlockResult::success(
        format!("{project}: strategic loop continuing — iteration {next_iteration}"),
        vec![Event::new(
            EventType::IterationRequested,
            project.to_string(),
            throttle,
            event_payload,
        )],
    ))
}

/// Emit `ProjectIterationCompleted` without `loop_context` to trigger
/// terminal blocks (`SummarizeResult`, `CommitAndPush`).
fn complete_loop(
    project: &str,
    throttle: foundry_core::throttle::Throttle,
    payload: &serde_json::Value,
) -> TaskBlockResult {
    let mut event_payload = serde_json::json!({
        "project": project,
        "success": true,
        "summary": "strategic loop completed",
    });
    // Forward actions but NOT loop_context — terminal blocks should fire
    forward_payload_fields(payload, &mut event_payload, &["actions"]);

    TaskBlockResult::success(
        format!("{project}: strategic loop completed"),
        vec![Event::new(
            EventType::ProjectIterationCompleted,
            project.to_string(),
            throttle,
            event_payload,
        )],
    )
}

/// Ask the AI agent whether further iteration is warranted.
async fn assess_continue(
    project: &str,
    entry: Option<&foundry_core::registry::ProjectEntry>,
    agent: &Arc<dyn AgentGateway>,
    custom_prompt: Option<&str>,
) -> bool {
    let Some(entry) = entry else {
        return false;
    };

    let project_path = PathBuf::from(&entry.path);
    let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

    let default_directive = "Review the current state of the codebase and decide whether further improvement \
         iterations are warranted.\n\n\
         Consider:\n\
         - Are there remaining engineering principle violations with severity >= 4?\n\
         - Would another iteration produce meaningful improvement?\n\
         - Or has the codebase reached a plateau where further changes would be diminishing returns?";
    let directive = custom_prompt.unwrap_or(default_directive);

    let prompt = format!(
        "You have just completed an iteration of improvements on project '{project}'. \
         {directive}\n\n\
         Output ONLY valid JSON: {{\"continue\": true/false, \"reason\": \"<brief explanation>\"}}"
    );

    let request = AgentRequest {
        prompt,
        working_dir: project_path,
        access: AgentAccess::ReadOnly,
        capability: AgentCapability::Quick,
        agent_file,
        timeout: std::time::Duration::from_secs(120),
    };

    match agent.invoke(&request).await {
        Ok(r) if r.success => {
            let trimmed = r.stdout.trim();
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
                let should_continue = parsed.bool_or("continue", false);
                let reason = parsed.str_or("reason", "no reason");
                tracing::info!(
                    project = %project,
                    should_continue = should_continue,
                    reason = %reason,
                    "strategic continue assessment"
                );
                return should_continue;
            }
            // Try to extract JSON from output
            if let Some(start) = trimmed.find('{') {
                if let Some(end) = trimmed.rfind('}') {
                    if let Ok(parsed) =
                        serde_json::from_str::<serde_json::Value>(&trimmed[start..=end])
                    {
                        return parsed.bool_or("continue", false);
                    }
                }
            }
            false
        }
        _ => {
            tracing::warn!(project = %project, "continue assessment failed, stopping loop");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;

    use super::StrategicLoopController;

    fn registry_with_project(name: &str, path: &str) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: name.to_string(),
                path: path.to_string(),
                stack: Stack::Rust,
                agent: "claude".to_string(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                notes: None,
                actions: ActionFlags::default(),
                install: None,
                timeout_secs: None,
            }],
        })
    }

    fn empty_registry() -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![],
        })
    }

    #[test]
    fn kind_is_observer() {
        let agent = FakeAgentGateway::success();
        let block = StrategicLoopController::new(agent, empty_registry());
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_correct_events() {
        let agent = FakeAgentGateway::success();
        let block = StrategicLoopController::new(agent, empty_registry());
        let sinks = block.sinks_on();
        assert!(sinks.contains(&EventType::StrategicAssessmentCompleted));
        assert!(sinks.contains(&EventType::InnerIterationCompleted));
    }

    #[tokio::test]
    async fn assessment_completed_enters_inner_loop() {
        let agent = FakeAgentGateway::success();
        let block = StrategicLoopController::new(agent, empty_registry());
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
        assert_eq!(result.events[0].event_type, EventType::IterationRequested);

        let payload = &result.events[0].payload;
        // loop_context should be updated to iteration 1
        assert_eq!(payload["loop_context"]["strategic"]["iteration"], 1);
        // Strategic area should be passed to inner loop
        assert!(payload.get("strategic_area").is_some());
    }

    #[tokio::test]
    async fn assessment_completed_with_no_areas_completes_loop() {
        let agent = FakeAgentGateway::success();
        let block = StrategicLoopController::new(agent, empty_registry());
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
        let block = StrategicLoopController::new(agent, empty_registry());
        let trigger = Event::new(
            EventType::InnerIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": true,
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
        let block = StrategicLoopController::new(agent, empty_registry());
        let trigger = Event::new(
            EventType::InnerIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": false,
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
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = StrategicLoopController::new(agent, registry);
        let trigger = Event::new(
            EventType::InnerIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": true,
                "loop_context": {
                    "strategic": { "iteration": 1, "max": 5 }
                },
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::IterationRequested);
        assert_eq!(result.events[0].payload["loop_context"]["strategic"]["iteration"], 2);
    }

    #[tokio::test]
    async fn inner_completed_continue_false_completes_loop() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(
            r#"{"continue": false, "reason": "quality plateau reached"}"#,
        );
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = StrategicLoopController::new(agent, registry);
        let trigger = Event::new(
            EventType::InnerIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": true,
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
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = StrategicLoopController::new(agent, registry);
        let trigger = Event::new(
            EventType::InnerIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": true,
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
        let block = StrategicLoopController::new(agent, empty_registry());
        let trigger = Event::new(
            EventType::InnerIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": true,
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
