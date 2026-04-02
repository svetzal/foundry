use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType, PayloadExt};
use foundry_core::loop_context::forward_chain_context;
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

/// Identifies the most-violated engineering principle in the project.
///
/// Observer — sinks on `PreflightCompleted` (filters for iterate workflow + passed only).
/// Uses `AgentGateway` with `Reasoning` capability and `ReadOnly` access for the
/// assessment, then `Quick` capability for generating a kebab-case audit filename.
/// Emits `AssessmentCompleted` with severity, principle, category, prose, and audit name.
pub struct AssessProject {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl AssessProject {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, agent }
    }
}

impl TaskBlock for AssessProject {
    task_block_meta! {
        name: "Assess Project",
        kind: Observer,
        sinks_on: [PreflightCompleted],
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

        // Self-filter: only run for iterate workflow with passed preflight
        let workflow = trigger.payload_str_or("workflow", "unknown").to_string();
        let all_passed = trigger.payload_bool_or("all_passed", false);

        if workflow != "iterate" || !all_passed {
            return Box::pin(async {
                Ok(TaskBlockResult::success(
                    "Skipped: not an iterate workflow or preflight failed",
                    vec![],
                ))
            });
        }

        let entry = self.registry.find_project(&project).cloned();
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            let Some(entry) = entry else {
                return Ok(super::project_not_found_result(&project));
            };

            let project_path = PathBuf::from(&entry.path);
            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            // Assessment prompt
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

            let assess_request = AgentRequest {
                prompt: assess_prompt,
                working_dir: project_path.clone(),
                access: AgentAccess::ReadOnly,
                capability: AgentCapability::Reasoning,
                agent_file: agent_file.clone(),
                timeout: entry.timeout(),
            };

            tracing::info!(project = %project, "assessing project via agent");

            let assess_response = agent.invoke(&assess_request).await;

            let (severity, principle, category, assessment) = match assess_response {
                Ok(r) if r.success => parse_assessment(&r.stdout),
                Ok(r) => {
                    tracing::warn!(project = %project, stderr = %r.stderr, "assessment agent failed");
                    (5, "unknown".to_string(), "conventions".to_string(), r.stderr)
                }
                Err(err) => {
                    tracing::warn!(error = %err, "agent invocation failed for assessment");
                    return Ok(TaskBlockResult::failure(format!("agent unavailable: {err}")));
                }
            };

            // Generate audit filename via Quick agent
            let name_prompt = format!(
                "Generate a short kebab-case filename (no extension) that describes this assessment: \
                 principle={principle}, category={category}. \
                 Output ONLY the kebab-case string, nothing else. Example: fix-error-handling"
            );

            let name_request = AgentRequest {
                prompt: name_prompt,
                working_dir: project_path,
                access: AgentAccess::ReadOnly,
                capability: AgentCapability::Quick,
                agent_file,
                timeout: std::time::Duration::from_secs(60),
            };

            let audit_name = match agent.invoke(&name_request).await {
                Ok(r) if r.success => {
                    let name = r.stdout.trim().to_string();
                    if name.is_empty() {
                        format!("assess-{category}")
                    } else {
                        name
                    }
                }
                _ => format!("assess-{category}"),
            };

            tracing::info!(
                project = %project,
                severity = severity,
                principle = %principle,
                category = %category,
                audit_name = %audit_name,
                "assessment completed"
            );

            let mut event_payload = serde_json::json!({
                "project": project,
                "severity": severity,
                "principle": principle,
                "category": category,
                "assessment": assessment,
                "audit_name": audit_name,
                "workflow": "iterate",
            });
            forward_chain_context(&payload, &mut event_payload);

            Ok(TaskBlockResult::success(
                format!("{project}: assessed — severity {severity}, {principle}"),
                vec![Event::new(
                    EventType::AssessmentCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
            ))
        })
    }
}

/// Parse the JSON assessment output from the agent.
fn parse_assessment(output: &str) -> (i64, String, String, String) {
    // Try to find JSON in the output (agent may include extra text)
    let json_str = extract_json(output);
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) {
        let severity = json.i64_or("severity", 5);
        let principle = json.str_or("principle", "unknown").to_string();
        let category = json.str_or("category", "conventions").to_string();
        let assessment = json.str_or("assessment", "").to_string();
        (severity, principle, category, assessment)
    } else {
        // Fallback: use first line as assessment
        let first_line = output.lines().next().unwrap_or("assessment failed");
        (5, "unknown".to_string(), "conventions".to_string(), first_line.to_string())
    }
}

/// Extract the first JSON object from a string (handles surrounding text).
pub(super) fn extract_json(s: &str) -> String {
    if let Some(start) = s.find('{') {
        if let Some(end) = s.rfind('}') {
            return s[start..=end].to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{AgentAccess, AgentCapability};

    use super::{AssessProject, parse_assessment};

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

    fn preflight_passed_event(project: &str) -> Event {
        Event::new(
            EventType::PreflightCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "workflow": "iterate",
                "all_passed": true,
                "required_passed": true,
            }),
        )
    }

    #[test]
    fn kind_is_observer() {
        let agent = FakeAgentGateway::success();
        let block = AssessProject::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_preflight_completed() {
        let agent = FakeAgentGateway::success();
        let block = AssessProject::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.sinks_on(), &[EventType::PreflightCompleted]);
    }

    #[tokio::test]
    async fn skips_non_iterate_workflow() {
        let agent = FakeAgentGateway::success();
        let registry = registry_with_project("my-project", "/tmp/test");
        let block = AssessProject::new(agent.clone(), registry);
        let trigger = Event::new(
            EventType::PreflightCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "maintain",
                "all_passed": true,
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
        let registry = registry_with_project("my-project", "/tmp/test");
        let block = AssessProject::new(agent.clone(), registry);
        let trigger = Event::new(
            EventType::PreflightCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "iterate",
                "all_passed": false,
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
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = AssessProject::new(agent.clone(), registry);
        let trigger = preflight_passed_event("my-project");

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
        assert_eq!(invocations[0].capability, AgentCapability::Reasoning);
        assert_eq!(invocations[1].capability, AgentCapability::Quick);
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
