use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::loop_context::forward_chain_context;
use foundry_core::payload::TriageCompletedPayload;
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::workflow::WorkflowType;

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentOutcome, AgentRequest};

use super::TriggerContext;

/// Creates a step-by-step correction plan for an accepted assessment.
///
/// Observer — sinks on `TriageCompleted` (filters for `accepted=true` only).
/// Uses `AgentGateway` with `Reasoning` capability and `ReadOnly` access.
/// Emits `PlanCompleted` with the plan text.
pub struct CreatePlan {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl CreatePlan {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, agent }
    }
}

impl TaskBlock for CreatePlan {
    task_block_meta! {
        name: "Create Plan",
        kind: Observer,
        sinks_on: [TriageCompleted],
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

        // Self-filter: only create plan for accepted triages
        let p = match trigger.parse_payload::<TriageCompletedPayload>() {
            Ok(p) => p,
            Err(e) => return Box::pin(async move { Err(e) }),
        };

        if !p.accepted {
            return Box::pin(async {
                Ok(TaskBlockResult::success("Skipped: triage was rejected", vec![]))
            });
        }

        let entry = match super::require_project(&self.registry, &project) {
            Ok(e) => e,
            Err(result) => return Box::pin(async { Ok(result) }),
        };
        let agent = Arc::clone(&self.agent);

        let principle = p.principle.clone();
        let category = p.category.clone();
        let assessment = p.assessment.clone();

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);

            let principle = principle.as_str();
            let category = category.as_str();
            let assessment = assessment.as_str();

            let prompt = format!(
                "You are creating a correction plan for project '{project}'.\n\n\
                 Assessment:\n\
                 - Principle violated: {principle}\n\
                 - Category: {category}\n\
                 - Details: {assessment}\n\n\
                 Create a step-by-step plan to correct this violation. Each step should be:\n\
                 - Specific (name exact files and functions where possible)\n\
                 - Minimal (only changes needed to address this violation)\n\
                 - Testable (describe how to verify the step succeeded)\n\n\
                 Output the plan as a numbered list of concrete steps."
            );

            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            let request = AgentRequest {
                prompt,
                working_dir: project_path,
                access: AgentAccess::ReadOnly,
                capability: AgentCapability::Reasoning,
                agent_file,
                timeout: entry.timeout(),
            };

            tracing::info!(project = %project, principle = %principle, "creating plan via agent");

            let response = agent.invoke(&request).await;

            let (plan, success) = match AgentOutcome::from_response(response) {
                AgentOutcome::Success { stdout } => (stdout.trim().to_string(), true),
                AgentOutcome::AgentFailed { stderr } => {
                    tracing::warn!(project = %project, stderr = %stderr, "plan agent failed");
                    (format!("Plan generation failed: {stderr}"), false)
                }
                AgentOutcome::Unavailable { error } => {
                    tracing::warn!(error = %error, "agent invocation failed for plan");
                    return Ok(TaskBlockResult::failure(format!("agent unavailable: {error}")));
                }
            };

            tracing::info!(project = %project, success = success, "plan created");

            let mut event_payload = serde_json::json!({
                "project": project,
                "plan": plan,
                "principle": principle,
                "category": category,
                "assessment": assessment,
                "workflow": WorkflowType::Iterate,
            });
            forward_chain_context(&payload, &mut event_payload);

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::PlanCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
                success,
                summary: format!("{project}: plan created for {principle} violation"),
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
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
    use super::CreatePlan;

    fn triage_accepted_event(project: &str) -> Event {
        Event::new(
            EventType::TriageCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "accepted": true,
                "reason": "violation is significant",
                "severity": 7,
                "principle": "DRY",
                "category": "duplication",
                "assessment": "Duplicate validation logic.",
                "audit_name": "fix-duplication",
                "workflow": "iterate",
            }),
        )
    }

    fn triage_rejected_event(project: &str) -> Event {
        Event::new(
            EventType::TriageCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "accepted": false,
                "reason": "too trivial",
                "severity": 2,
                "principle": "unknown",
                "category": "conventions",
                "assessment": "",
                "workflow": "iterate",
            }),
        )
    }

    #[test]
    fn kind_is_observer() {
        let agent = FakeAgentGateway::success();
        let block = CreatePlan::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_triage_completed() {
        let agent = FakeAgentGateway::success();
        let block = CreatePlan::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.sinks_on(), &[EventType::TriageCompleted]);
    }

    #[tokio::test]
    async fn skips_rejected_triage() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project("my-project", "/tmp/test");
        let block = CreatePlan::new(agent.clone(), registry);
        let trigger = triage_rejected_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(agent.invocations().is_empty());
    }

    #[tokio::test]
    async fn creates_plan_for_accepted_triage() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(
            "1. Extract shared validation into a helper function\n2. Update callers\n3. Add tests",
        );
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CreatePlan::new(agent.clone(), registry);
        let trigger = triage_accepted_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::PlanCompleted);
        assert!(result.events[0].payload["plan"].as_str().unwrap().contains("Extract"));
        assert_eq!(result.events[0].payload["principle"], "DRY");

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].access, AgentAccess::ReadOnly);
        assert_eq!(invocations[0].capability, AgentCapability::Reasoning);
    }

    #[tokio::test]
    async fn forwards_actions_and_audit_name() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("1. Do the thing");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CreatePlan::new(agent, registry);
        let trigger = Event::new(
            EventType::TriageCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "accepted": true,
                "reason": "violation is significant",
                "severity": 7,
                "principle": "SRP",
                "category": "architecture",
                "assessment": "Too many responsibilities.",
                "audit_name": "fix-srp",
                "actions": {"maintain": true},
                "workflow": "iterate",
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events[0].payload["audit_name"], "fix-srp");
        assert_eq!(result.events[0].payload["actions"]["maintain"], true);
    }
}
