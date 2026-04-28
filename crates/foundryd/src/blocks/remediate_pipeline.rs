use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{PipelineCheckedPayload, RemediationCompletedPayload};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

/// Attempts to fix a failing GitHub Actions pipeline.
/// Mutator -- events logged but not delivered at `audit_only`;
/// simulated success at `dry_run`.
///
/// Self-filters: only acts when `passing=false` in the trigger payload.
///
/// Uses `AgentGateway` with `Coding` capability and `Full` access to
/// diagnose and fix the CI failure.
pub struct RemediatePipeline {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl RemediatePipeline {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, agent }
    }

    /// Generous timeout for Claude CLI -- pipeline fixes can take several minutes.
    const CLAUDE_TIMEOUT: Duration = Duration::from_secs(900); // 15 minutes

    #[cfg(test)]
    fn with_agent(registry: Arc<Registry>, agent: Arc<dyn AgentGateway>) -> Self {
        Self { registry, agent }
    }
}

impl TaskBlock for RemediatePipeline {
    task_block_meta! {
        name: "Remediate Pipeline",
        kind: Mutator,
        sinks_on: [PipelineChecked],
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        // Respect the self-filter: only remediate when failing.
        let p = trigger
            .parse_payload::<PipelineCheckedPayload>()
            .expect("dry_run_events called with invalid PipelineChecked payload");
        if p.passing {
            return vec![];
        }

        let event_payload = Event::serialize_payload(&RemediationCompletedPayload {
            cve: None,
            success: true,
            summary: None,
            dry_run: Some(true),
            pipeline_fix: Some(true),
        })
        .expect("RemediationCompletedPayload is infallibly serializable");

        vec![Event::new(
            EventType::RemediationCompleted,
            trigger.project.clone(),
            trigger.throttle,
            event_payload,
        )]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        // Self-filter: only remediate when pipeline is failing.
        let p = parse_payload!(trigger, PipelineCheckedPayload);

        if p.passing {
            tracing::info!("pipeline is passing, no remediation needed");
            return skip!("Pipeline is passing, no remediation needed");
        }

        let failure_logs = p.failure_logs.unwrap_or_default();
        let run_name = p.run_name.clone();

        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);

        tracing::info!(%project, %run_name, "remediating pipeline failure");

        Box::pin(run_remediation(project, throttle, run_name, failure_logs, entry, agent))
    }
}

async fn run_remediation(
    project: String,
    throttle: foundry_core::throttle::Throttle,
    run_name: String,
    failure_logs: String,
    entry: foundry_core::registry::ProjectEntry,
    agent: Arc<dyn AgentGateway>,
) -> anyhow::Result<TaskBlockResult> {
    let project_path = PathBuf::from(&entry.path);

    // Verify AGENTS.md exists -- required by Claude Code for agentic automation.
    let agents_md = project_path.join("AGENTS.md");
    if !agents_md.exists() {
        tracing::warn!(path = %agents_md.display(), "AGENTS.md not found, skipping pipeline remediation");
        return Ok(TaskBlockResult::failure(format!(
            "AGENTS.md not found at {}; cannot invoke Claude CLI",
            agents_md.display()
        )));
    }

    let prompt = format!(
        "The GitHub Actions CI pipeline '{run_name}' for project '{project}' is failing. \
         Diagnose and fix the CI failure so the pipeline passes.\n\n\
         Failure logs:\n{failure_logs}"
    );

    let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

    let request = AgentRequest {
        prompt,
        working_dir: project_path,
        access: AgentAccess::Full,
        capability: AgentCapability::Coding,
        agent_file,
        timeout: RemediatePipeline::CLAUDE_TIMEOUT,
    };

    tracing::info!(
        project = %project,
        %run_name,
        "invoking agent for pipeline remediation"
    );

    let response = agent.invoke(&request).await;

    Ok(super::build_agent_remediation_result(
        &project,
        throttle,
        response,
        None,
        Some(true),
        "Pipeline fixed",
        "Pipeline fix failed",
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::Registry;
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;

    use super::super::test_helpers;
    use super::RemediatePipeline;

    fn empty_registry() -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![],
        })
    }

    fn failing_trigger(project: &str) -> Event {
        Event::new(
            EventType::PipelineChecked,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "passing": false,
                "conclusion": "failure",
                "run_id": 99999,
                "run_name": "CI",
                "failure_logs": "error: test failed",
            }),
        )
    }

    fn passing_trigger(project: &str) -> Event {
        Event::new(
            EventType::PipelineChecked,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "passing": true,
                "conclusion": "success",
                "run_id": 12345,
                "run_name": "CI",
            }),
        )
    }

    #[test]
    fn kind_is_mutator() {
        let agent = FakeAgentGateway::success();
        let block = RemediatePipeline::new(agent, empty_registry());
        assert_eq!(block.kind(), BlockKind::Mutator);
    }

    #[tokio::test]
    async fn skips_when_pipeline_passing() {
        let agent = FakeAgentGateway::success();
        let block = RemediatePipeline::new(agent, empty_registry());
        let t = passing_trigger("my-project");

        let result = block.execute(&t).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("passing"));
    }

    #[tokio::test]
    async fn fails_when_project_not_in_registry() {
        let agent = FakeAgentGateway::success();
        let block = RemediatePipeline::new(agent, empty_registry());
        let t = failing_trigger("unknown-project");

        let result = block.execute(&t).await.unwrap();
        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("not found in registry"));
    }

    #[tokio::test]
    async fn successful_remediation_emits_remediation_completed() {
        let (mut entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        entry.repo = "owner/repo".to_string();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::success_with("Fixed the CI pipeline");
        let block = RemediatePipeline::with_agent(registry, agent);
        let t = failing_trigger("my-project");

        let result = block.execute(&t).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::RemediationCompleted);
        assert_eq!(result.events[0].payload["pipeline_fix"], true);
        assert_eq!(result.events[0].payload["success"], true);
    }

    #[tokio::test]
    async fn failed_remediation_still_emits_event() {
        let (mut entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        entry.repo = "owner/repo".to_string();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::failure("agent exited with code 1");
        let block = RemediatePipeline::with_agent(registry, agent);
        let t = failing_trigger("my-project");

        let result = block.execute(&t).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::RemediationCompleted);
        assert_eq!(result.events[0].payload["success"], false);
        assert!(result.summary.contains("failed"));
    }
}
