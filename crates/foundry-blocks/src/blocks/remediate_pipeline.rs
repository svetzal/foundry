use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::PipelineCheckedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentGateway, AgentProvider};

use super::TriggerContext;

agent_block_new!(
    /// Attempts to fix a failing GitHub Actions pipeline.
    /// Mutator -- simulated success at `dry_run`.
    ///
    /// Self-filters: only acts when `passing=false` in the trigger payload.
    ///
    /// Uses `AgentGateway` with `Coding` capability and `Full` access to
    /// diagnose and fix the CI failure.
    pub struct RemediatePipeline
);

impl RemediatePipeline {
    /// Generous timeout for Claude CLI -- pipeline fixes can take several minutes.
    const CLAUDE_TIMEOUT: Duration = Duration::from_secs(900); // 15 minutes
}

impl TaskBlock for RemediatePipeline {
    task_block_meta! {
        name: "Remediate Pipeline",
        kind: Mutator,
        sinks_on: [PipelineChecked],
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        // Respect the self-filter: only remediate when failing.
        let p = match trigger.parse_payload::<PipelineCheckedPayload>() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "dry_run_events: invalid PipelineChecked payload; emitting no events");
                return vec![];
            }
        };
        if p.passing {
            return vec![];
        }

        super::dry_run_remediation_event(trigger, None, Some(true))
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project,
            throttle,
            payload,
        } = TriggerContext::from_trigger(trigger);

        // Self-filter: only remediate when pipeline is failing.
        let p = parse_payload!(trigger, PipelineCheckedPayload);

        if p.passing {
            tracing::info!("pipeline is passing, no remediation needed");
            return skip!("Pipeline is passing, no remediation needed");
        }

        let failure_logs = p.failure_logs.unwrap_or_default();
        let run_name = p.run_name.clone().unwrap_or_default();
        let provider = super::chain_agent_provider(&payload);

        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);

        tracing::info!(%project, %run_name, "remediating pipeline failure");

        Box::pin(run_remediation(
            RemediationRequest {
                project,
                throttle,
                run_name,
                failure_logs,
                entry,
                provider,
            },
            agent,
        ))
    }
}

struct RemediationRequest {
    project: String,
    throttle: foundry_sdk::throttle::Throttle,
    run_name: String,
    failure_logs: String,
    entry: foundry_sdk::registry::ProjectEntry,
    provider: Option<AgentProvider>,
}

async fn run_remediation(
    request: RemediationRequest,
    agent: Arc<dyn AgentGateway>,
) -> anyhow::Result<TaskBlockResult> {
    let RemediationRequest {
        project,
        throttle,
        run_name,
        failure_logs,
        entry,
        provider,
    } = request;
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

    let outcome = super::invoke_coding_agent(
        &*agent,
        &project,
        super::CodingAgentSpec {
            working_dir: project_path,
            prompt,
            agent_file,
            provider,
            timeout: RemediatePipeline::CLAUDE_TIMEOUT,
        },
        "remediate pipeline",
    )
    .await;

    Ok(super::build_agent_remediation_result(
        &project,
        throttle,
        outcome,
        None,
        Some(true),
        "Pipeline fixed",
        "Pipeline fix failed",
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::EventType;
    use foundry_sdk::registry::Registry;
    use foundry_sdk::task_block::TaskBlock;

    use crate::gateway::fakes::FakeAgentGateway;

    use super::super::test_helpers;
    use super::RemediatePipeline;

    assert_block_meta!(
        RemediatePipeline::new(FakeAgentGateway::success(), Arc::new(RwLock::new(Registry { version: 2, projects: vec![] }))),
        kind: Mutator,
        sinks_on: [PipelineChecked],
    );

    #[tokio::test]
    async fn skips_when_pipeline_passing() {
        let agent = FakeAgentGateway::success();
        let block = RemediatePipeline::new(agent, test_helpers::empty_registry());
        let t = test_event!(EventType::PipelineChecked, "my-project", {
            "passing": true,
            "conclusion": "success",
            "run_id": 12345,
            "run_name": "CI",
        });

        let result = block.execute(&t).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("passing"));
    }

    #[tokio::test]
    async fn fails_when_project_not_in_registry() {
        let agent = FakeAgentGateway::success();
        let block = RemediatePipeline::new(agent, test_helpers::empty_registry());
        let t = test_event!(EventType::PipelineChecked, "unknown-project", {
            "passing": false,
            "conclusion": "failure",
            "run_id": 99999,
            "run_name": "CI",
            "failure_logs": "error: test failed",
        });

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
        let block = RemediatePipeline::new(agent, registry);
        let t = test_event!(EventType::PipelineChecked, "my-project", {
            "passing": false,
            "conclusion": "failure",
            "run_id": 99999,
            "run_name": "CI",
            "failure_logs": "error: test failed",
        });

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
        let block = RemediatePipeline::new(agent, registry);
        let t = test_event!(EventType::PipelineChecked, "my-project", {
            "passing": false,
            "conclusion": "failure",
            "run_id": 99999,
            "run_name": "CI",
            "failure_logs": "error: test failed",
        });

        let result = block.execute(&t).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::RemediationCompleted);
        assert_eq!(result.events[0].payload["success"], false);
        assert!(result.summary.contains("failed"));
    }
}
