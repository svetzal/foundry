use std::path::PathBuf;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::loop_context::has_loop_context;
use foundry_sdk::payload::{ProjectCompletedPayload, SummarizeCompletedPayload};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock};

use crate::gateway::AgentGateway;

use super::{ReadOnlyAgentSpec, TriggerContext, fold_agent_outcome, invoke_summary_agent};

agent_block_new!(
    /// Generates a commit headline and summary after a successful workflow.
    ///
    /// Observer — sinks on `ProjectIterationCompleted` and `ProjectMaintenanceCompleted`
    /// (filters for `success=true` only).
    /// Uses `AgentGateway` with `Quick` capability and `ReadOnly` access.
    /// Emits `SummarizeCompleted` with headline and summary.
    pub struct SummarizeResult
);

fn accepts_summarize(trigger: &Event) -> bool {
    if has_loop_context(&trigger.payload) {
        return false;
    }
    trigger
        .parse_payload::<ProjectCompletedPayload>()
        .ok()
        .is_some_and(|p| p.success)
}

impl TaskBlock for SummarizeResult {
    task_block_meta! {
        name: "Summarize Result",
        kind: Observer,
        sinks_on: [ProjectIterationCompleted, ProjectMaintenanceCompleted],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        accepts_summarize(trigger)
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project,
            throttle,
            payload,
        } = TriggerContext::from_trigger(trigger);

        // accepts() already filtered loop-context and failed completions.
        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);
        let provider = super::chain_agent_provider(&payload);

        Box::pin(async move {
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

            let agent_file = super::resolve_agent_file(&entry.agent);

            let outcome = invoke_summary_agent(
                &*agent,
                &project,
                ReadOnlyAgentSpec {
                    prompt,
                    working_dir: project_path,
                    agent_file,
                    provider,
                    timeout: std::time::Duration::from_secs(120),
                },
                "summarize result",
            )
            .await;

            let (headline, summary) = extract_headline_summary(outcome, &project);

            tracing::info!(
                project = %project,
                headline = %headline,
                "summary generated"
            );

            super::emit_result(
                format!("{project}: {headline}"),
                EventType::SummarizeCompleted,
                &project,
                throttle,
                &SummarizeCompletedPayload {
                    project: project.clone(),
                    headline,
                    summary,
                },
            )
        })
    }
}

fn extract_headline_summary(
    outcome: crate::gateway::AgentOutcome,
    project: &str,
) -> (String, String) {
    fold_agent_outcome(
        outcome,
        project,
        "summary",
        |stdout| parse_summary_output(&stdout),
        |_ns| (format!("Update {project}"), "Automated maintenance completed.".to_string()),
    )
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
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::Registry;
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{AgentAccess, ModelTier, ReasoningEffort};

    use super::super::test_helpers;
    use super::{SummarizeResult, parse_summary_output};

    fn success_completion(project: &str, event_type: EventType) -> Event {
        Event::new(
            event_type,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "success": true,
                "summary": "",
                "workflow": "iterate",
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
                "summary": "",
                "workflow": "iterate",
            }),
        )
    }

    assert_block_meta!(
        SummarizeResult::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry { version: 2, projects: vec![] })),
        ),
        kind: Observer,
        sinks_on: [ProjectIterationCompleted, ProjectMaintenanceCompleted],
    );

    #[test]
    fn accepts_returns_false_when_loop_context_present() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project("my-project", "/tmp/test");
        let block = SummarizeResult::new(agent, registry);
        let trigger = Event::new(
            EventType::ProjectIterationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "success": true,
                "summary": "",
                "workflow": "iterate",
                "loop_context": { "strategic": { "iteration": 1 } }
            }),
        );

        assert!(!block.accepts(&trigger), "should not accept events with loop context");
    }

    #[test]
    fn accepts_returns_false_for_failed_completion() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project("my-project", "/tmp/test");
        let block = SummarizeResult::new(agent, registry);
        let trigger = failed_completion("my-project", EventType::ProjectMaintenanceCompleted);

        assert!(!block.accepts(&trigger), "should not accept failed completions");
    }

    #[test]
    fn accepts_returns_true_for_successful_completion_without_loop_context() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project("my-project", "/tmp/test");
        let block = SummarizeResult::new(agent, registry);
        let trigger = success_completion("my-project", EventType::ProjectIterationCompleted);

        assert!(
            block.accepts(&trigger),
            "should accept successful completions without loop context"
        );
    }

    #[tokio::test]
    async fn summarizes_successful_maintain() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(
            "HEADLINE: Update dependencies to latest versions\nSUMMARY: Updated cargo deps.",
        );
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
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
        assert_eq!(invocations[0].tier, ModelTier::Fast);
        assert_eq!(invocations[0].effort, ReasoningEffort::Low);
    }

    #[tokio::test]
    async fn summarizes_successful_iterate() {
        let dir = tempfile::tempdir().unwrap();
        let agent =
            FakeAgentGateway::success_with("HEADLINE: Fix linting\nSUMMARY: Fixed lint issues.");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
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
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
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
