use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

/// Generates a commit headline and summary after a successful workflow.
///
/// Observer — sinks on `ProjectIterationCompleted` and `ProjectMaintenanceCompleted`
/// (filters for `success=true` only).
/// Uses `AgentGateway` with `Quick` capability and `ReadOnly` access.
/// Emits `SummarizeCompleted` with headline and summary.
pub struct SummarizeResult {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl SummarizeResult {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, agent }
    }
}

impl TaskBlock for SummarizeResult {
    task_block_meta! {
        name: "Summarize Result",
        kind: Observer,
        sinks_on: [ProjectIterationCompleted, ProjectMaintenanceCompleted],
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

        // Self-filter: only summarize successful completions
        let success = payload.get("success").and_then(serde_json::Value::as_bool).unwrap_or(false);

        if !success {
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: workflow did not succeed".to_string(),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                })
            });
        }

        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            let Some(entry) = entry else {
                tracing::warn!(project = %project, "project not found in registry");
                return Ok(TaskBlockResult {
                    events: vec![],
                    success: false,
                    summary: format!("Project '{project}' not found in registry"),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                });
            };

            let project_path = PathBuf::from(&entry.path);

            let prompt = "Review the recent changes in this project's working directory \
                 (use `git diff HEAD~1` or `git log -1` to see what changed). \
                 Generate:\n\
                 1. A commit headline (max 72 characters, imperative mood)\n\
                 2. A 2-3 sentence summary of what changed and why\n\n\
                 Output ONLY in this exact format, nothing else:\n\
                 HEADLINE: <your headline here>\n\
                 SUMMARY: <your summary here>"
                .to_string();

            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            let request = AgentRequest {
                prompt,
                working_dir: project_path,
                access: AgentAccess::ReadOnly,
                capability: AgentCapability::Quick,
                agent_file,
                timeout: std::time::Duration::from_secs(120),
            };

            tracing::info!(project = %project, "generating summary via agent");

            let response = agent.invoke(&request).await;

            let (headline, summary) = match response {
                Ok(r) if r.success => parse_summary_output(&r.stdout),
                Ok(r) => {
                    tracing::warn!(
                        project = %project,
                        stderr = %r.stderr,
                        "summary agent failed"
                    );
                    (format!("Update {project}"), "Automated maintenance completed.".to_string())
                }
                Err(err) => {
                    tracing::warn!(error = %err, "agent invocation failed for summary");
                    (format!("Update {project}"), "Automated maintenance completed.".to_string())
                }
            };

            tracing::info!(
                project = %project,
                headline = %headline,
                "summary generated"
            );

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::SummarizeCompleted,
                    project.clone(),
                    throttle,
                    serde_json::json!({
                        "project": project,
                        "headline": headline,
                        "summary": summary,
                    }),
                )],
                success: true,
                summary: format!("{project}: {headline}"),
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
    }
}

/// Parse the agent output for HEADLINE: and SUMMARY: lines.
/// Falls back to defaults if the format doesn't match.
fn parse_summary_output(output: &str) -> (String, String) {
    let mut headline = None;
    let mut summary = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(h) = trimmed.strip_prefix("HEADLINE:") {
            headline = Some(h.trim().to_string());
        } else if let Some(s) = trimmed.strip_prefix("SUMMARY:") {
            summary = Some(s.trim().to_string());
        }
    }

    (
        headline.unwrap_or_else(|| {
            // Use first non-empty line as headline
            output
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("Update project")
                .chars()
                .take(72)
                .collect()
        }),
        summary.unwrap_or_else(|| "Automated changes applied.".to_string()),
    )
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

    use super::{SummarizeResult, parse_summary_output};

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

    fn success_completion(project: &str, event_type: EventType) -> Event {
        Event::new(
            event_type,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "success": true,
            }),
        )
    }

    fn failed_completion(project: &str, event_type: EventType) -> Event {
        Event::new(
            event_type,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "success": false,
            }),
        )
    }

    #[test]
    fn kind_is_observer() {
        let agent = FakeAgentGateway::success();
        let block = SummarizeResult::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_both_completion_events() {
        let agent = FakeAgentGateway::success();
        let block = SummarizeResult::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        let sinks = block.sinks_on();
        assert!(sinks.contains(&EventType::ProjectIterationCompleted));
        assert!(sinks.contains(&EventType::ProjectMaintenanceCompleted));
    }

    #[tokio::test]
    async fn skips_failed_completion() {
        let agent = FakeAgentGateway::success();
        let registry = registry_with_project("my-project", "/tmp/test");
        let block = SummarizeResult::new(agent.clone(), registry);
        let trigger = failed_completion("my-project", EventType::ProjectMaintenanceCompleted);

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(agent.invocations().is_empty());
    }

    #[tokio::test]
    async fn summarizes_successful_maintain() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(
            "HEADLINE: Update dependencies to latest versions\nSUMMARY: Updated cargo deps.",
        );
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = SummarizeResult::new(agent.clone(), registry);
        let trigger = success_completion("my-project", EventType::ProjectMaintenanceCompleted);

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::SummarizeCompleted);
        assert_eq!(result.events[0].payload["headline"], "Update dependencies to latest versions");
        assert_eq!(result.events[0].payload["summary"], "Updated cargo deps.");

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].access, AgentAccess::ReadOnly);
        assert_eq!(invocations[0].capability, AgentCapability::Quick);
    }

    #[tokio::test]
    async fn summarizes_successful_iterate() {
        let dir = tempfile::tempdir().unwrap();
        let agent =
            FakeAgentGateway::success_with("HEADLINE: Fix linting\nSUMMARY: Fixed lint issues.");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = SummarizeResult::new(agent, registry);
        let trigger = success_completion("my-project", EventType::ProjectIterationCompleted);

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::SummarizeCompleted);
    }

    #[tokio::test]
    async fn agent_failure_uses_fallback_headline() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::failure("agent error");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = SummarizeResult::new(agent, registry);
        let trigger = success_completion("my-project", EventType::ProjectMaintenanceCompleted);

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert!(result.events[0].payload["headline"].as_str().unwrap().contains("my-project"));
    }

    #[test]
    fn parse_summary_output_extracts_headline_and_summary() {
        let output = "HEADLINE: Fix formatting issues\nSUMMARY: Applied cargo fmt to all files.";
        let (headline, summary) = parse_summary_output(output);
        assert_eq!(headline, "Fix formatting issues");
        assert_eq!(summary, "Applied cargo fmt to all files.");
    }

    #[test]
    fn parse_summary_output_fallback_on_missing_format() {
        let output = "Some random output without the expected format";
        let (headline, summary) = parse_summary_output(output);
        assert_eq!(headline, "Some random output without the expected format");
        assert_eq!(summary, "Automated changes applied.");
    }

    #[test]
    fn parse_summary_output_handles_empty_output() {
        let (headline, summary) = parse_summary_output("");
        assert_eq!(headline, "Update project");
        assert_eq!(summary, "Automated changes applied.");
    }
}
