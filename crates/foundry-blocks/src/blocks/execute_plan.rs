use std::pin::Pin;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{ExecutionCompletedPayload, LoopContext, PlanCompletedPayload};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::{AgentGateway, ProcessShellGateway, ShellGateway};

use super::{ExecutionContext, TriggerContext};

agent_execution_block! {
    /// Applies the correction plan to the project.
    ///
    /// Mutator — sinks on `PlanCompleted`.
    /// Uses `AgentGateway` with `Coding` capability and `Full` access.
    /// Emits `ExecutionCompleted` with success status and `changes_detected` flag.
    pub struct ExecutePlan
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

        if !plan_payload.correction_needed {
            let result = build_no_correction_result(
                &project,
                trigger,
                &payload,
                &plan_payload.correction_reason,
            );
            return Box::pin(async move { result });
        }

        // Read the workflow from the trigger payload so that non-iterate callers
        // (e.g. the prompt workflow) receive the correct WorkflowType.  This is
        // important because the silent no-op override only fires for WorkflowType::Iterate.
        let workflow = WorkflowType::from_payload(&payload);

        Box::pin(async move {
            let plan = &plan_payload.plan;
            let principle = &plan_payload.principle;
            let gates = plan_payload.chain.gates.as_ref();
            let prompt = build_execution_prompt(&project, plan, principle, gates);
            let ctx = ExecutionContext {
                project: &project,
                workflow,
                payload: &payload,
                throttle,
                label: "plan execution",
                retry_count: None,
                correction_needed: plan_payload.correction_needed,
            };

            Ok(super::execute_agent_block(&*agent, &*shell, &entry, &ctx, prompt).await)
        })
    }
}

fn build_no_correction_result(
    project: &str,
    trigger: &Event,
    payload: &serde_json::Value,
    reason: &str,
) -> anyhow::Result<TaskBlockResult> {
    let summary = "no correction needed — codebase satisfies assessed principle".to_string();
    let context = LoopContext::extract_from(payload);
    let workflow = WorkflowType::from_payload(payload);
    let exec_payload = ExecutionCompletedPayload {
        project: project.to_string(),
        workflow: workflow.to_string(),
        success: true,
        summary: summary.clone(),
        execution_output: if reason.is_empty() {
            None
        } else {
            Some(reason.to_string())
        },
        dry_run: None,
        retry_count: None,
        changes_detected: Some(false),
        files_changed: vec![],
        context,
    };
    tracing::info!(
        project = %project,
        "iterate plan concluded no correction needed; short-circuiting before agent invocation"
    );
    let event = trigger.with_payload(EventType::ExecutionCompleted, &exec_payload)?;
    Ok(TaskBlockResult {
        events: vec![event],
        success: true,
        summary: format!("{project}: {summary}"),
        ..Default::default()
    })
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
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::Registry;
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{AgentAccess, ModelTier, ReasoningEffort};

    use super::super::test_helpers;
    use super::ExecutePlan;

    assert_block_meta!(
        ExecutePlan::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry { version: 2, projects: vec![] })),
        ),
        kind: Mutator,
        sinks_on: [PlanCompleted],
    );

    #[tokio::test]
    async fn executes_plan_successfully() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Changes applied successfully");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        // Simulate real file changes so the iterate override does not fire
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "M  src/lib.rs\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let block = ExecutePlan::with_gateways(agent.clone(), registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(result.events[0].payload["workflow"], "iterate");
        assert_eq!(result.events[0].payload["success"], true);

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].access, AgentAccess::Full);
        assert_eq!(invocations[0].tier, ModelTier::Balanced);
        assert_eq!(invocations[0].effort, ReasoningEffort::Medium);
        assert!(invocations[0].prompt.contains("DRY"));
    }

    #[tokio::test]
    async fn agent_failure_emits_execution_completed_with_failure() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::failure("compilation error");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ExecutePlan::new(agent, registry);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });
        test_helpers::assert_agent_failure_emits_failure(&block, &trigger).await;
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
        test_helpers::assert_forwards_actions(&block, &trigger).await;
    }

    #[test]
    fn dry_run_emits_simulated_success() {
        let agent = FakeAgentGateway::success();
        let block = ExecutePlan::new(
            agent,
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });

        let events = block.dry_run_events(&trigger);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(events[0].payload["dry_run"], true);
        assert_eq!(events[0].payload["success"], true);
        assert_eq!(events[0].payload["workflow"], "iterate");
    }

    #[tokio::test]
    async fn project_not_in_registry_returns_failure() {
        let block = ExecutePlan::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        test_helpers::assert_missing_project_fails(&block, EventType::PlanCompleted).await;
    }

    #[tokio::test]
    async fn detects_changes_when_tree_dirty() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Changes applied");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        // Shell sequence:
        // 1. `git rev-parse HEAD` (pre-execution sha capture) → returns sha
        // 2. `git diff --name-only <sha>` (post-execution detection) → returns files
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: "abc123\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: "src/lib.rs\nnew.txt\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = ExecutePlan::with_gateways(agent, registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });
        test_helpers::assert_detects_changes_when_dirty(
            &block,
            &trigger,
            &["src/lib.rs", "new.txt"],
        )
        .await;
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
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });
        // Iterate + clean tree → overridden to failure (silent no-op)
        test_helpers::assert_reports_no_changes_when_clean(&block, &trigger, false).await;
    }

    #[tokio::test]
    async fn iterate_clean_tree_overrides_to_failure() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success(); // empty stdout → no changes
        let block = ExecutePlan::with_gateways(agent, registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
            "correction_needed": true,
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success, "expected failure for iterate + clean tree");
        assert!(
            result.summary.contains("silent no-op"),
            "expected 'silent no-op' in summary, got: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn iterate_clean_tree_no_correction_needed_succeeds() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("THIS SHOULD NEVER RUN");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success();
        let block = ExecutePlan::with_gateways(agent.clone(), registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "No changes needed.",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
            "correction_needed": false,
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(
            result.success,
            "expected success for legitimate no-op (correction_needed=false)"
        );
        assert!(
            result.summary.contains("no correction needed"),
            "expected 'no correction needed' in summary, got: {}",
            result.summary
        );
        assert_eq!(result.events[0].payload["success"], true);
        assert!(
            agent.invocations().is_empty(),
            "agent must not be invoked when correction_needed=false"
        );
    }

    #[tokio::test]
    async fn iterate_no_correction_needed_emits_reason_in_execution_output() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("THIS SHOULD NEVER RUN");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success();
        let block = ExecutePlan::with_gateways(agent.clone(), registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "No changes needed.",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
            "correction_needed": false,
            "correction_reason": "Codebase already extracts shared validation into a helper.",
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(agent.invocations().is_empty());
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(
            result.events[0].payload["execution_output"],
            "Codebase already extracts shared validation into a helper."
        );
        assert_eq!(result.events[0].payload["changes_detected"], false);
        assert_eq!(result.events[0].payload["workflow"], "iterate");
    }

    #[tokio::test]
    async fn iterate_aux_only_changes_treated_as_clean() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        // Shell sequence:
        // 1. `git rev-parse HEAD` → sha
        // 2. `git diff --name-only <sha>` → only a .claude/worktrees/ path
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: "abc123\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: ".claude/worktrees/abc/session.jsonl\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = ExecutePlan::with_gateways(agent, registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success, "all-auxiliary files must be treated as a no-op");
        assert!(result.summary.contains("silent no-op"));
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
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });
        // Git status failure → changes_detected=false → iterate override fires
        test_helpers::assert_tolerates_git_failure(&block, &trigger, false).await;
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
