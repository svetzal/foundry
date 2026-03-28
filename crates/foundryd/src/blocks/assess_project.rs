use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

/// Assesses the project to identify the most-violated software engineering principle.
///
/// Observer — sinks on `PreflightCompleted`.
/// Self-filter: skips if preflight failed (`all_passed: false`).
/// Uses `AgentGateway` with two invocations:
/// 1. Reasoning + `ReadOnly` — assess the codebase
/// 2. Quick + `ReadOnly` — generate audit filename
///
/// Emits `AssessmentCompleted` with severity, principle, category, description, and audit name.
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
    fn name(&self) -> &'static str {
        "Assess Project"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::PreflightCompleted]
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

        // Self-filter: skip if preflight failed
        let all_passed =
            payload.get("all_passed").and_then(serde_json::Value::as_bool).unwrap_or(false);

        if !all_passed {
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: preflight gates did not pass".to_string(),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                })
            });
        }

        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            let Some(entry) = entry else {
                tracing::warn!(project = %project, "project not found in registry");
                return Ok(TaskBlockResult {
                    events: vec![],
                    success: false,
                    summary: format!("Project '{project}' not found in registry"),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                });
            };

            let project_path = PathBuf::from(&entry.path);
            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            // Invocation 1: Assess the codebase (Reasoning, ReadOnly)
            let assess_prompt = "Examine this project and identify the single most-violated \
                software engineering principle. Return JSON: \
                { \"severity\": \"high\"|\"medium\"|\"low\", \
                \"principle\": \"<name>\", \
                \"category\": \"<category>\", \
                \"description\": \"<prose explanation>\" }"
                .to_string();

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

            let assessment_json = match assess_response {
                Ok(r) if r.success => r.stdout,
                Ok(r) => {
                    tracing::warn!(project = %project, stderr = %r.stderr, "assessment agent failed");
                    return Ok(TaskBlockResult {
                        events: vec![],
                        success: false,
                        summary: format!("{project}: assessment failed"),
                        raw_output: Some(r.stderr),
                        exit_code: Some(r.exit_code),
                        audit_artifacts: vec![],
                    });
                }
                Err(err) => {
                    tracing::warn!(error = %err, "agent invocation failed for assessment");
                    return Ok(TaskBlockResult {
                        events: vec![],
                        success: false,
                        summary: format!("{project}: agent unavailable for assessment: {err}"),
                        raw_output: None,
                        exit_code: None,
                        audit_artifacts: vec![],
                    });
                }
            };

            // Parse the assessment JSON from the agent output
            let assessment: serde_json::Value =
                extract_json(&assessment_json).unwrap_or_else(|| {
                    serde_json::json!({
                        "severity": "medium",
                        "principle": "unknown",
                        "category": "unknown",
                        "description": assessment_json.trim(),
                    })
                });

            let severity = assessment
                .get("severity")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("medium")
                .to_string();
            let principle = assessment
                .get("principle")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let category = assessment
                .get("category")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let description = assessment
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();

            // Invocation 2: Generate audit filename (Quick, ReadOnly)
            let name_prompt = format!(
                "Given this assessment, generate a short kebab-case filename (no extension) \
                 suitable for an audit file. Just output the filename, nothing else. \
                 Assessment: {description}"
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
                Ok(r) if r.success => r.stdout.trim().to_string(),
                _ => "assessment".to_string(),
            };

            tracing::info!(
                project = %project,
                severity = %severity,
                principle = %principle,
                audit_name = %audit_name,
                "assessment completed"
            );

            let mut event_payload = serde_json::json!({
                "project": project,
                "severity": severity,
                "principle": principle,
                "category": category,
                "description": description,
                "audit_name": audit_name,
            });

            // Forward actions and workflow from trigger chain
            if let Some(actions) = payload.get("actions") {
                event_payload["actions"] = actions.clone();
            }
            if let Some(workflow) = payload.get("workflow") {
                event_payload["workflow"] = workflow.clone();
            }

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::AssessmentCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
                success: true,
                summary: format!(
                    "{project}: assessed — {severity} severity, principle: {principle}"
                ),
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
    }
}

/// Try to extract a JSON object from agent output that may contain surrounding prose.
pub(super) fn extract_json(output: &str) -> Option<serde_json::Value> {
    // Try direct parse first
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(output.trim()) {
        if val.is_object() {
            return Some(val);
        }
    }

    // Look for JSON object in the output
    let trimmed = output.trim();
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&trimmed[start..=end]) {
                if val.is_object() {
                    return Some(val);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{AgentAccess, AgentCapability, AgentResponse};

    use super::{AssessProject, extract_json};

    fn registry_with_project(name: &str, path: &str) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: name.to_string(),
                path: path.to_string(),
                stack: Stack::Rust,
                agent: "rust-craftsperson".to_string(),
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

    fn preflight_passed(project: &str) -> Event {
        Event::new(
            EventType::PreflightCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "all_passed": true,
                "actions": {"iterate": true},
            }),
        )
    }

    fn preflight_failed(project: &str) -> Event {
        Event::new(
            EventType::PreflightCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "all_passed": false,
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
    async fn skips_when_preflight_failed() {
        let agent = FakeAgentGateway::success();
        let registry = registry_with_project("my-project", "/tmp/test");
        let block = AssessProject::new(agent.clone(), registry);
        let trigger = preflight_failed("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(agent.invocations().is_empty());
    }

    #[tokio::test]
    async fn assesses_project_with_two_agent_calls() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::sequence(vec![
            // First call: assessment
            AgentResponse {
                stdout: r#"{ "severity": "high", "principle": "Single Responsibility", "category": "design", "description": "Classes have too many responsibilities" }"#.to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // Second call: audit filename
            AgentResponse {
                stdout: "single-responsibility-violation".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);

        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = AssessProject::new(agent.clone(), registry);
        let trigger = preflight_passed("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::AssessmentCompleted);
        assert_eq!(result.events[0].payload["severity"], "high");
        assert_eq!(result.events[0].payload["principle"], "Single Responsibility");
        assert_eq!(result.events[0].payload["category"], "design");
        assert_eq!(result.events[0].payload["audit_name"], "single-responsibility-violation");

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 2);
        assert_eq!(invocations[0].capability, AgentCapability::Reasoning);
        assert_eq!(invocations[0].access, AgentAccess::ReadOnly);
        assert_eq!(invocations[1].capability, AgentCapability::Quick);
        assert_eq!(invocations[1].access, AgentAccess::ReadOnly);
    }

    #[tokio::test]
    async fn forwards_actions_from_payload() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::sequence(vec![
            AgentResponse {
                stdout: r#"{ "severity": "low", "principle": "DRY", "category": "design", "description": "Some duplication" }"#.to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            AgentResponse {
                stdout: "dry-duplication".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);

        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = AssessProject::new(agent, registry);
        let trigger = preflight_passed("my-project");

        let result = block.execute(&trigger).await.unwrap();

        let actions = result.events[0].payload.get("actions").unwrap();
        assert_eq!(actions["iterate"], true);
    }

    #[tokio::test]
    async fn assessment_agent_failure_returns_failure() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::failure("assessment error");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = AssessProject::new(agent, registry);
        let trigger = preflight_passed("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
    }

    #[test]
    fn extract_json_from_clean_output() {
        let output = r#"{ "severity": "high", "principle": "SRP" }"#;
        let val = extract_json(output).unwrap();
        assert_eq!(val["severity"], "high");
    }

    #[test]
    fn extract_json_from_prose_wrapped_output() {
        let output = "Here is the assessment:\n{ \"severity\": \"medium\" }\nDone.";
        let val = extract_json(output).unwrap();
        assert_eq!(val["severity"], "medium");
    }

    #[test]
    fn extract_json_returns_none_for_non_json() {
        assert!(extract_json("no json here").is_none());
    }
}
