use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType, PayloadExt};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

/// Executes the maintain workflow: updates dependencies, fixes vulnerabilities,
/// and resolves quality gate failures.
///
/// Mutator — sinks on `GateResolutionCompleted` (workflow = "maintain" only).
/// Uses `AgentGateway` with `Coding` capability and `Full` access.
/// Emits `ExecutionCompleted` with success status and details.
pub struct ExecuteMaintain {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl ExecuteMaintain {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, agent }
    }
}

/// Resolve the agent file path from the registry agent name.
/// Convention: `~/.claude/agents/{agent}.md`
pub(super) fn resolve_agent_file(agent_name: &str) -> Option<PathBuf> {
    if agent_name.is_empty() {
        return None;
    }
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home)
        .join(".claude")
        .join("agents")
        .join(format!("{agent_name}.md"));
    if path.exists() { Some(path) } else { None }
}

impl TaskBlock for ExecuteMaintain {
    task_block_meta! {
        name: "Execute Maintain",
        kind: Mutator,
        sinks_on: [GateResolutionCompleted],
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        let workflow = trigger.payload.str_or("workflow", "unknown");
        if workflow != "maintain" {
            return vec![];
        }

        vec![Event::new(
            EventType::ExecutionCompleted,
            trigger.project.clone(),
            trigger.throttle,
            serde_json::json!({
                "project": trigger.project,
                "workflow": "maintain",
                "success": true,
                "dry_run": true,
            }),
        )]
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

        // Self-filter: only handle maintain workflow
        let workflow = payload.str_or("workflow", "unknown").to_string();

        if workflow != "maintain" {
            return Box::pin(async {
                Ok(TaskBlockResult::success("Skipped: not a maintain workflow", vec![]))
            });
        }

        let entry = self.registry.find_project(&project).cloned();
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            let Some(entry) = entry else {
                return Ok(super::project_not_found_result(&project));
            };

            let project_path = PathBuf::from(&entry.path);

            // Build gate context for the prompt
            let gates_context = if let Some(gates) = payload.get("gates") {
                format!(
                    "\n\nThe following quality gates must pass after your changes:\n{}",
                    serde_json::to_string_pretty(gates).unwrap_or_default()
                )
            } else {
                String::new()
            };

            let prompt = format!(
                "You are maintaining the project '{project}'. \
                 Update dependencies to their latest compatible versions, \
                 fix any known vulnerabilities, and resolve any quality gate failures. \
                 Make only the changes necessary to bring the project up to date \
                 and ensure all gates pass.{gates_context}"
            );

            let agent_file = resolve_agent_file(&entry.agent);

            let request = AgentRequest {
                prompt,
                working_dir: project_path,
                access: AgentAccess::Full,
                capability: AgentCapability::Coding,
                agent_file,
                timeout: entry.timeout(),
            };

            tracing::info!(project = %project, "executing maintain via agent");

            let response = agent.invoke(&request).await;

            let (raw_output, exit_code, success, summary) = match response {
                Ok(r) => {
                    let s = r.success;
                    let out = format!("{}\n{}", r.stdout, r.stderr).trim().to_string();
                    let summary = if s {
                        "maintenance completed".to_string()
                    } else {
                        let first_line = r.stderr.lines().next().unwrap_or("agent failed");
                        format!("maintenance failed: {first_line}")
                    };
                    (Some(out), Some(r.exit_code), s, summary)
                }
                Err(err) => {
                    tracing::warn!(error = %err, "agent invocation failed");
                    (None, None, false, format!("agent unavailable: {err}"))
                }
            };

            tracing::info!(project = %project, success = success, "maintain execution completed");

            let mut event_payload = serde_json::json!({
                "project": project,
                "workflow": "maintain",
                "success": success,
                "summary": summary,
            });
            if let Some(actions) = payload.get("actions") {
                event_payload["actions"] = actions.clone();
            }

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ExecutionCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
                success,
                summary: format!("{project}: {summary}"),
                raw_output,
                exit_code,
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

    use super::ExecuteMaintain;

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

    fn gate_resolution_maintain(project: &str) -> Event {
        Event::new(
            EventType::GateResolutionCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "workflow": "maintain",
                "gates": [
                    {"name": "fmt", "command": "cargo fmt --check", "required": true}
                ],
            }),
        )
    }

    fn gate_resolution_iterate(project: &str) -> Event {
        Event::new(
            EventType::GateResolutionCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "workflow": "iterate",
                "gates": [],
            }),
        )
    }

    #[test]
    fn kind_is_mutator() {
        let agent = FakeAgentGateway::success();
        let block = ExecuteMaintain::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.kind(), BlockKind::Mutator);
    }

    #[test]
    fn sinks_on_gate_resolution_completed() {
        let agent = FakeAgentGateway::success();
        let block = ExecuteMaintain::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.sinks_on(), &[EventType::GateResolutionCompleted]);
    }

    #[tokio::test]
    async fn skips_iterate_workflow() {
        let agent = FakeAgentGateway::success();
        let registry = registry_with_project("my-project", "/tmp/test");
        let block = ExecuteMaintain::new(agent.clone(), registry);
        let trigger = gate_resolution_iterate("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("not a maintain"));
        assert!(agent.invocations().is_empty());
    }

    #[tokio::test]
    async fn executes_maintain_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Dependencies updated");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ExecuteMaintain::new(agent.clone(), registry);
        let trigger = gate_resolution_maintain("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(result.events[0].payload["workflow"], "maintain");
        assert_eq!(result.events[0].payload["success"], true);

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].access, AgentAccess::Full);
        assert_eq!(invocations[0].capability, AgentCapability::Coding);
        assert!(invocations[0].prompt.contains("maintaining"));
    }

    #[tokio::test]
    async fn includes_gate_definitions_in_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ExecuteMaintain::new(agent.clone(), registry);
        let trigger = gate_resolution_maintain("my-project");

        block.execute(&trigger).await.unwrap();

        let invocations = agent.invocations();
        assert!(invocations[0].prompt.contains("quality gates"));
        assert!(invocations[0].prompt.contains("fmt"));
    }

    #[tokio::test]
    async fn project_not_in_registry_returns_failure() {
        let agent = FakeAgentGateway::success();
        let block = ExecuteMaintain::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        let trigger = gate_resolution_maintain("unknown-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("not found"));
    }

    #[tokio::test]
    async fn agent_failure_emits_execution_completed_with_failure() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::failure("something went wrong");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ExecuteMaintain::new(agent, registry);
        let trigger = gate_resolution_maintain("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(result.events[0].payload["success"], false);
    }

    #[tokio::test]
    async fn forwards_actions_from_payload() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ExecuteMaintain::new(agent, registry);
        let trigger = Event::new(
            EventType::GateResolutionCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "maintain",
                "gates": [],
                "actions": {"maintain": true},
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        let actions = result.events[0].payload.get("actions").unwrap();
        assert_eq!(actions["maintain"], true);
    }

    #[test]
    fn dry_run_emits_for_maintain_workflow() {
        let agent = FakeAgentGateway::success();
        let block = ExecuteMaintain::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        let trigger = gate_resolution_maintain("my-project");

        let events = block.dry_run_events(&trigger);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(events[0].payload["dry_run"], true);
    }

    #[test]
    fn dry_run_skips_iterate_workflow() {
        let agent = FakeAgentGateway::success();
        let block = ExecuteMaintain::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        let trigger = gate_resolution_iterate("my-project");

        let events = block.dry_run_events(&trigger);

        assert!(events.is_empty());
    }
}
