use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{ExecutionCompletedPayload, LoopContext, RetryRequestedPayload};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::workflow::WorkflowType;

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

use super::TriggerContext;

/// Retries the execution phase with context about which gates failed.
///
/// Mutator — sinks on `RetryRequested`.
/// Uses `AgentGateway` with `Coding` capability and `Full` access.
/// Emits `ExecutionCompleted` which feeds back into `RunVerifyGates` -> `RouteGateResult`.
pub struct RetryExecution {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl RetryExecution {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, agent }
    }
}

impl TaskBlock for RetryExecution {
    task_block_meta! {
        name: "Retry Execution",
        kind: Mutator,
        sinks_on: [RetryRequested],
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        let p = trigger
            .parse_payload::<RetryRequestedPayload>()
            .expect("dry_run_events called with invalid RetryRequested payload");
        let workflow = WorkflowType::from_payload(&trigger.payload);
        let context = LoopContext::extract_from(&trigger.payload);

        let payload = Event::serialize_payload(&ExecutionCompletedPayload {
            project: trigger.project.clone(),
            workflow: workflow.to_string(),
            success: true,
            summary: String::new(),
            execution_output: None,
            dry_run: Some(true),
            retry_count: Some(p.retry_count),
            context,
        })
        .expect("ExecutionCompletedPayload is infallibly serializable");
        vec![Event::new(
            EventType::ExecutionCompleted,
            trigger.project.clone(),
            trigger.throttle,
            payload,
        )]
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

        let p = match trigger.parse_payload::<RetryRequestedPayload>() {
            Ok(p) => p,
            Err(e) => return Box::pin(async move { Err(e) }),
        };

        let retry_count = p.retry_count;
        let failure_context = p.failure_context.clone();
        let prior_output = p.prior_execution_output.unwrap_or_default();

        let entry = match super::require_project(&self.registry, &project) {
            Ok(e) => e,
            Err(result) => return Box::pin(async { Ok(result) }),
        };
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);

            let prompt = build_retry_prompt(
                &project,
                workflow,
                retry_count,
                &failure_context,
                &prior_output,
            );

            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            let request = AgentRequest {
                prompt,
                working_dir: project_path,
                access: AgentAccess::Full,
                capability: AgentCapability::Coding,
                agent_file,
                timeout: entry.timeout(),
            };

            tracing::info!(
                project = %project,
                retry_count = retry_count,
                "retrying execution via agent"
            );

            let response = agent.invoke(&request).await;

            Ok(build_retry_result(
                &project,
                workflow,
                retry_count,
                response,
                &payload,
                throttle,
            ))
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

fn build_retry_result(
    project: &str,
    workflow: WorkflowType,
    retry_count: u64,
    response: anyhow::Result<crate::gateway::AgentResponse>,
    payload: &serde_json::Value,
    throttle: foundry_core::throttle::Throttle,
) -> TaskBlockResult {
    let (raw_output, exit_code, success, summary, execution_output) = match response {
        Ok(r) => {
            let s = r.success;
            let out = format!("{}\n{}", r.stdout, r.stderr).trim().to_string();
            let summary = if s {
                format!("retry {retry_count} completed")
            } else {
                let first_line = r.stderr.lines().next().unwrap_or("agent failed");
                format!("retry {retry_count} failed: {first_line}")
            };
            let lines: Vec<&str> = out.lines().collect();
            let start = lines.len().saturating_sub(200);
            let trimmed_output = lines[start..].join("\n");
            let exec_out = if trimmed_output.is_empty() {
                None
            } else {
                Some(trimmed_output)
            };
            (Some(out), Some(r.exit_code), s, summary, exec_out)
        }
        Err(err) => {
            tracing::warn!(error = %err, "agent invocation failed during retry");
            (None, None, false, format!("agent unavailable during retry: {err}"), None)
        }
    };

    tracing::info!(
        project = %project,
        retry_count = retry_count,
        success = success,
        "retry execution completed"
    );

    let context = LoopContext::extract_from(payload);
    let event_payload = Event::serialize_payload(&ExecutionCompletedPayload {
        project: project.to_string(),
        workflow: workflow.to_string(),
        success,
        summary: summary.clone(),
        execution_output,
        dry_run: None,
        retry_count: Some(retry_count),
        context,
    })
    .expect("ExecutionCompletedPayload is infallibly serializable");

    TaskBlockResult {
        events: vec![Event::new(
            EventType::ExecutionCompleted,
            project.to_string(),
            throttle,
            event_payload,
        )],
        success,
        summary: format!("{project}: {summary}"),
        raw_output,
        exit_code,
        audit_artifacts: vec![],
    }
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
    use super::RetryExecution;

    fn retry_event(project: &str, retry_count: u64, workflow: &str) -> Event {
        Event::new(
            EventType::RetryRequested,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "workflow": workflow,
                "retry_count": retry_count,
                "failure_context": "fmt: formatting error\ntest: 2 tests failed",
            }),
        )
    }

    #[test]
    fn kind_is_mutator() {
        let agent = FakeAgentGateway::success();
        let block = RetryExecution::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.kind(), BlockKind::Mutator);
    }

    #[test]
    fn sinks_on_retry_requested() {
        let agent = FakeAgentGateway::success();
        let block = RetryExecution::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.sinks_on(), &[EventType::RetryRequested]);
    }

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
        assert_eq!(invocations[0].capability, AgentCapability::Coding);
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
        let agent = FakeAgentGateway::success();
        let block = RetryExecution::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        let trigger = retry_event("unknown", 1, "maintain");

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

        let result = block.execute(&trigger).await.unwrap();

        let actions = result.events[0].payload.get("actions").unwrap();
        assert_eq!(actions["maintain"], true);
    }
}
