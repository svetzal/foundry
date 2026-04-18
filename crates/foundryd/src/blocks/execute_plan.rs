use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{ExecutionCompletedPayload, LoopContext, PlanCompletedPayload};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::workflow::WorkflowType;

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

use super::TriggerContext;

/// Applies the correction plan to the project.
///
/// Mutator — sinks on `PlanCompleted`.
/// Uses `AgentGateway` with `Coding` capability and `Full` access.
/// Emits `ExecutionCompleted` with success status.
pub struct ExecutePlan {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl ExecutePlan {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, agent }
    }
}

impl TaskBlock for ExecutePlan {
    task_block_meta! {
        name: "Execute Plan",
        kind: Mutator,
        sinks_on: [PlanCompleted],
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        let context = LoopContext::extract_from(&trigger.payload);
        let payload = Event::serialize_payload(&ExecutionCompletedPayload {
            project: trigger.project.clone(),
            workflow: WorkflowType::Iterate.to_string(),
            success: true,
            summary: String::new(),
            execution_output: None,
            dry_run: Some(true),
            retry_count: None,
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

        let entry = match super::require_project(&self.registry, &project) {
            Ok(e) => e,
            Err(result) => return Box::pin(async { Ok(result) }),
        };
        let agent = Arc::clone(&self.agent);

        let plan_payload = match trigger.parse_payload::<PlanCompletedPayload>() {
            Ok(p) => p,
            Err(e) => return Box::pin(async move { Err(e) }),
        };

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);

            let plan = &plan_payload.plan;
            let principle = &plan_payload.principle;
            let gates = plan_payload.chain.gates.as_ref();

            let prompt = build_execution_prompt(&project, plan, principle, gates);

            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            let request = AgentRequest {
                prompt,
                working_dir: project_path,
                access: AgentAccess::Full,
                capability: AgentCapability::Coding,
                agent_file,
                timeout: entry.timeout(),
            };

            tracing::info!(project = %project, principle = %principle, "executing plan via agent");

            let response = agent.invoke(&request).await;

            Ok(build_execution_result(&project, response, &payload, throttle))
        })
    }
}

fn build_execution_prompt(
    project: &str,
    plan: &str,
    principle: &str,
    gates: Option<&serde_json::Value>,
) -> String {
    let gates_context = if let Some(gates) = gates {
        format!(
            "\n\nThe following quality gates must pass after your changes:\n{}",
            serde_json::to_string_pretty(gates).unwrap_or_default()
        )
    } else {
        String::new()
    };
    format!(
        "You are executing a correction plan for project '{project}'.\n\n\
         Principle being addressed: {principle}\n\n\
         Plan:\n{plan}\n\n\
         Execute this plan. Make only the changes described. \
         Ensure the code compiles and existing tests pass after your changes.{gates_context}"
    )
}

fn build_execution_result(
    project: &str,
    response: anyhow::Result<crate::gateway::AgentResponse>,
    payload: &serde_json::Value,
    throttle: foundry_core::throttle::Throttle,
) -> TaskBlockResult {
    let (raw_output, exit_code, success, summary, execution_output) = match response {
        Ok(r) => {
            let s = r.success;
            let out = format!("{}\n{}", r.stdout, r.stderr).trim().to_string();
            let summary = if s {
                "plan execution completed".to_string()
            } else {
                let first_line = r.stderr.lines().next().unwrap_or("agent failed");
                format!("plan execution failed: {first_line}")
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
            tracing::warn!(error = %err, "agent invocation failed");
            (None, None, false, format!("agent unavailable: {err}"), None)
        }
    };

    tracing::info!(project = %project, success = success, "plan execution completed");

    let context = LoopContext::extract_from(payload);
    let event_payload = Event::serialize_payload(&ExecutionCompletedPayload {
        project: project.to_string(),
        workflow: WorkflowType::Iterate.to_string(),
        success,
        summary: summary.clone(),
        execution_output,
        dry_run: None,
        retry_count: None,
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
}
