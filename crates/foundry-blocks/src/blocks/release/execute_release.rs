use std::path::PathBuf;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::ReleaseRequestedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockFuture, BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentGateway, ProcessShellGateway, ShellGateway};

use super::{
    FOREGROUND_INSTRUCTION, ReleaseInput, SimulatedSuccess, release_completed_event, run_release,
    with_left_uncommitted,
};

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
        super::release_enabled(&self.registry, &trigger.project).then_some(())
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
        let trace_id = trigger.trace_id.clone();
        let throttle = trigger.throttle;
        // Best-effort: an absent or unparseable bump field falls back to
        // "let the agent determine the bump from the changelog," which is a
        // legitimate release strategy on its own — not a fault worth failing
        // the release over. Warn so a persistently malformed payload is visible.
        let bump = trigger
            .parse_payload::<ReleaseRequestedPayload>()
            .map_err(|e| {
                tracing::warn!(error = %e, "release-requested payload unreadable; defaulting to auto bump");
                e
            })
            .ok()
            .and_then(|p| p.bump);
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
                 {FOREGROUND_INSTRUCTION}\n\
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
                trace_id: trace_id.clone(),
            };
            match run_release(agent.as_ref(), shell.as_ref(), input).await {
                Ok(output) => {
                    let event = with_left_uncommitted(
                        release_completed_event(
                            &project,
                            throttle,
                            "manual",
                            output.new_tag.as_deref(),
                            output.success,
                            None,
                            None,
                        ),
                        &output.left_uncommitted,
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
        assert!(
            result.events.is_empty(),
            "expected `result.events` to be empty: {:?}",
            result.events
        );
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
        assert!(
            result.events.is_empty(),
            "expected `result.events` to be empty: {:?}",
            result.events
        );
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
        assert!(
            block.dry_run_events(&trigger).is_empty(),
            "expected `block.dry_run_events(&trigger)` to be empty: {:?}",
            block.dry_run_events(&trigger)
        );
    }

    #[test]
    fn simulate_warns_and_skips_when_registry_poisoned() {
        let registry = empty_registry();
        let r2 = Arc::clone(&registry);
        let _ = std::thread::spawn(move || {
            let _guard = r2.write().unwrap();
            panic!("intentional poison");
        })
        .join();
        let block = ExecuteRelease::new(FakeAgentGateway::success(), registry);
        let trigger = Event::new(
            EventType::ReleaseRequested,
            "my-project".to_string(),
            Throttle::DryRun,
            serde_json::json!({}),
        );

        assert!(
            block.dry_run_events(&trigger).is_empty(),
            "expected `block.dry_run_events(&trigger)` to be empty: {:?}",
            block.dry_run_events(&trigger)
        );
    }

    fn ok(stdout: &str) -> crate::shell::CommandResult {
        crate::shell::CommandResult {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    fn failed(stderr: &str) -> crate::shell::CommandResult {
        crate::shell::CommandResult {
            stdout: String::new(),
            stderr: stderr.to_string(),
            exit_code: 128,
            success: false,
        }
    }

    /// `git rev-parse --show-toplevel`, `git status` and `git rev-list` for a
    /// checkout at `/repo` holding `porcelain` and `unpushed` commits.
    fn checkout_snapshot(porcelain: &str, unpushed: &str) -> Vec<crate::shell::CommandResult> {
        vec![ok("/repo\n"), ok(porcelain), ok(unpushed)]
    }

    /// A clean checkout, then tag verification's two `git rev-parse` calls,
    /// then a clean checkout again for a release that fails.
    fn git_revparse_sequence(
        tag_commit: &str,
        head_commit: &str,
    ) -> Arc<dyn crate::gateway::ShellGateway> {
        use crate::gateway::fakes::FakeShellGateway;
        let mut results = checkout_snapshot("", "0\n");
        results.push(ok(&format!("{tag_commit}\n")));
        results.push(ok(&format!("{head_commit}\n")));
        results.extend([ok(""), ok("0\n")]);
        FakeShellGateway::sequence(results)
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
            stderr: "fatal: not a git repository".to_string(),
            exit_code: 1,
            success: false,
        });
        let block = ExecuteRelease::with_gateways(agent, registry, shell);
        let trigger = release_trigger(serde_json::json!({}));
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events[0].payload["new_tag"], "v4.0.0");
    }

    fn release_block(
        agent: Arc<dyn crate::gateway::AgentGateway>,
        shell: Arc<dyn crate::gateway::ShellGateway>,
    ) -> (ExecuteRelease, Option<tempfile::TempDir>) {
        let (mut entry, dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        (ExecuteRelease::with_gateways(agent, registry, shell), dir)
    }

    fn git_calls(shell: &crate::gateway::fakes::FakeShellGateway) -> Vec<String> {
        shell.invocations().iter().map(|call| call.args.join(" ")).collect()
    }

    #[tokio::test]
    async fn a_failed_release_names_the_files_and_commits_it_left_in_the_checkout() {
        use crate::gateway::fakes::FakeShellGateway;
        let mut results = checkout_snapshot("", "0\n");
        results.extend([
            ok(" M mix.exs\0 M CHANGELOG.md\0 M README.md\0?? guides/getting_started.md\0"),
            ok("1\n"),
        ]);
        let shell = FakeShellGateway::sequence(results);
        let (block, _dir) = release_block(
            FakeAgentGateway::failure("agent stream timed out after 900.0s"),
            Arc::clone(&shell) as Arc<dyn crate::gateway::ShellGateway>,
        );

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(!result.success);
        assert!(
            result.summary.contains(
                "Left uncommitted in /repo: mix.exs, CHANGELOG.md, README.md, \
                 guides/getting_started.md (4 files). 1 local commit not pushed."
            ),
            "summary: {}",
            result.summary
        );
        assert_eq!(
            result.events[0].payload["left_uncommitted"],
            serde_json::json!([
                "mix.exs",
                "CHANGELOG.md",
                "README.md",
                "guides/getting_started.md"
            ])
        );
        assert_eq!(
            git_calls(&shell),
            [
                "rev-parse --show-toplevel",
                "status --porcelain=v1 -z",
                "rev-list --count @{upstream}..HEAD",
                "status --porcelain=v1 -z",
                "rev-list --count @{upstream}..HEAD",
            ]
        );
    }

    #[tokio::test]
    async fn dirt_present_before_the_release_is_reported_as_pre_existing() {
        use crate::gateway::fakes::FakeShellGateway;
        let mut results = checkout_snapshot(" M notes.md\0", "0\n");
        results.extend([ok(" M notes.md\0 M mix.exs\0"), ok("0\n")]);
        let (block, _dir) = release_block(
            FakeAgentGateway::failure("release failed"),
            FakeShellGateway::sequence(results),
        );

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(
            result.summary.contains("Left uncommitted in /repo: mix.exs (1 file)."),
            "summary: {}",
            result.summary
        );
        assert!(
            result
                .summary
                .contains("Already uncommitted before the release: notes.md (1 file)."),
            "summary: {}",
            result.summary
        );
        assert!(!result.summary.contains("not pushed"), "summary: {}", result.summary);
        assert_eq!(result.events[0].payload["left_uncommitted"], serde_json::json!(["mix.exs"]));
    }

    #[tokio::test]
    async fn a_successful_release_takes_no_second_snapshot_and_reports_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agent guidance").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let mut entry = test_helpers::project_entry("my-project", dir.path().to_str().unwrap());
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let shell = crate::gateway::fakes::FakeShellGateway::sequence({
            let mut results = checkout_snapshot("", "0\n");
            results.extend([ok("aaaa\n"), ok("aaaa\n"), ok(" M stray.rs\0")]);
            results
        });
        let block = ExecuteRelease::with_gateways(
            FakeAgentGateway::success_with("Release complete!\nv3.0.0"),
            registry,
            Arc::clone(&shell) as Arc<dyn crate::gateway::ShellGateway>,
        );

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(result.success);
        assert_eq!(result.summary, "Release completed — v3.0.0");
        assert!(result.events[0].payload.get("left_uncommitted").is_none());
        assert_eq!(
            git_calls(&shell),
            [
                "rev-parse --show-toplevel",
                "status --porcelain=v1 -z",
                "rev-list --count @{upstream}..HEAD",
                "rev-parse v3.0.0^{commit}",
                "rev-parse HEAD",
            ]
        );
    }

    #[tokio::test]
    async fn a_checkout_outside_git_leaves_the_failure_summary_unchanged() {
        use crate::gateway::fakes::FakeShellGateway;
        let shell = FakeShellGateway::always(failed("fatal: not a git repository"));
        let (block, _dir) = release_block(
            FakeAgentGateway::failure("release failed"),
            Arc::clone(&shell) as Arc<dyn crate::gateway::ShellGateway>,
        );

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.summary, "Claude CLI exited with code 1; stderr: release failed");
        assert!(result.events[0].payload.get("left_uncommitted").is_none());
        assert_eq!(git_calls(&shell), ["rev-parse --show-toplevel"]);
    }

    #[tokio::test]
    async fn a_git_status_failure_after_the_release_leaves_the_failure_summary_unchanged() {
        use crate::gateway::fakes::FakeShellGateway;
        let mut results = checkout_snapshot("", "0\n");
        results.push(failed("fatal: index file corrupt"));
        let (block, _dir) = release_block(
            FakeAgentGateway::failure("release failed"),
            FakeShellGateway::sequence(results),
        );

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.summary, "Claude CLI exited with code 1; stderr: release failed");
        assert!(result.events[0].payload.get("left_uncommitted").is_none());
    }
}
