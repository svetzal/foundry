use std::path::PathBuf;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::ReleaseRequestedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockFuture, BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentGateway, ProcessShellGateway, ShellGateway};

use super::{ReleaseInput, SimulatedSuccess, release_completed_event, run_release};

agent_execution_block! {
    /// Executes a manual release in response to a `ReleaseRequested` event.
    ///
    /// Mutator — simulated success at `dry_run`.
    ///
    /// Does not override `accepts()` (default `true`); the `actions.release` guard
    /// is checked inside `execute()` as a domain skip.
    pub struct ExecuteRelease
}

impl SimulatedSuccess for ExecuteRelease {
    /// `Some(())` → proceed with dry-run release event; `None` → skip (action disabled or project absent).
    type Outcome = Option<()>;

    fn simulate(&self, trigger: &Event) -> Option<()> {
        let guard = super::super::read_registry(&self.registry).ok()?;
        let entry = guard.find_project(&trigger.project)?;
        if !entry.actions.release {
            return None;
        }
        Some(())
    }

    fn success_events(&self, trigger: &Event, outcome: &Option<()>) -> Vec<Event> {
        match outcome {
            None => vec![],
            Some(()) => vec![release_completed_event(
                &trigger.project,
                trigger.throttle,
                "manual",
                None,
                true,
                None,
                Some(true),
            )],
        }
    }
}

impl TaskBlock for ExecuteRelease {
    task_block_meta! {
        name: "Execute Release",
        kind: Mutator,
        sinks_on: [ReleaseRequested],
    }

    dry_run_via_simulation!();

    fn execute(&self, trigger: &Event) -> BlockFuture<'_> {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let bump = trigger.parse_payload::<ReleaseRequestedPayload>().ok().and_then(|p| p.bump);
        let entry = require_project!(self, project);

        if !entry.actions.release {
            // Domain skip: release action is disabled for this project.
            return skip!("Skipped: release action disabled");
        }

        let agent = Arc::clone(&self.agent);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);

            let bump_instruction = match &bump {
                Some(b) => format!("The version bump type is {b}."),
                None => {
                    "Determine the appropriate version bump from the changelog and unreleased changes."
                        .to_string()
                }
            };

            let prompt = format!(
                "Release {project}. Follow the release process documented in AGENTS.md exactly.\n\
                 {bump_instruction}\n\
                 Complete all steps: run quality gates, update the changelog, bump the version in all \
                 locations, commit (the version-bump commit must be the HEAD commit), then create the \
                 git tag pointing at that HEAD commit, and finally push both the commit and the tag. \
                 IMPORTANT: create the git tag ONLY after the version-bump/changelog commit so the \
                 tag points at the correct commit. Verify that `git rev-parse <tag>^{{commit}}` \
                 matches `git rev-parse HEAD` before pushing. \
                 Output the new version tag on a line by itself (e.g. v1.2.3)."
            );

            tracing::info!(%project, bump = bump.as_deref().unwrap_or("auto"), "executing release");

            let input = ReleaseInput {
                project: project.clone(),
                project_path,
                prompt,
            };
            match run_release(agent.as_ref(), shell.as_ref(), input).await {
                Ok(output) => {
                    let event = release_completed_event(
                        &project,
                        throttle,
                        "manual",
                        output.new_tag.as_deref(),
                        output.success,
                        None,
                        None,
                    );
                    Ok(TaskBlockResult {
                        events: vec![event],
                        success: output.success,
                        summary: output.summary,
                        raw_output: output.raw_output,
                        exit_code: output.exit_code,
                        ..Default::default()
                    })
                }
                Err(err) => Ok(TaskBlockResult::failure(format!("Block execution failed: {err}"))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::{ActionFlags, Registry};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use crate::blocks::test_helpers;
    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{AgentAccess, ModelTier, ReasoningEffort};

    use super::ExecuteRelease;

    fn empty_registry() -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        }))
    }

    fn release_actions() -> ActionFlags {
        ActionFlags {
            release: true,
            ..ActionFlags::default()
        }
    }

    fn release_trigger(payload: serde_json::Value) -> Event {
        Event::new(EventType::ReleaseRequested, "my-project".to_string(), Throttle::Full, payload)
    }

    assert_block_meta!(
        ExecuteRelease::new(FakeAgentGateway::success(), Arc::new(RwLock::new(Registry { version: 2, projects: vec![] }))),
        kind: Mutator,
        sinks_on: [ReleaseRequested],
    );

    #[tokio::test]
    async fn execute_release_skips_when_action_disabled() {
        let (entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        let registry = test_helpers::registry_with_entry(entry);
        let block = ExecuteRelease::new(FakeAgentGateway::success(), registry);
        let trigger = release_trigger(serde_json::json!({}));
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("Skipped"));
    }

    #[tokio::test]
    async fn execute_release_fails_when_project_not_in_registry() {
        let block = ExecuteRelease::new(FakeAgentGateway::success(), empty_registry());
        let trigger = release_trigger(serde_json::json!({}));
        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert!(result.summary.contains("not found in registry"));
    }

    #[tokio::test]
    async fn execute_release_fails_when_agents_md_missing() {
        let (mut entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", false);
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let block = ExecuteRelease::new(FakeAgentGateway::success(), registry);
        let trigger = release_trigger(serde_json::json!({}));
        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("AGENTS.md not found"));
    }

    #[tokio::test]
    async fn execute_release_success_emits_release_completed() {
        let (mut entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::success_with("Release complete!\nv2.0.0\nAll steps done.");
        let block = ExecuteRelease::new(agent.clone(), registry);
        let trigger = release_trigger(serde_json::json!({ "bump": "minor" }));
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(result.events[0].payload["new_tag"], "v2.0.0");
        assert_eq!(result.events[0].payload["success"], true);
        assert_eq!(result.events[0].payload["release"], "manual");
        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].tier, ModelTier::Balanced);
        assert_eq!(invocations[0].effort, ReasoningEffort::Medium);
        assert_eq!(invocations[0].access, AgentAccess::Full);
        assert!(invocations[0].prompt.contains("minor"));
        assert!(invocations[0].prompt.contains("AGENTS.md"));
    }

    #[tokio::test]
    async fn execute_release_auto_bump_when_no_bump_specified() {
        let (mut entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::success_with("v1.3.0");
        let block = ExecuteRelease::new(agent.clone(), registry);
        let trigger = release_trigger(serde_json::json!({}));
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        let invocations = agent.invocations();
        assert!(invocations[0].prompt.contains("Determine the appropriate version bump"));
    }

    #[tokio::test]
    async fn execute_release_failure_emits_release_completed_with_success_false() {
        let (mut entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::failure("release failed");
        let block = ExecuteRelease::new(agent, registry);
        let trigger = release_trigger(serde_json::json!({}));
        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(result.events[0].payload["success"], false);
    }

    #[tokio::test]
    async fn dry_run_events_are_empty_when_release_action_disabled() {
        let (entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        // entry has release=false (default ActionFlags)
        let registry = test_helpers::registry_with_entry(entry);
        let block = ExecuteRelease::new(FakeAgentGateway::success(), registry);
        let trigger = Event::new(
            EventType::ReleaseRequested,
            "my-project".to_string(),
            Throttle::DryRun,
            serde_json::json!({}),
        );
        assert!(block.dry_run_events(&trigger).is_empty());
    }

    fn git_revparse_sequence(
        tag_commit: &str,
        head_commit: &str,
    ) -> Arc<dyn crate::gateway::ShellGateway> {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;
        FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: format!("{tag_commit}\n"),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: format!("{head_commit}\n"),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ])
    }

    #[tokio::test]
    async fn execute_release_fails_when_tag_not_at_head() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agent guidance").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let mut entry = test_helpers::project_entry("my-project", dir.path().to_str().unwrap());
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::success_with("Release complete!\nv3.0.0\nAll done.");
        let shell = git_revparse_sequence("aaaa", "bbbb");
        let block = ExecuteRelease::with_gateways(agent, registry, shell);
        let trigger = release_trigger(serde_json::json!({}));
        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(result.events[0].payload["success"], false);
        assert!(result.summary.contains("does not point at HEAD"));
    }

    #[tokio::test]
    async fn execute_release_succeeds_when_tag_at_head() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agent guidance").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let mut entry = test_helpers::project_entry("my-project", dir.path().to_str().unwrap());
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::success_with("Release complete!\nv3.0.0\nAll done.");
        let shell = git_revparse_sequence("aaaa", "aaaa");
        let block = ExecuteRelease::with_gateways(agent, registry, shell);
        let trigger = release_trigger(serde_json::json!({}));
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events[0].payload["success"], true);
        assert_eq!(result.events[0].payload["new_tag"], "v3.0.0");
    }

    #[tokio::test]
    async fn execute_release_fails_when_tag_missing_from_git() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agent guidance").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let mut entry = test_helpers::project_entry("my-project", dir.path().to_str().unwrap());
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::success_with("v3.0.0");
        let shell = FakeShellGateway::sequence(vec![CommandResult {
            stdout: String::new(),
            stderr: "fatal: ambiguous argument 'v3.0.0^{commit}'".to_string(),
            exit_code: 128,
            success: false,
        }]);
        let block = ExecuteRelease::with_gateways(agent, registry, shell);
        let trigger = release_trigger(serde_json::json!({}));
        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert_eq!(result.events[0].payload["success"], false);
    }

    #[tokio::test]
    async fn tag_verification_skipped_when_no_git_dir() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;
        let (mut entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::success_with("v4.0.0");
        let shell = FakeShellGateway::always(CommandResult {
            stdout: String::new(),
            stderr: "should not be called".to_string(),
            exit_code: 1,
            success: false,
        });
        let block = ExecuteRelease::with_gateways(agent, registry, shell);
        let trigger = release_trigger(serde_json::json!({}));
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events[0].payload["new_tag"], "v4.0.0");
    }
}
