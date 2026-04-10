use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use foundry_core::event::{Event, EventType};
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
        let passing = trigger.payload_bool_or("passing", true);
        if passing {
            return vec![];
        }

        vec![Event::new(
            EventType::RemediationCompleted,
            trigger.project.clone(),
            trigger.throttle,
            serde_json::json!({
                "pipeline_fix": true,
                "success": true,
                "dry_run": true,
            }),
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
        let passing = trigger.payload_bool_or("passing", true);

        if passing {
            tracing::info!("pipeline is passing, no remediation needed");
            return Box::pin(async {
                Ok(TaskBlockResult::success("Pipeline is passing, no remediation needed", vec![]))
            });
        }

        let failure_logs = trigger.payload_str_or("failure_logs", "").to_string();
        let run_name = trigger.payload_str_or("run_name", "unknown").to_string();

        let entry = self.registry.find_project(&project).cloned();
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
    entry: Option<foundry_core::registry::ProjectEntry>,
    agent: Arc<dyn AgentGateway>,
) -> anyhow::Result<TaskBlockResult> {
    let Some(entry) = entry else {
        return Ok(super::project_not_found_result(&project));
    };

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

    let (raw_output, exit_code, success, summary) = match response {
        Ok(r) => {
            let s = r.success;
            let out = format!("{}\n{}", r.stdout, r.stderr).trim().to_string();
            let summary = if s {
                "pipeline remediation completed".to_string()
            } else {
                let first_line = r.stderr.lines().next().unwrap_or("agent failed");
                format!("pipeline remediation failed: {first_line}")
            };
            (Some(out), Some(r.exit_code), s, summary)
        }
        Err(err) => {
            tracing::warn!(error = %err, "agent not available or failed to spawn");
            (None, None, false, format!("agent unavailable: {err}"))
        }
    };

    tracing::info!(
        project = %project,
        success = success,
        summary = %summary,
        "pipeline remediation completed"
    );

    Ok(TaskBlockResult {
        events: vec![Event::new(
            EventType::RemediationCompleted,
            project,
            throttle,
            serde_json::json!({
                "pipeline_fix": true,
                "success": success,
                "summary": summary,
            }),
        )],
        success,
        summary: if success {
            format!("Pipeline fixed: {summary}")
        } else {
            format!("Pipeline fix failed: {summary}")
        },
        raw_output,
        exit_code,
        audit_artifacts: vec![],
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;
    use tempfile::TempDir;

    use crate::gateway::fakes::FakeAgentGateway;

    use super::RemediatePipeline;

    fn empty_registry() -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![],
        })
    }

    fn registry_with_project(name: &str, has_agents_md: bool) -> Arc<Registry> {
        let project_path = if has_agents_md {
            let dir = TempDir::new().unwrap();
            let agents_path = dir.path().join("AGENTS.md");
            std::fs::write(&agents_path, "# Agent guidance").unwrap();
            let p = dir.path().to_str().unwrap().to_string();
            std::mem::forget(dir);
            p
        } else {
            "/nonexistent/path".to_string()
        };

        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: name.to_string(),
                path: project_path,
                stack: Stack::Rust,
                agent: String::new(),
                repo: "owner/repo".to_string(),
                branch: "main".to_string(),
                skip: None,
                notes: None,
                actions: ActionFlags::default(),
                install: None,
                timeout_secs: None,
            }],
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
        let registry = registry_with_project("my-project", true);
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
        let registry = registry_with_project("my-project", true);
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
