use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::GateResolutionCompletedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock};
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::{AgentGateway, ProcessShellGateway, ShellGateway};

use super::{ExecutionContext, SimulatedSuccess, TriggerContext};

agent_execution_block! {
    /// Executes the maintain workflow: updates dependencies, fixes vulnerabilities,
    /// and resolves quality gate failures.
    ///
    /// Mutator — sinks on `GateResolutionCompleted` (workflow = "maintain" only).
    /// Uses `AgentGateway` with `Coding` capability and `Full` access.
    /// Emits `ExecutionCompleted` with success status and `changes_detected` flag.
    pub struct ExecuteMaintain
}

/// Decision outcome for an execute-maintain trigger.
///
/// Centralises the workflow-type guard shared between `dry_run_events` and `execute`.
#[derive(Debug, PartialEq)]
enum MaintainDecision {
    /// Trigger is not a maintain workflow — skip.
    SkipNonMaintain,
    /// Trigger is a maintain workflow — proceed.
    Proceed,
}

/// Evaluate the trigger and decide what `ExecuteMaintain` should do.
fn decide_maintain(trigger: &Event) -> MaintainDecision {
    if WorkflowType::from_payload(&trigger.payload) == WorkflowType::Maintain {
        MaintainDecision::Proceed
    } else {
        MaintainDecision::SkipNonMaintain
    }
}

impl SimulatedSuccess for ExecuteMaintain {
    type Outcome = Option<()>;

    fn simulate(&self, trigger: &Event) -> Option<()> {
        match decide_maintain(trigger) {
            MaintainDecision::SkipNonMaintain => None,
            MaintainDecision::Proceed => Some(()),
        }
    }

    fn success_events(&self, trigger: &Event, outcome: &Option<()>) -> Vec<Event> {
        match outcome {
            None => vec![],
            Some(()) => super::dry_run_execution_event(trigger, WorkflowType::Maintain, None),
        }
    }
}

impl TaskBlock for ExecuteMaintain {
    task_block_meta! {
        name: "Execute Maintain",
        kind: Mutator,
        sinks_on: [GateResolutionCompleted],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        decide_maintain(trigger) == MaintainDecision::Proceed
    }

    dry_run_via_simulation!();

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project,
            throttle,
            payload,
        } = TriggerContext::from_trigger(trigger);

        let p = parse_payload!(trigger, GateResolutionCompletedPayload);
        let gates = p.gates;

        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let prompt = build_maintain_prompt(&project, Some(&gates).filter(|v| !v.is_null()));
            let ctx = ExecutionContext {
                project: &project,
                workflow: WorkflowType::Maintain,
                payload: &payload,
                throttle,
                label: "maintenance",
                retry_count: None,
                // Maintain is never the iterate workflow, so correction_needed is
                // irrelevant — the clean-tree override only fires for WorkflowType::Iterate.
                correction_needed: true,
            };

            Ok(super::execute_agent_block(&*agent, &*shell, &entry, &ctx, prompt).await)
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

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::Registry;
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{AgentAccess, ModelTier, ReasoningEffort};

    use super::super::test_helpers;
    use super::{ExecuteMaintain, MaintainDecision, decide_maintain};

    assert_block_meta!(
        ExecuteMaintain::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry { version: 2, projects: vec![] })),
        ),
        kind: Mutator,
        sinks_on: [GateResolutionCompleted],
    );

    #[test]
    fn accepts_returns_false_for_iterate_workflow() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            "/tmp/test",
            "rust-craftsperson",
        ));
        let block = ExecuteMaintain::new(agent, registry);
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "iterate",
            "gates": [],
        });

        assert!(!block.accepts(&trigger), "block should not accept iterate workflow events");
    }

    #[test]
    fn accepts_returns_true_for_maintain_workflow() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            "/tmp/test",
            "rust-craftsperson",
        ));
        let block = ExecuteMaintain::new(agent, registry);
        let trigger = test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": "maintain",
            "gates": [],
        });

        assert!(block.accepts(&trigger), "block should accept maintain workflow events");
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
        assert_eq!(invocations[0].tier, ModelTier::Balanced);
        assert_eq!(invocations[0].effort, ReasoningEffort::Medium);
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
            "gates": [{"name": "fmt", "command": "cargo fmt --check", "required": true}],
        });
        test_helpers::assert_agent_failure_emits_failure(&block, &trigger).await;
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
        test_helpers::assert_forwards_actions(&block, &trigger).await;
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
        test_helpers::assert_detects_changes_when_dirty(
            &block,
            &trigger,
            &["Cargo.lock", "new-patch.txt"],
        )
        .await;
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
            "gates": [{"name": "fmt", "command": "cargo fmt --check", "required": true}],
        });
        test_helpers::assert_reports_no_changes_when_clean(&block, &trigger, true).await;
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
            "gates": [{"name": "fmt", "command": "cargo fmt --check", "required": true}],
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
            "gates": [{"name": "fmt", "command": "cargo fmt --check", "required": true}],
        });
        test_helpers::assert_tolerates_git_failure(&block, &trigger, true).await;
    }

    // --- decide_maintain pure function tests ---

    #[test]
    fn decide_maintain_skips_iterate_workflow() {
        let trigger = test_event!(EventType::GateResolutionCompleted, "proj", {
            "workflow": "iterate",
            "gates": [],
        });
        assert_eq!(decide_maintain(&trigger), MaintainDecision::SkipNonMaintain);
    }

    #[test]
    fn decide_maintain_proceeds_for_maintain_workflow() {
        let trigger = test_event!(EventType::GateResolutionCompleted, "proj", {
            "workflow": "maintain",
            "gates": [],
        });
        assert_eq!(decide_maintain(&trigger), MaintainDecision::Proceed);
    }

    #[test]
    fn decide_maintain_skips_unknown_workflow() {
        let trigger = test_event!(EventType::GateResolutionCompleted, "proj", {
            "workflow": "unknown",
            "gates": [],
        });
        assert_eq!(decide_maintain(&trigger), MaintainDecision::SkipNonMaintain);
    }

    #[test]
    fn dry_run_and_execute_agree_on_skip_for_non_maintain() {
        // Both dry_run_events and execute must skip when workflow is not maintain.
        let block = ExecuteMaintain::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        let trigger = test_event!(EventType::GateResolutionCompleted, "proj", {
            "workflow": "iterate",
            "gates": [],
        });
        assert!(
            block.dry_run_events(&trigger).is_empty(),
            "dry_run must skip non-maintain workflows"
        );
        assert!(!block.accepts(&trigger), "accepts() must reject non-maintain workflows");
    }
}
