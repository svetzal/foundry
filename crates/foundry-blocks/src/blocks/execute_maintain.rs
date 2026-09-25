use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{ClassificationPhase, DependencyUpdatesClassifiedPayload};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock};
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::{AgentGateway, ProcessShellGateway, ShellGateway};

use super::{ExecutionContext, SimulatedSuccess, TriggerContext};

agent_execution_block! {
    /// Executes the maintain workflow: applies the dependency brief, fixes
    /// vulnerabilities, and resolves quality gate failures.
    ///
    /// Mutator — sinks on `DependencyUpdatesClassified` (phase `before`,
    /// workflow = "maintain" only). The prompt lists exactly which dependency
    /// moves to apply; the agent decides none of them.
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
///
/// Only the `before` classification of a maintain run starts the agent; the
/// `after` and `review` classifications are records, not requests.
fn decide_maintain(trigger: &Event) -> MaintainDecision {
    let before = trigger.payload.get("phase").and_then(serde_json::Value::as_str) == Some("before");
    if before && WorkflowType::from_payload(&trigger.payload) == WorkflowType::Maintain {
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
        sinks_on: [DependencyUpdatesClassified],
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
            trace_id,
        } = TriggerContext::from_trigger(trigger);

        let p = parse_payload!(trigger, DependencyUpdatesClassifiedPayload);
        debug_assert_eq!(p.phase, ClassificationPhase::Before);
        let gates = p.chain.gates.clone().unwrap_or(serde_json::Value::Null);
        let dependency_brief =
            crate::dependency_updates::brief::render(&p.brief, &p.classification);

        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let prompt = build_maintain_prompt(
                &project,
                &dependency_brief,
                Some(&gates).filter(|v| !v.is_null()),
            );
            let ctx = ExecutionContext {
                trace_id: trace_id.clone(),
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

/// The maintain prompt: the dependency brief, decided in code, plus the gates.
///
/// The agent is told what to apply rather than asked what is compatible, so
/// the same policy holds on every project and every night.
fn build_maintain_prompt(
    project: &str,
    dependency_brief: &str,
    gates: Option<&serde_json::Value>,
) -> String {
    let gates_context = super::format_gates_context(gates);
    format!(
        "You are maintaining the project '{project}'. Apply the dependency updates \
         listed below and resolve any quality gate failures. Make only the changes \
         needed for that.\n\n\
         {dependency_brief}\n\
         Vulnerabilities: a security fix appears in the list above with its advisory. \
         If a vulnerability needs a dependency change that is not listed, report it \
         in your final message instead of making the change.\n\n\
         {rules}\n\
         Foundry checks this run for new suppressions; a run that adds one fails and \
         needs review.{gates_context}",
        rules = super::suppression_guard::ADVISORY_RULES,
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
        sinks_on: [DependencyUpdatesClassified],
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "iterate",
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "unknown-project", {
            "project": "unknown-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
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
            EventType::DependencyUpdatesClassified,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "iterate",
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
        // Shell sequence: rev-parse HEAD → sha; branch guard's
        // rev-parse --abbrev-ref HEAD → main; git diff --name-only <sha> → files
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: "abc123\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: "main\n".to_string(),
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
            "gates": [{"name": "fmt", "command": "cargo fmt --check", "required": true}],
        });
        test_helpers::assert_tolerates_git_failure(&block, &trigger, true).await;
    }

    // --- branch drift after the agent ---

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(dir: &std::path::Path, file: &str, message: &str) {
        std::fs::write(dir.join(file), message).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", message]);
    }

    /// A repo whose checkout the "agent" left on its own branch.
    fn repo_left_on_agent_branch(main_moves: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "t@example.com"]);
        git(dir.path(), &["config", "user.name", "T"]);
        commit(dir.path(), "README.md", "init");
        if main_moves {
            git(dir.path(), &["checkout", "-q", "-b", "chore/dependency-update"]);
            commit(dir.path(), "mix.lock", "agent");
            git(dir.path(), &["checkout", "-q", "main"]);
            commit(dir.path(), "other.txt", "main moved");
            git(dir.path(), &["checkout", "-q", "chore/dependency-update"]);
        } else {
            git(dir.path(), &["checkout", "-q", "-b", "chore/dependency-update"]);
            commit(dir.path(), "mix.lock", "agent");
        }
        dir
    }

    fn maintain_trigger() -> Event {
        test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
            "gates": [{"name": "fmt", "command": "true", "required": true}],
        })
    }

    #[tokio::test]
    async fn agent_branch_is_fast_forwarded_back_onto_the_configured_branch() {
        let dir = repo_left_on_agent_branch(false);
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "elixir-craftsperson",
        ));
        let block = ExecuteMaintain::new(FakeAgentGateway::success(), registry);

        let result = block.execute(&maintain_trigger()).await.unwrap();

        assert!(result.success, "{}", result.summary);
        assert_eq!(git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
        assert_eq!(git(dir.path(), &["branch", "--format=%(refname:short)"]), "main");
    }

    #[tokio::test]
    async fn agent_branch_that_cannot_fast_forward_fails_the_run() {
        let dir = repo_left_on_agent_branch(true);
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "elixir-craftsperson",
        ));
        let block = ExecuteMaintain::new(FakeAgentGateway::success(), registry);

        let result = block.execute(&maintain_trigger()).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events[0].payload["success"], false);
        assert!(
            result.summary.contains("chore/dependency-update")
                && result.summary.contains("does not fast-forward main"),
            "{}",
            result.summary
        );
    }

    // --- decide_maintain pure function tests ---

    #[test]
    fn decide_maintain_skips_iterate_workflow() {
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "proj", {
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "iterate",
            "gates": [],
        });
        assert_eq!(decide_maintain(&trigger), MaintainDecision::SkipNonMaintain);
    }

    #[test]
    fn decide_maintain_proceeds_for_maintain_workflow() {
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "proj", {
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "maintain",
            "gates": [],
        });
        assert_eq!(decide_maintain(&trigger), MaintainDecision::Proceed);
    }

    #[test]
    fn decide_maintain_skips_unknown_workflow() {
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "proj", {
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "unknown",
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
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "proj", {
            "phase": "before", "classification": {}, "brief": {"policy": "minor", "policy_set": false}, "workflow": "iterate",
            "gates": [],
        });
        assert!(
            block.dry_run_events(&trigger).is_empty(),
            "dry_run must skip non-maintain workflows"
        );
        assert!(!block.accepts(&trigger), "accepts() must reject non-maintain workflows");
    }

    // --- the dependency brief ---

    #[test]
    fn accepts_returns_false_for_the_after_and_review_phases() {
        let block =
            ExecuteMaintain::new(FakeAgentGateway::success(), test_helpers::empty_registry());
        for phase in ["after", "review"] {
            let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
                "project": "my-project",
                "phase": phase,
                "workflow": "maintain",
                "classification": {},
                "brief": {"policy": "minor", "policy_set": false},
            });
            assert!(!block.accepts(&trigger), "{phase}");
        }
    }

    #[tokio::test]
    async fn the_prompt_lists_exactly_the_briefed_updates() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "elixir-craftsperson",
        ));
        let block = ExecuteMaintain::new(agent.clone(), registry);
        let trigger = test_event!(EventType::DependencyUpdatesClassified, "my-project", {
            "project": "my-project",
            "phase": "before",
            "workflow": "maintain",
            "gates": [{"name": "test", "command": "mix test", "required": true}],
            "classification": {},
            "brief": {
                "policy": "patch",
                "policy_set": true,
                "apply": [{
                    "ecosystem": "hex", "manifest": "apps/bedrock", "package": "phoenix",
                    "from": "1.8.1", "to": "1.8.3", "class": "patch", "change": "lockfile"
                }],
                "held_by_policy": [{
                    "ecosystem": "hex", "manifest": "apps/bedrock", "package": "phoenix",
                    "from": "1.8.1", "to": "1.9.0", "class": "minor",
                    "reason": "policy is patch: this needs a constraint change"
                }],
                "majors": [{
                    "ecosystem": "hex", "manifest": "apps/bedrock", "package": "req",
                    "from": "0.7.4", "to": "0.8.0", "class": "major", "change": "manifest"
                }]
            },
        });

        block.execute(&trigger).await.unwrap();

        let prompt = &agent.invocations()[0].prompt;
        assert!(
            prompt.contains("Dependency update policy: patch (set for this project)."),
            "{prompt}"
        );
        assert!(
            prompt.contains("- [hex apps/bedrock] phoenix 1.8.1 -> 1.8.3 (patch, lockfile only)")
        );
        assert!(prompt.contains("and no others"));
        assert!(prompt.contains("Held back (do not apply)"));
        assert!(prompt.contains("req 0.7.4 -> 0.8.0"));
        assert!(prompt.contains("Never take a major upgrade"));
        assert!(
            !prompt.contains("latest compatible versions"),
            "the agent no longer decides compatibility"
        );
        assert!(prompt.contains("mix test"), "gates still reach the agent");
        assert!(
            prompt.contains(
                "Never suppress, ignore or allowlist an advisory that has a fixed release"
            )
        );
        assert!(prompt.contains("Never edit .supply-chain-allow.json"));
        assert!(prompt.contains("unless you cite"));
    }
}
