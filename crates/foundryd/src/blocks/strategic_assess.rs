use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentOutcome, AgentRequest};

use super::TriggerContext;

/// Performs a strategic assessment of the project to identify multiple areas
/// for improvement, then emits a plan for the strategic loop controller.
///
/// Observer — sinks on `IterationRequested`.
/// Self-filters: only runs when the payload contains `strategic: true`.
/// Without that flag, the existing `CheckCharter` block handles the event
/// instead (standalone iterate).
///
/// Emits `StrategicAssessmentCompleted` with a ranked list of improvement
/// areas and an initialized `loop_context`.
pub struct StrategicAssessor {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl StrategicAssessor {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, agent }
    }
}

impl TaskBlock for StrategicAssessor {
    task_block_meta! {
        name: "Strategic Assessor",
        kind: Observer,
        sinks_on: [IterationRequested],
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

        // Self-filter: only run when strategic mode is requested.
        // When strategic=false or absent, the existing CheckCharter path handles it.
        // Use direct Value access — this block may receive minimal payloads in tests.
        let strategic =
            payload.get("strategic").and_then(serde_json::Value::as_bool).unwrap_or(false);

        if !strategic {
            return Box::pin(async {
                Ok(TaskBlockResult::success("Skipped: not a strategic iteration", vec![]))
            });
        }

        let entry = match super::require_project(&self.registry, &project) {
            Ok(e) => e,
            Err(result) => return Box::pin(async { Ok(result) }),
        };
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);
            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            let max_iterations =
                payload.get("max_iterations").and_then(serde_json::Value::as_u64).unwrap_or(5);

            let strategic_prompt = payload
                .get("strategic_prompt")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);

            let prompt = build_strategic_prompt(
                &project,
                strategic_prompt.as_deref().unwrap_or(
                    "Analyse the codebase holistically and identify the top areas that need \
                     improvement. Consider: code clarity, test coverage, error handling, naming, \
                     duplication, separation of concerns, and adherence to the project's stated \
                     conventions.\n\n\
                     Rank the areas by impact — the most impactful improvement should come first.",
                ),
            );

            let request = AgentRequest {
                prompt,
                working_dir: project_path,
                access: AgentAccess::ReadOnly,
                capability: AgentCapability::Reasoning,
                agent_file,
                timeout: entry.timeout(),
            };

            tracing::info!(project = %project, "performing strategic assessment via agent");

            let response = agent.invoke(&request).await;

            let areas = match AgentOutcome::from_response(response) {
                AgentOutcome::Success { stdout } => parse_strategic_assessment(&stdout),
                AgentOutcome::AgentFailed { stderr } => {
                    tracing::warn!(project = %project, stderr = %stderr, "strategic assessment agent failed");
                    vec![serde_json::json!({
                        "area": "general quality improvement",
                        "severity": 5,
                        "category": "conventions",
                    })]
                }
                AgentOutcome::Unavailable { error } => {
                    tracing::warn!(error = %error, "agent invocation failed for strategic assessment");
                    return Ok(TaskBlockResult::failure(format!("agent unavailable: {error}")));
                }
            };

            tracing::info!(
                project = %project,
                area_count = areas.len(),
                "strategic assessment completed"
            );

            Ok(build_strategic_result(
                &project,
                &areas,
                strategic_prompt.as_deref(),
                max_iterations,
                &payload,
                throttle,
            ))
        })
    }
}

fn build_strategic_prompt(project: &str, directive: &str) -> String {
    format!(
        "You are performing a strategic assessment of the project '{project}'.\n\n\
         {directive}\n\n\
         Output ONLY valid JSON in this exact format, nothing else:\n\
         {{\n  \
           \"areas\": [\n    \
             {{\"area\": \"<short description>\", \"severity\": <1-10>, \"category\": \"<category>\"}}\n  \
           ]\n\
         }}"
    )
}

fn build_strategic_result(
    project: &str,
    areas: &[serde_json::Value],
    strategic_prompt: Option<&str>,
    max_iterations: u64,
    payload: &serde_json::Value,
    throttle: foundry_core::throttle::Throttle,
) -> TaskBlockResult {
    let mut strategic_context = serde_json::json!({
        "iteration": 0,
        "max": max_iterations,
        "total_areas": areas.len(),
    });
    if let Some(sp) = strategic_prompt {
        strategic_context["prompt"] = serde_json::json!(sp);
    }

    let area_count = areas.len();
    let mut event_payload = serde_json::json!({
        "project": project,
        "areas": areas,
        "loop_context": {
            "strategic": strategic_context,
        },
    });
    if let Some(actions) = payload.get("actions") {
        event_payload["actions"] = actions.clone();
    }

    TaskBlockResult::success(
        format!("{project}: strategic assessment identified {area_count} areas"),
        vec![Event::new(
            EventType::StrategicAssessmentCompleted,
            project.to_string(),
            throttle,
            event_payload,
        )],
    )
}

/// Parse the agent output as a JSON object with an `areas` array.
fn parse_strategic_assessment(output: &str) -> Vec<serde_json::Value> {
    // Try to extract JSON from the output (agent may include extra text)
    let trimmed = output.trim();

    // Try direct parse first
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(areas) = parsed.get("areas").and_then(serde_json::Value::as_array) {
            return areas.clone();
        }
    }

    // Try to find JSON block in the output
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            let json_str = &trimmed[start..=end];
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(areas) = parsed.get("areas").and_then(serde_json::Value::as_array) {
                    return areas.clone();
                }
            }
        }
    }

    // Fallback: single generic area
    vec![serde_json::json!({
        "area": "general quality improvement",
        "severity": 5,
        "category": "conventions",
    })]
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::Registry;
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;

    use super::super::test_helpers;
    use super::{StrategicAssessor, parse_strategic_assessment};

    #[test]
    fn kind_is_observer() {
        let agent = FakeAgentGateway::success();
        let block = StrategicAssessor::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_iteration_requested() {
        let agent = FakeAgentGateway::success();
        let block = StrategicAssessor::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.sinks_on(), &[EventType::IterationRequested]);
    }

    #[tokio::test]
    async fn skips_when_strategic_not_set() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project("my-project", "/tmp/test");
        let block = StrategicAssessor::new(agent.clone(), registry);
        let trigger = Event::new(
            EventType::IterationRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(agent.invocations().is_empty());
    }

    #[tokio::test]
    async fn runs_assessment_when_strategic_true() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(
            r#"{"areas": [{"area": "test coverage", "severity": 8, "category": "testing"}]}"#,
        );
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = StrategicAssessor::new(agent.clone(), registry);
        let trigger = Event::new(
            EventType::IterationRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({"strategic": true}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::StrategicAssessmentCompleted);

        let payload = &result.events[0].payload;
        let areas = payload["areas"].as_array().unwrap();
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0]["area"], "test coverage");

        // loop_context should be initialized
        let lc = &payload["loop_context"]["strategic"];
        assert_eq!(lc["iteration"], 0);
        assert_eq!(lc["total_areas"], 1);
    }

    #[tokio::test]
    async fn forwards_actions() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(r#"{"areas": []}"#);
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = StrategicAssessor::new(agent, registry);
        let trigger = Event::new(
            EventType::IterationRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({"strategic": true, "actions": {"maintain": true}}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["actions"]["maintain"], true);
    }

    #[test]
    fn parse_strategic_assessment_valid_json() {
        let output = r#"{"areas": [{"area": "error handling", "severity": 7, "category": "error-handling"}]}"#;
        let areas = parse_strategic_assessment(output);
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0]["area"], "error handling");
    }

    #[test]
    fn parse_strategic_assessment_with_extra_text() {
        let output = "Here is my assessment:\n{\"areas\": [{\"area\": \"testing\", \"severity\": 5, \"category\": \"testing\"}]}\nDone.";
        let areas = parse_strategic_assessment(output);
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0]["area"], "testing");
    }

    #[test]
    fn parse_strategic_assessment_invalid_fallback() {
        let areas = parse_strategic_assessment("not json at all");
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0]["category"], "conventions");
    }
}
