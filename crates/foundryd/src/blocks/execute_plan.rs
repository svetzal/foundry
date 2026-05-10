use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::PlanCompletedPayload;
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::workflow::WorkflowType;

use crate::gateway::{
    AgentAccess, AgentCapability, AgentGateway, ProcessShellGateway, ShellGateway,
};

use super::{AgentBlockSpec, TriggerContext, invoke_agent};

/// Applies the correction plan to the project.
///
/// Mutator — sinks on `PlanCompleted`.
/// Uses `AgentGateway` with `Coding` capability and `Full` access.
/// Emits `ExecutionCompleted` with success status and `changes_detected` flag.
pub struct ExecutePlan {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
    shell: Arc<dyn ShellGateway>,
}

impl ExecutePlan {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self {
            registry,
            agent,
            shell: Arc::new(ProcessShellGateway),
        }
    }

    #[cfg(test)]
    fn with_gateways(
        agent: Arc<dyn AgentGateway>,
        registry: Arc<Registry>,
        shell: Arc<dyn ShellGateway>,
    ) -> Self {
        Self {
            registry,
            agent,
            shell,
        }
    }
}

impl TaskBlock for ExecutePlan {
    task_block_meta! {
        name: "Execute Plan",
        kind: Mutator,
        sinks_on: [PlanCompleted],
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        super::dry_run_execution_event(trigger, WorkflowType::Iterate, None)
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

        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);
        let shell = Arc::clone(&self.shell);

        let plan_payload = parse_payload!(trigger, PlanCompletedPayload);

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);

            let plan = &plan_payload.plan;
            let principle = &plan_payload.principle;
            let gates = plan_payload.chain.gates.as_ref();

            let prompt = build_execution_prompt(&project, plan, principle, gates);

            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            let outcome = invoke_agent(
                &*agent,
                AgentBlockSpec {
                    prompt,
                    working_dir: project_path.clone(),
                    access: AgentAccess::Full,
                    capability: AgentCapability::Coding,
                    agent_file,
                    timeout: entry.timeout(),
                },
                "plan execution",
                &project,
            )
            .await;

            let (changes_detected, files_changed) =
                super::detect_post_execution_changes(&*shell, &project_path).await;

            Ok(super::build_agent_execution_result(
                &project,
                WorkflowType::Iterate,
                outcome,
                &payload,
                throttle,
                "plan execution",
                None,
                changes_detected,
                files_changed,
            ))
        })
    }
}

fn build_execution_prompt(
    project: &str,
    plan: &str,
    principle: &str,
    gates: Option<&serde_json::Value>,
) -> String {
    let gates_context = super::format_gates_context(gates);
    format!(
        "You are executing a correction plan for project '{project}'.\n\n\
         Principle being addressed: {principle}\n\n\
         Plan:\n{plan}\n\n\
         REQUIREMENTS:\n\
         - The plan above describes mutations that MUST be applied to the source files. Apply them now.\n\
         - Do NOT skip the plan because quality gates currently pass. Passing gates is necessary, not sufficient — the purpose of this run is to apply the plan, not to re-verify a clean tree.\n\
         - After applying the plan, the working tree MUST contain modifications to the files named or implied by the plan. If `git status --porcelain` would be empty when you finish, you have not done the job and the run has failed.\n\
         - Make only the changes the plan describes; do not expand scope.\n\
         - Once the plan is applied, the following quality gates must still pass:{gates_context}"
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::Registry;
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{AgentAccess, AgentCapability};

    use super::super::test_helpers;
    use super::ExecutePlan;

    fn plan_completed_event(project: &str) -> Event {
        Event::new(
            EventType::PlanCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "plan": "1. Extract helper\n2. Update callers",
                "principle": "DRY",
                "category": "duplication",
                "workflow": "iterate",
            }),
        )
    }

    #[test]
    fn kind_is_mutator() {
        let agent = FakeAgentGateway::success();
        let block = ExecutePlan::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.kind(), BlockKind::Mutator);
    }

    #[test]
    fn sinks_on_plan_completed() {
        let agent = FakeAgentGateway::success();
        let block = ExecutePlan::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.sinks_on(), &[EventType::PlanCompleted]);
    }

    #[tokio::test]
    async fn executes_plan_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Changes applied successfully");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ExecutePlan::new(agent.clone(), registry);
        let trigger = plan_completed_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(result.events[0].payload["workflow"], "iterate");
        assert_eq!(result.events[0].payload["success"], true);

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].access, AgentAccess::Full);
        assert_eq!(invocations[0].capability, AgentCapability::Coding);
        assert!(invocations[0].prompt.contains("DRY"));
    }

    #[tokio::test]
    async fn agent_failure_emits_execution_completed_with_failure() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::failure("compilation error");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ExecutePlan::new(agent, registry);
        let trigger = plan_completed_event("my-project");

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
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ExecutePlan::new(agent, registry);
        let trigger = Event::new(
            EventType::PlanCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "plan": "1. Do things",
                "principle": "SRP",
                "workflow": "iterate",
                "actions": {"maintain": true},
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        let actions = result.events[0].payload.get("actions").unwrap();
        assert_eq!(actions["maintain"], true);
    }

    #[test]
    fn dry_run_emits_simulated_success() {
        let agent = FakeAgentGateway::success();
        let block = ExecutePlan::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        let trigger = plan_completed_event("my-project");

        let events = block.dry_run_events(&trigger);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(events[0].payload["dry_run"], true);
        assert_eq!(events[0].payload["success"], true);
        assert_eq!(events[0].payload["workflow"], "iterate");
    }

    #[tokio::test]
    async fn project_not_in_registry_returns_failure() {
        let agent = FakeAgentGateway::success();
        let block = ExecutePlan::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        let trigger = plan_completed_event("unknown");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn detects_changes_when_tree_dirty() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Changes applied");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::always(CommandResult {
            stdout: " M src/lib.rs\n?? new.txt\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let block = ExecutePlan::with_gateways(agent, registry, shell);
        let trigger = plan_completed_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["changes_detected"], true);
        let files = result.events[0].payload["files_changed"].as_array().unwrap();
        assert!(files.iter().any(|f| f == "src/lib.rs"), "expected src/lib.rs in {files:?}");
        assert!(files.iter().any(|f| f == "new.txt"), "expected new.txt in {files:?}");
    }

    #[tokio::test]
    async fn reports_no_changes_when_tree_clean() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success(); // empty stdout
        let block = ExecutePlan::with_gateways(agent, registry, shell);
        let trigger = plan_completed_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["changes_detected"], false);
        // files_changed is skip_serializing_if Vec::is_empty, so absent when empty
        assert!(
            result.events[0]
                .payload
                .get("files_changed")
                .is_none_or(|v| v.as_array().is_none_or(std::vec::Vec::is_empty))
        );
    }

    #[tokio::test]
    async fn tolerates_git_status_failure() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Changes applied");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::failure("fatal: not a git repository");
        let block = ExecutePlan::with_gateways(agent, registry, shell);
        let trigger = plan_completed_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        // No error propagated; event still emitted with success from agent
        assert!(result.success);
        assert_eq!(result.events[0].payload["changes_detected"], false);
    }

    #[test]
    fn prompt_contains_imperative_requirements() {
        let gates = serde_json::json!({"cargo test": "pass"});
        let prompt = super::build_execution_prompt(
            "my-project",
            "1. Extract helper\n2. Update callers",
            "DRY",
            Some(&gates),
        );

        // Imperative: plan application is mandatory
        assert!(
            prompt.contains("MUST be applied"),
            "expected imperative phrase 'MUST be applied' in:\n{prompt}"
        );
        // Invalidation of "gates already pass" as a stopping condition
        assert!(
            prompt.contains("necessary, not sufficient"),
            "expected invalidation phrase 'necessary, not sufficient' in:\n{prompt}"
        );
        // Empty working tree means failure
        assert!(
            prompt.contains("git status --porcelain"),
            "expected 'git status --porcelain' in:\n{prompt}"
        );
        assert!(
            prompt.contains("empty"),
            "expected 'empty' (empty-tree-means-failure) in:\n{prompt}"
        );
        // Core content: principle and plan still present
        assert!(prompt.contains("DRY"), "expected principle 'DRY' in:\n{prompt}");
        assert!(prompt.contains("1. Extract helper"), "expected plan content in:\n{prompt}");
        // Gates section present when gates are provided
        assert!(
            prompt.contains("quality gates"),
            "expected gates section when gates are provided, in:\n{prompt}"
        );
    }

    #[test]
    fn prompt_without_gates_still_contains_requirements() {
        let prompt = super::build_execution_prompt("other-project", "1. Do the thing", "SRP", None);

        assert!(prompt.contains("MUST be applied"), "expected 'MUST be applied' in:\n{prompt}");
        assert!(
            prompt.contains("necessary, not sufficient"),
            "expected 'necessary, not sufficient' in:\n{prompt}"
        );
        assert!(
            prompt.contains("git status --porcelain"),
            "expected 'git status --porcelain' in:\n{prompt}"
        );
        assert!(prompt.contains("SRP"), "expected principle 'SRP' in:\n{prompt}");
        assert!(prompt.contains("1. Do the thing"), "expected plan content in:\n{prompt}");
    }
}
