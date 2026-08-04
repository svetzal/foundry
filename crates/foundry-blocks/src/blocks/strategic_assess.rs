use std::path::PathBuf;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::ProjectIterationRequestedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::AgentGateway;

use super::{ReadOnlyAgentSpec, TriggerContext, fold_agent_outcome, invoke_reasoning_agent};

agent_block_new!(
    /// Performs a strategic assessment of the project to identify multiple areas
    /// for improvement, then emits a plan for the strategic loop controller.
    ///
    /// Observer — sinks on `ProjectIterationRequested`.
    /// Self-filters: only runs when the payload contains `strategic: true`.
    /// Without that flag, the existing `CheckCharter` block handles the event
    /// instead (standalone iterate).
    ///
    /// Emits `StrategicAssessmentCompleted` with a ranked list of improvement
    /// areas and an initialized `loop_context`.
    pub struct StrategicAssessor
);

fn accepts_strategic(trigger: &Event) -> bool {
    trigger
        .parse_payload::<ProjectIterationRequestedPayload>()
        .ok()
        .and_then(|p| p.strategic)
        .unwrap_or(false)
}

impl TaskBlock for StrategicAssessor {
    task_block_meta! {
        name: "Strategic Assessor",
        kind: Observer,
        sinks_on: [ProjectIterationRequested],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        accepts_strategic(trigger)
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project,
            throttle,
            trace_id,
            ..
        } = TriggerContext::from_trigger(trigger);

        // accepts() already filtered non-strategic events.
        let p = parse_payload!(trigger, ProjectIterationRequestedPayload);

        let max_iterations = p.max_iterations.unwrap_or(5);
        let strategic_prompt = p.strategic_prompt.clone();
        let actions = p.chain.actions.clone();
        let provider = super::parse_agent_provider(p.chain.agent_provider.as_deref());

        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);
            let agent_file = super::resolve_agent_file(&entry.agent);

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

            let outcome = invoke_reasoning_agent(
                &*agent,
                &project,
                ReadOnlyAgentSpec {
                    prompt,
                    working_dir: project_path,
                    agent_file,
                    provider,
                    timeout: entry.timeout(),
                    trace_id: trace_id.clone(),
                },
                "strategic assessment",
            )
            .await;

            let areas = match fold_agent_outcome(
                outcome,
                &project,
                "strategic assessment",
                |stdout| Ok(parse_strategic_assessment(&stdout)),
                |ns| {
                    if ns.unavailable {
                        Err(TaskBlockResult::failure(format!("agent unavailable: {}", ns.error)))
                    } else {
                        Ok(vec![serde_json::json!({
                            "area": "general quality improvement",
                            "severity": 5,
                            "category": "conventions",
                        })])
                    }
                },
            ) {
                Ok(a) => a,
                Err(f) => return Ok(f),
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
                actions.as_ref(),
                throttle,
            ))
        })
    }
}

fn build_strategic_prompt(project: &str, directive: &str) -> String {
    super::json_output_prompt(
        &format!(
            "You are performing a strategic assessment of the project '{project}'.\n\n\
             {directive}"
        ),
        "{{\n  \
           \"areas\": [\n    \
             {{\"area\": \"<short description>\", \"severity\": <1-10>, \"category\": \"<category>\"}}\n  \
           ]\n\
         }}",
    )
}

fn build_strategic_result(
    project: &str,
    areas: &[serde_json::Value],
    strategic_prompt: Option<&str>,
    max_iterations: u64,
    actions: Option<&serde_json::Value>,
    throttle: foundry_sdk::throttle::Throttle,
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
    if let Some(actions) = actions {
        event_payload["actions"] = actions.clone();
    }

    super::single_event_result(
        format!("{project}: strategic assessment identified {area_count} areas"),
        EventType::StrategicAssessmentCompleted,
        project.to_string(),
        throttle,
        event_payload,
    )
}

/// Parse the agent output as a JSON object with an `areas` array.
fn parse_strategic_assessment(output: &str) -> Vec<serde_json::Value> {
    if let Some(parsed) = super::parse_agent_json(output)
        && let Some(areas) = parsed.get("areas").and_then(serde_json::Value::as_array)
    {
        return areas.clone();
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
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::EventType;
    use foundry_sdk::registry::Registry;
    use foundry_sdk::task_block::TaskBlock;

    use crate::gateway::fakes::FakeAgentGateway;

    use super::super::test_helpers;
    use super::{StrategicAssessor, parse_strategic_assessment};

    assert_block_meta!(
        StrategicAssessor::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry { version: 2, projects: vec![] })),
        ),
        kind: Observer,
        sinks_on: [ProjectIterationRequested],
    );

    #[test]
    fn accepts_returns_false_when_strategic_not_set() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project("my-project", "/tmp/test");
        let block = StrategicAssessor::new(agent, registry);
        let trigger = test_event!(EventType::ProjectIterationRequested, "my-project", {
            "project": "my-project", "workflow": "iterate"
        });

        assert!(!block.accepts(&trigger), "should not accept non-strategic events");
    }

    #[test]
    fn accepts_returns_true_when_strategic() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project("my-project", "/tmp/test");
        let block = StrategicAssessor::new(agent, registry);
        let trigger = test_event!(EventType::ProjectIterationRequested, "my-project", {
            "project": "my-project", "workflow": "iterate", "strategic": true
        });

        assert!(block.accepts(&trigger), "should accept strategic events");
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
        let trigger = test_event!(EventType::ProjectIterationRequested, "my-project", {
            "project": "my-project", "workflow": "iterate", "strategic": true
        });

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
        let trigger = test_event!(EventType::ProjectIterationRequested, "my-project", {
            "project": "my-project", "workflow": "iterate", "strategic": true, "actions": {"maintain": true}
        });

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
