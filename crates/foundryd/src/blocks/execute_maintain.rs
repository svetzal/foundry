use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use foundry_core::event::{Event, EventType};
use foundry_core::payload::GateResolutionCompletedPayload;
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::workflow::WorkflowType;

use crate::gateway::{AgentGateway, ProcessShellGateway, ShellGateway};

use super::TriggerContext;

/// Executes the maintain workflow: updates dependencies, fixes vulnerabilities,
/// and resolves quality gate failures.
///
/// Mutator — sinks on `GateResolutionCompleted` (workflow = "maintain" only).
/// Uses `AgentGateway` with `Coding` capability and `Full` access.
/// Emits `ExecutionCompleted` with success status and `changes_detected` flag.
pub struct ExecuteMaintain {
    registry: Arc<RwLock<Registry>>,
    agent: Arc<dyn AgentGateway>,
    shell: Arc<dyn ShellGateway>,
}

impl ExecuteMaintain {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<RwLock<Registry>>) -> Self {
        Self {
            registry,
            agent,
            shell: Arc::new(ProcessShellGateway),
        }
    }

    #[cfg(test)]
    fn with_gateways(
        agent: Arc<dyn AgentGateway>,
        registry: Arc<RwLock<Registry>>,
        shell: Arc<dyn ShellGateway>,
    ) -> Self {
        Self {
            registry,
            agent,
            shell,
        }
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
        let workflow = WorkflowType::from_payload(&trigger.payload);
        if workflow != WorkflowType::Maintain {
            return vec![];
        }
        super::dry_run_execution_event(trigger, WorkflowType::Maintain, None)
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

        // Self-filter: only handle maintain workflow
        let workflow = WorkflowType::from_payload(&payload);

        if workflow != WorkflowType::Maintain {
            return skip!("Skipped: not a maintain workflow");
        }

        let p = parse_payload!(trigger, GateResolutionCompletedPayload);
        let gates = p.gates;

        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);

            let prompt = build_maintain_prompt(&project, Some(&gates).filter(|v| !v.is_null()));

            let agent_file = resolve_agent_file(&entry.agent);

            // Capture HEAD before the agent runs so post-execution change
            // detection can compare against a stable snapshot.
            let pre_sha = super::capture_pre_execution_sha(&*shell, &project_path).await;

            let outcome = super::invoke_coding_agent(
                &*agent,
                &project,
                project_path.clone(),
                prompt,
                agent_file,
                entry.timeout(),
                "maintain",
            )
            .await;

            Ok(super::build_execution_outcome(
                &*shell,
                &project_path,
                &project,
                WorkflowType::Maintain,
                outcome,
                &payload,
                throttle,
                "maintenance",
                None,
                pre_sha,
                // Maintain is never the iterate workflow, so correction_needed is
                // irrelevant — the clean-tree override only fires for WorkflowType::Iterate.
                true,
            )
            .await)
        })
    }
}

fn build_maintain_prompt(project: &str, gates: Option<&serde_json::Value>) -> String {
    let gates_context = super::format_gates_context(gates);
    format!(
        "You are maintaining the project '{project}'. \
         Update dependencies to their latest compatible versions, \
         fix any known vulnerabilities, and resolve any quality gate failures. \
         Make only the changes necessary to bring the project up to date \
         and ensure all gates pass.{gates_context}"
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::Registry;
    use foundry_core::task_block::TaskBlock;
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{AgentAccess, AgentCapability};

    use super::super::test_helpers;
    use super::ExecuteMaintain;

    assert_block_meta!(
        ExecuteMaintain::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry { version: 2, projects: vec![] })),
        ),
        kind: Mutator,
        sinks_on: [GateResolutionCompleted],
    );

    #[tokio::test]
    async fn skips_iterate_workflow() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            "/tmp/test",
            "rust-craftsperson",
        ));
        let block = ExecuteMaintain::new(agent.clone(), registry);
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "iterate",
            "gates": [],
        });

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
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "rust-craftsperson",
        ));
        let block = ExecuteMaintain::new(agent.clone(), registry);
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "maintain",
            "gates": [
                {"name": "fmt", "command": "cargo fmt --check", "required": true}
            ],
        });

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
    async fn emitted_event_includes_execution_output() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Dependencies updated");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "rust-craftsperson",
        ));
        let block = ExecuteMaintain::new(agent, registry);
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "maintain",
            "gates": [
                {"name": "fmt", "command": "cargo fmt --check", "required": true}
            ],
        });

        let result = block.execute(&trigger).await.unwrap();

        let exec_output = result.events[0].payload.get("execution_output").and_then(|v| v.as_str());
        assert!(
            exec_output.is_some(),
            "ExecutionCompleted should include execution_output in payload",
        );
        assert!(
            exec_output.unwrap().contains("Dependencies updated"),
            "execution_output should contain agent stdout",
        );
    }

    #[tokio::test]
    async fn includes_gate_definitions_in_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "rust-craftsperson",
        ));
        let block = ExecuteMaintain::new(agent.clone(), registry);
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "maintain",
            "gates": [
                {"name": "fmt", "command": "cargo fmt --check", "required": true}
            ],
        });

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
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        let trigger = test_event!(EventType::GateResolutionCompleted, "unknown-project", {
            "project": "unknown-project",
            "workflow": "maintain",
            "gates": [
                {"name": "fmt", "command": "cargo fmt --check", "required": true}
            ],
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("not found"));
    }

    #[tokio::test]
    async fn agent_failure_emits_execution_completed_with_failure() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::failure("something went wrong");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "rust-craftsperson",
        ));
        let block = ExecuteMaintain::new(agent, registry);
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "maintain",
            "gates": [
                {"name": "fmt", "command": "cargo fmt --check", "required": true}
            ],
        });

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
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "rust-craftsperson",
        ));
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
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "maintain",
            "gates": [
                {"name": "fmt", "command": "cargo fmt --check", "required": true}
            ],
        });

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
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "iterate",
            "gates": [],
        });

        let events = block.dry_run_events(&trigger);

        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn detects_changes_when_tree_dirty() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Dependencies updated");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "rust-craftsperson",
        ));
        // Shell sequence: rev-parse HEAD → sha; git diff --name-only <sha> → files
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: "abc123\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: "Cargo.lock\nnew-patch.txt\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = ExecuteMaintain::with_gateways(agent, registry, shell);
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "maintain",
            "gates": [
                {"name": "fmt", "command": "cargo fmt --check", "required": true}
            ],
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["changes_detected"], true);
        let files = result.events[0].payload["files_changed"].as_array().unwrap();
        assert!(files.iter().any(|f| f == "Cargo.lock"), "expected Cargo.lock in {files:?}");
        assert!(
            files.iter().any(|f| f == "new-patch.txt"),
            "expected new-patch.txt in {files:?}"
        );
    }

    #[tokio::test]
    async fn reports_no_changes_when_tree_clean() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "rust-craftsperson",
        ));
        let shell = FakeShellGateway::success(); // empty stdout
        let block = ExecuteMaintain::with_gateways(agent, registry, shell);
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "maintain",
            "gates": [
                {"name": "fmt", "command": "cargo fmt --check", "required": true}
            ],
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["changes_detected"], false);
        assert!(
            result.events[0]
                .payload
                .get("files_changed")
                .is_none_or(|v| v.as_array().is_none_or(std::vec::Vec::is_empty))
        );
    }

    #[tokio::test]
    async fn maintain_clean_tree_remains_success() {
        use crate::gateway::fakes::FakeShellGateway;

        // Maintain workflow must NOT apply the iterate override — clean tree stays success.
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "rust-craftsperson",
        ));
        let shell = FakeShellGateway::success(); // empty stdout → no changes
        let block = ExecuteMaintain::with_gateways(agent, registry, shell);
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "maintain",
            "gates": [
                {"name": "fmt", "command": "cargo fmt --check", "required": true}
            ],
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success, "maintain workflow must NOT override to failure on clean tree");
        assert_eq!(result.events[0].payload["success"], true);
    }

    #[tokio::test]
    async fn tolerates_git_status_failure() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Dependencies updated");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "rust-craftsperson",
        ));
        let shell = FakeShellGateway::failure("fatal: not a git repository");
        let block = ExecuteMaintain::with_gateways(agent, registry, shell);
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "maintain",
            "gates": [
                {"name": "fmt", "command": "cargo fmt --check", "required": true}
            ],
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["changes_detected"], false);
    }
}
