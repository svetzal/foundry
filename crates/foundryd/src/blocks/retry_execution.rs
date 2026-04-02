use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::loop_context::forward_loop_context;
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

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
        let workflow = trigger
            .payload
            .get("workflow")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let retry_count = trigger
            .payload
            .get("retry_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);

        let mut payload = serde_json::json!({
            "project": trigger.project,
            "workflow": workflow,
            "retry_count": retry_count,
            "success": true,
            "dry_run": true,
        });
        forward_loop_context(&trigger.payload, &mut payload);
        vec![Event::new(
            EventType::ExecutionCompleted,
            trigger.project.clone(),
            trigger.throttle,
            payload,
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

        let workflow = trigger.payload_str_or("workflow", "unknown").to_string();

        let retry_count = trigger.payload_u64_or("retry_count", 1);

        let failure_context = trigger
            .payload_str_or("failure_context", "no failure context available")
            .to_string();

        let entry = self.registry.find_project(&project).cloned();
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            let Some(entry) = entry else {
                return Ok(super::project_not_found_result(&project));
            };

            let project_path = PathBuf::from(&entry.path);

            let prompt = format!(
                "You are retrying a {workflow} operation on project '{project}' \
                 (attempt {retry_count} of 3).\n\n\
                 The previous attempt failed because the following quality gates did not pass:\n\
                 {failure_context}\n\n\
                 Please fix the issues that caused these gate failures. \
                 Focus specifically on the failures listed above. \
                 Make only the changes necessary to resolve these issues."
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

            let (raw_output, exit_code, success, summary) = match response {
                Ok(r) => {
                    let s = r.success;
                    let out = format!("{}\n{}", r.stdout, r.stderr).trim().to_string();
                    let summary = if s {
                        format!("retry {retry_count} completed")
                    } else {
                        let first_line = r.stderr.lines().next().unwrap_or("agent failed");
                        format!("retry {retry_count} failed: {first_line}")
                    };
                    (Some(out), Some(r.exit_code), s, summary)
                }
                Err(err) => {
                    tracing::warn!(error = %err, "agent invocation failed during retry");
                    (None, None, false, format!("agent unavailable during retry: {err}"))
                }
            };

            tracing::info!(
                project = %project,
                retry_count = retry_count,
                success = success,
                "retry execution completed"
            );

            let mut event_payload = serde_json::json!({
                "project": project,
                "workflow": workflow,
                "retry_count": retry_count,
                "success": success,
                "summary": summary,
            });
            if let Some(actions) = payload.get("actions") {
                event_payload["actions"] = actions.clone();
            }
            forward_loop_context(&payload, &mut event_payload);

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

    use super::RetryExecution;

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
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
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
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
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
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
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
    async fn forwards_actions_from_payload() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
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
