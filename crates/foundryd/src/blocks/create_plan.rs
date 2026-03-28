use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

/// Creates a step-by-step plan to fix the identified code quality issue.
///
/// Observer — sinks on `TriageCompleted`.
/// Self-filter: skips if triage rejected (`accepted: false`).
/// Uses `AgentGateway` with `Reasoning` capability and `ReadOnly` access.
/// Emits `PlanCompleted` with the plan text and forwarded assessment.
pub struct CreatePlan {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl CreatePlan {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, agent }
    }
}

impl TaskBlock for CreatePlan {
    fn name(&self) -> &'static str {
        "Create Plan"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::TriageCompleted]
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

        // Self-filter: skip if triage rejected
        let accepted =
            payload.get("accepted").and_then(serde_json::Value::as_bool).unwrap_or(false);

        if !accepted {
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: triage rejected this assessment".to_string(),
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

            let assessment = payload.get("assessment").cloned().unwrap_or_default();
            let description = assessment
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown issue");
            let principle = assessment
                .get("principle")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown principle");

            let prompt = format!(
                "Create a step-by-step plan to fix this code quality issue. \
                 Be specific about which files to modify and what changes to make. \
                 Assessment: {description}. Principle violated: {principle}."
            );

            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            let request = AgentRequest {
                prompt,
                working_dir: project_path,
                access: AgentAccess::ReadOnly,
                capability: AgentCapability::Reasoning,
                agent_file,
                timeout: entry.timeout(),
            };

            tracing::info!(project = %project, "creating plan via agent");

            let response = agent.invoke(&request).await;

            let (plan, success) = match response {
                Ok(r) if r.success => (r.stdout.trim().to_string(), true),
                Ok(r) => {
                    tracing::warn!(project = %project, stderr = %r.stderr, "plan agent failed");
                    return Ok(TaskBlockResult {
                        events: vec![],
                        success: false,
                        summary: format!("{project}: plan creation failed"),
                        raw_output: Some(r.stderr),
                        exit_code: Some(r.exit_code),
                        audit_artifacts: vec![],
                    });
                }
                Err(err) => {
                    tracing::warn!(error = %err, "agent invocation failed for plan");
                    return Ok(TaskBlockResult {
                        events: vec![],
                        success: false,
                        summary: format!("{project}: agent unavailable for planning: {err}"),
                        raw_output: None,
                        exit_code: None,
                        audit_artifacts: vec![],
                    });
                }
            };

            tracing::info!(project = %project, "plan created");

            let mut event_payload = serde_json::json!({
                "project": project,
                "plan": plan,
                "assessment": assessment,
            });

            // Forward actions and audit_name from trigger chain
            if let Some(actions) = payload.get("actions") {
                event_payload["actions"] = actions.clone();
            }
            if let Some(audit_name) = payload.get("audit_name") {
                event_payload["audit_name"] = audit_name.clone();
            }

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::PlanCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
                success,
                summary: format!("{project}: plan created"),
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
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
    use crate::gateway::{AgentAccess, AgentCapability};

    use super::CreatePlan;

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

    fn triage_accepted(project: &str) -> Event {
        Event::new(
            EventType::TriageCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "accepted": true,
                "reason": "High severity worth fixing",
                "assessment": {
                    "severity": "high",
                    "principle": "Single Responsibility",
                    "category": "design",
                    "description": "Classes have too many responsibilities",
                },
                "audit_name": "srp-violation",
                "actions": {"iterate": true},
            }),
        )
    }

    fn triage_rejected(project: &str) -> Event {
        Event::new(
            EventType::TriageCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "accepted": false,
                "reason": "Low severity, not worth it",
            }),
        )
    }

    #[test]
    fn kind_is_observer() {
        let agent = FakeAgentGateway::success();
        let block = CreatePlan::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_triage_completed() {
        let agent = FakeAgentGateway::success();
        let block = CreatePlan::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.sinks_on(), &[EventType::TriageCompleted]);
    }

    #[tokio::test]
    async fn creates_plan_when_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(
            "1. Extract UserService from UserController\n2. Move validation logic to separate module",
        );
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CreatePlan::new(agent.clone(), registry);
        let trigger = triage_accepted("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::PlanCompleted);
        assert!(result.events[0].payload["plan"].as_str().unwrap().contains("Extract"));
        assert_eq!(result.events[0].payload["assessment"]["principle"], "Single Responsibility");

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].capability, AgentCapability::Reasoning);
        assert_eq!(invocations[0].access, AgentAccess::ReadOnly);
        assert!(invocations[0].prompt.contains("Single Responsibility"));
    }

    #[tokio::test]
    async fn skips_when_rejected() {
        let agent = FakeAgentGateway::success();
        let registry = registry_with_project("my-project", "/tmp/test");
        let block = CreatePlan::new(agent.clone(), registry);
        let trigger = triage_rejected("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(agent.invocations().is_empty());
    }

    #[tokio::test]
    async fn forwards_actions_and_audit_name() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Step 1: fix it");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CreatePlan::new(agent, registry);
        let trigger = triage_accepted("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events[0].payload["actions"]["iterate"], true);
        assert_eq!(result.events[0].payload["audit_name"], "srp-violation");
    }

    #[tokio::test]
    async fn agent_failure_returns_failure() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::failure("plan error");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CreatePlan::new(agent, registry);
        let trigger = triage_accepted("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
    }
}
