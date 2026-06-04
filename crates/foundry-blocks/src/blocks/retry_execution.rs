use std::pin::Pin;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::RetryRequestedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::{AgentGateway, ProcessShellGateway, ShellGateway};

use super::{ExecutionContext, TriggerContext};

agent_execution_block! {
    /// Retries the execution phase with context about which gates failed.
    ///
    /// Mutator — sinks on `RetryRequested`.
    /// Uses `AgentGateway` with `Coding` capability and `Full` access.
    /// Emits `ExecutionCompleted` which feeds back into `RunVerifyGates` -> `RouteGateResult`.
    pub struct RetryExecution
}

impl TaskBlock for RetryExecution {
    task_block_meta! {
        name: "Retry Execution",
        kind: Mutator,
        sinks_on: [RetryRequested],
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        let Some(p) = trigger.parse_payload::<RetryRequestedPayload>().ok() else {
            return vec![];
        };
        let workflow = WorkflowType::from_payload(&trigger.payload);
        super::dry_run_execution_event(trigger, workflow, Some(p.retry_count))
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

        let workflow = WorkflowType::from_payload(&payload);

        let p = parse_payload!(trigger, RetryRequestedPayload);

        let retry_count = p.retry_count;
        let failure_context = p.failure_context.clone();
        let prior_output = p.prior_execution_output.unwrap_or_default();

        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let prompt = build_retry_prompt(
                &project,
                workflow,
                retry_count,
                &failure_context,
                &prior_output,
            );
            let label = format!("retry {retry_count}");
            let ctx = ExecutionContext {
                project: &project,
                workflow,
                payload: &payload,
                throttle,
                label: &label,
                retry_count: Some(retry_count),
                // Retry only fires when the initial execution was detected as a silent
                // no-op (or a genuine failure), so correction is always required here.
                correction_needed: true,
            };

            Ok(super::execute_agent_block(&*agent, &*shell, &entry, &ctx, prompt).await)
        })
    }
}

fn build_retry_prompt(
    project: &str,
    workflow: WorkflowType,
    retry_count: u64,
    failure_context: &str,
    prior_output: &str,
) -> String {
    let prior_work_section = if prior_output.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nHere is the output from the previous attempt:\n\
             {prior_output}\n\n\
             Analyze what was tried and avoid repeating the same approach if it failed."
        )
    };
    format!(
        "You are retrying a {workflow} operation on project '{project}' \
         (attempt {retry_count} of 3).\n\n\
         The previous attempt failed because the following quality gates did not pass:\n\
         {failure_context}{prior_work_section}\n\n\
         Please fix the issues that caused these gate failures. \
         Focus specifically on the failures listed above. \
         Make only the changes necessary to resolve these issues."
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
    use super::RetryExecution;

    fn retry_event(project: &str, retry_count: u64, workflow: &str) -> Event {
        test_event!(EventType::RetryRequested, project, {
            "project": project,
            "workflow": workflow,
            "retry_count": retry_count,
            "failure_context": "fmt: formatting error\ntest: 2 tests failed",
        })
    }

    assert_block_meta!(
        RetryExecution::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry { version: 2, projects: vec![] })),
        ),
        kind: Mutator,
        sinks_on: [RetryRequested],
    );

    #[tokio::test]
    async fn emits_execution_completed_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Fixed formatting");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RetryExecution::new(agent.clone(), registry);
        let trigger = retry_event("my-project", 1, "maintain");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(result.events[0].payload["workflow"], "maintain");
        assert_eq!(result.events[0].payload["retry_count"], 1);
        assert_eq!(result.events[0].payload["success"], true);
    }

    #[tokio::test]
    async fn includes_failure_context_in_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RetryExecution::new(agent.clone(), registry);
        let trigger = retry_event("my-project", 2, "maintain");

        block.execute(&trigger).await.unwrap();

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert!(invocations[0].prompt.contains("formatting error"));
        assert!(invocations[0].prompt.contains("2 tests failed"));
        assert!(invocations[0].prompt.contains("attempt 2 of 3"));
        assert_eq!(invocations[0].access, AgentAccess::Full);
        assert_eq!(invocations[0].tier, ModelTier::Balanced);
        assert_eq!(invocations[0].effort, ReasoningEffort::Medium);
    }

    #[tokio::test]
    async fn emits_execution_completed_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::failure("still broken");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RetryExecution::new(agent, registry);
        let trigger = retry_event("my-project", 1, "iterate");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(result.events[0].payload["success"], false);
        assert_eq!(result.events[0].payload["workflow"], "iterate");
    }

    #[tokio::test]
    async fn project_not_in_registry_returns_failure() {
        let block = RetryExecution::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        let trigger = retry_event("unknown-project", 1, "maintain");
        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn includes_prior_execution_output_in_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RetryExecution::new(agent.clone(), registry);
        let trigger = Event::new(
            EventType::RetryRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "maintain",
                "retry_count": 2,
                "failure_context": "fmt failed",
                "prior_execution_output": "tried updating deps\ncargo fmt failed on lib.rs",
            }),
        );

        block.execute(&trigger).await.unwrap();

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert!(
            invocations[0].prompt.contains("tried updating deps"),
            "prompt should include prior execution output",
        );
        assert!(
            invocations[0].prompt.contains("Analyze what was tried"),
            "prompt should include guidance about prior attempt",
        );
    }

    #[tokio::test]
    async fn emitted_event_includes_execution_output() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Fixed the issue");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RetryExecution::new(agent, registry);
        let trigger = retry_event("my-project", 1, "maintain");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let exec_output = result.events[0].payload.get("execution_output").and_then(|v| v.as_str());
        assert!(
            exec_output.is_some(),
            "ExecutionCompleted should include execution_output in payload",
        );
        assert!(
            exec_output.unwrap().contains("Fixed the issue"),
            "execution_output should contain agent stdout",
        );
    }

    #[tokio::test]
    async fn detects_changes_when_tree_dirty() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Fixed formatting");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        // Shell sequence: rev-parse HEAD → sha; git diff --name-only <sha> → files
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: "abc123\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: "src/main.rs\nfix.patch\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = RetryExecution::with_gateways(agent, registry, shell);
        let trigger = retry_event("my-project", 1, "maintain");
        test_helpers::assert_detects_changes_when_dirty(
            &block,
            &trigger,
            &["src/main.rs", "fix.patch"],
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
        let block = RetryExecution::with_gateways(agent, registry, shell);
        let trigger = retry_event("my-project", 1, "maintain");
        test_helpers::assert_reports_no_changes_when_clean(&block, &trigger, true).await;
    }

    #[tokio::test]
    async fn tolerates_git_status_failure() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Fixed issue");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::failure("fatal: not a git repository");
        let block = RetryExecution::with_gateways(agent, registry, shell);
        // Use maintain so the iterate override doesn't fire (we're testing git failure tolerance)
        let trigger = retry_event("my-project", 1, "maintain");
        test_helpers::assert_tolerates_git_failure(&block, &trigger, true).await;
    }

    #[tokio::test]
    async fn iterate_retry_clean_tree_overrides_to_failure() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success(); // empty stdout → no changes
        let block = RetryExecution::with_gateways(agent, registry, shell);
        let trigger = retry_event("my-project", 1, "iterate");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success, "iterate retry + clean tree must override to failure");
        assert!(
            result.summary.contains("silent no-op"),
            "expected 'silent no-op' in summary, got: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn maintain_retry_clean_tree_remains_success() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success(); // empty stdout → no changes
        let block = RetryExecution::with_gateways(agent, registry, shell);
        let trigger = retry_event("my-project", 1, "maintain");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success, "maintain retry must NOT override to failure on clean tree");
        assert_eq!(result.events[0].payload["success"], true);
    }

    #[tokio::test]
    async fn forwards_actions_from_payload() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RetryExecution::new(agent, registry);
        let trigger = Event::new(
            EventType::RetryRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "workflow": "maintain",
                "retry_count": 1,
                "failure_context": "fmt failed",
                "actions": {"maintain": true},
            }),
        );
        test_helpers::assert_forwards_actions(&block, &trigger).await;
    }
}
