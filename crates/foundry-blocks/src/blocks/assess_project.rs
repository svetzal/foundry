use std::path::PathBuf;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{AssessmentCompletedPayload, ChainContext, PreflightCompletedPayload};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::{
    AgentAccess, AgentGateway, AgentOutcome, AgentProvider, ModelTier, ReasoningEffort,
};

use super::{AgentBlockSpec, TriggerContext};

agent_block_new!(
    /// Identifies the most-violated engineering principle in the project.
    ///
    /// Observer — sinks on `PreflightCompleted` (filters for iterate workflow + passed only).
    /// Uses `AgentGateway` with `Reasoning` capability and `ReadOnly` access for the
    /// assessment, then `Quick` capability for generating a kebab-case audit filename.
    /// Emits `AssessmentCompleted` with severity, principle, category, prose, and audit name.
    pub struct AssessProject
);

impl TaskBlock for AssessProject {
    task_block_meta! {
        name: "Assess Project",
        kind: Observer,
        sinks_on: [PreflightCompleted],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project,
            throttle,
            payload,
        } = TriggerContext::from_trigger(trigger);

        // Self-filter: only run for iterate workflow with passed preflight
        let p = parse_payload!(trigger, PreflightCompletedPayload);
        let workflow = WorkflowType::from_payload(&payload);
        let all_passed = p.all_passed;

        if workflow != WorkflowType::Iterate || !all_passed {
            return skip!("Skipped: not an iterate workflow or preflight failed");
        }

        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);

        let provider = super::chain_agent_provider(&payload);

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);
            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            let (severity, principle, category, assessment) = match run_assessment_agent(
                &agent,
                &project,
                project_path.clone(),
                agent_file.clone(),
                provider,
                entry.timeout(),
            )
            .await
            {
                Ok(v) => v,
                Err(err) => {
                    return Ok(TaskBlockResult::failure(format!("agent unavailable: {err}")));
                }
            };

            let audit_name = run_naming_agent(
                &agent,
                &project,
                NamingAgentArgs {
                    principle: principle.clone(),
                    category: category.clone(),
                    project_path,
                    agent_file,
                    provider,
                },
            )
            .await;

            tracing::info!(
                project = %project,
                severity = severity,
                principle = %principle,
                category = %category,
                audit_name = %audit_name,
                "assessment completed"
            );

            let chain = ChainContext::extract_from(&payload);
            super::emit_result(
                format!("{project}: assessed — severity {severity}, {principle}"),
                EventType::AssessmentCompleted,
                &project,
                throttle,
                &AssessmentCompletedPayload {
                    project: project.clone(),
                    // SAFETY: severity.max(0) is guaranteed non-negative; cast is lossless.
                    #[allow(clippy::cast_sign_loss)]
                    severity: severity.max(0) as u64,
                    principle: principle.clone(),
                    category: category.clone(),
                    assessment: assessment.clone(),
                    audit_name: Some(audit_name.clone()),
                    workflow: WorkflowType::Iterate.to_string(),
                    chain,
                },
            )
        })
    }
}

async fn run_assessment_agent(
    agent: &Arc<dyn AgentGateway>,
    project: &str,
    project_path: PathBuf,
    agent_file: Option<PathBuf>,
    provider: Option<AgentProvider>,
    timeout: std::time::Duration,
) -> anyhow::Result<(i64, String, String, String)> {
    let assess_prompt = format!(
        "You are assessing the project '{project}' for code quality improvements.\n\n\
         Analyze the codebase and identify the single most-violated engineering principle. \
         Consider: code clarity, test coverage, error handling, naming, duplication, \
         separation of concerns, and adherence to the project's stated conventions.\n\n\
         Output ONLY valid JSON in this exact format, nothing else:\n\
         {{\n  \
           \"severity\": <1-10 integer>,\n  \
           \"principle\": \"<the principle being violated>\",\n  \
           \"category\": \"<one of: clarity, testing, error-handling, naming, duplication, architecture, conventions>\",\n  \
           \"assessment\": \"<2-3 sentence description of the violation and where it occurs>\"\n\
         }}"
    );

    let outcome = super::invoke_agent(
        agent.as_ref(),
        AgentBlockSpec {
            prompt: assess_prompt,
            working_dir: project_path,
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Deep,
            effort: ReasoningEffort::High,
            agent_file,
            provider,
            timeout,
        },
        "assess project",
        project,
    )
    .await;

    match outcome {
        AgentOutcome::Success { stdout } => Ok(parse_assessment(&stdout)),
        AgentOutcome::AgentFailed { stderr } => {
            tracing::warn!(project = %project, stderr = %stderr, "assessment agent failed");
            Ok((5, "unknown".to_string(), "conventions".to_string(), stderr))
        }
        AgentOutcome::Unavailable { error } => Err(anyhow::anyhow!(error)),
    }
}

struct NamingAgentArgs {
    principle: String,
    category: String,
    project_path: PathBuf,
    agent_file: Option<PathBuf>,
    provider: Option<AgentProvider>,
}

async fn run_naming_agent(
    agent: &Arc<dyn AgentGateway>,
    project: &str,
    args: NamingAgentArgs,
) -> String {
    let NamingAgentArgs {
        principle,
        category,
        project_path,
        agent_file,
        provider,
    } = args;
    let name_prompt = format!(
        "Generate a short kebab-case filename (no extension) that describes this assessment: \
         principle={principle}, category={category}. \
         Output ONLY the kebab-case string, nothing else. Example: fix-error-handling"
    );

    let outcome = super::invoke_agent(
        agent.as_ref(),
        AgentBlockSpec {
            prompt: name_prompt,
            working_dir: project_path,
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Fast,
            effort: ReasoningEffort::Low,
            agent_file,
            provider,
            timeout: std::time::Duration::from_secs(60),
        },
        "name assessment",
        project,
    )
    .await;

    if let AgentOutcome::Success { stdout } = outcome {
        let name = stdout.trim().to_string();
        if name.is_empty() {
            format!("assess-{category}")
        } else {
            name
        }
    } else {
        tracing::warn!(project = %project, "naming agent failed, using fallback");
        format!("assess-{category}")
    }
}

/// Parse the JSON assessment output from the agent.
fn parse_assessment(output: &str) -> (i64, String, String, String) {
    if let Some(json) = super::parse_agent_json(output) {
        let severity = json.get("severity").and_then(serde_json::Value::as_i64).unwrap_or(5);
        let principle = json
            .get("principle")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let category = json
            .get("category")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("conventions")
            .to_string();
        let assessment = json
            .get("assessment")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        (severity, principle, category, assessment)
    } else {
        // Fallback: use first line as assessment
        let first_line = output.lines().next().unwrap_or("assessment failed");
        (5, "unknown".to_string(), "conventions".to_string(), first_line.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::Registry;
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{AgentAccess, ModelTier, ReasoningEffort};

    use super::super::test_helpers;
    use super::{AssessProject, parse_assessment};

    assert_block_meta!(
        AssessProject::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry { version: 2, projects: vec![] })),
        ),
        kind: Observer,
        sinks_on: [PreflightCompleted],
    );

    #[tokio::test]
    async fn skips_non_iterate_workflow() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project("my-project", "/tmp/test");
        let block = AssessProject::new(agent.clone(), registry);
        let trigger = Event::new(
            EventType::PreflightCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "maintain",
                "all_passed": true,
                "required_passed": true,
                "results": [],
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(agent.invocations().is_empty());
    }

    #[tokio::test]
    async fn skips_failed_preflight() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project("my-project", "/tmp/test");
        let block = AssessProject::new(agent.clone(), registry);
        let trigger = Event::new(
            EventType::PreflightCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "iterate",
                "all_passed": false,
                "required_passed": false,
                "results": [],
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn assesses_project_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::sequence(vec![
            // Assessment response
            crate::gateway::AgentResponse {
                stdout: r#"{"severity": 7, "principle": "DRY", "category": "duplication", "assessment": "Several methods duplicate validation logic."}"#.to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // Name generation response
            crate::gateway::AgentResponse {
                stdout: "fix-duplicate-validation".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = AssessProject::new(agent.clone(), registry);
        let trigger = test_event!(EventType::PreflightCompleted, "my-project", {
            "project": "my-project",
            "workflow": "iterate",
            "all_passed": true,
            "required_passed": true,
            "results": [],
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::AssessmentCompleted);
        assert_eq!(result.events[0].payload["severity"], 7);
        assert_eq!(result.events[0].payload["principle"], "DRY");
        assert_eq!(result.events[0].payload["category"], "duplication");
        assert_eq!(result.events[0].payload["audit_name"], "fix-duplicate-validation");

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 2);
        assert_eq!(invocations[0].access, AgentAccess::ReadOnly);
        assert_eq!(invocations[0].tier, ModelTier::Deep);
        assert_eq!(invocations[0].effort, ReasoningEffort::High);
        assert_eq!(invocations[1].tier, ModelTier::Fast);
        assert_eq!(invocations[1].effort, ReasoningEffort::Low);
    }

    #[test]
    fn parse_assessment_extracts_json() {
        let output = r#"{"severity": 8, "principle": "SRP", "category": "architecture", "assessment": "Too many responsibilities."}"#;
        let (severity, principle, category, assessment) = parse_assessment(output);
        assert_eq!(severity, 8);
        assert_eq!(principle, "SRP");
        assert_eq!(category, "architecture");
        assert_eq!(assessment, "Too many responsibilities.");
    }

    #[test]
    fn parse_assessment_handles_surrounding_text() {
        let output = "Here is my assessment:\n{\"severity\": 3, \"principle\": \"naming\", \"category\": \"naming\", \"assessment\": \"Poor names.\"}\nDone.";
        let (severity, principle, _, _) = parse_assessment(output);
        assert_eq!(severity, 3);
        assert_eq!(principle, "naming");
    }

    #[test]
    fn parse_assessment_fallback_on_invalid_json() {
        let output = "This is not JSON at all";
        let (severity, principle, category, _) = parse_assessment(output);
        assert_eq!(severity, 5);
        assert_eq!(principle, "unknown");
        assert_eq!(category, "conventions");
    }
}
