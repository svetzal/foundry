use std::path::PathBuf;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::ReleaseRequestedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockFuture, BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentGateway, ProcessShellGateway, ShellGateway};

use super::{
    FOREGROUND_INSTRUCTION, ReleaseInput, SimulatedSuccess, release_completed_event,
    release_outcome_event, run_release,
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
            let branch = entry.branch.clone();

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
                 The checkout has already been fetched and fast-forwarded to origin/{branch}; \
                 do not pull, rebase, or switch branches.\n\
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
                branch,
                prompt,
                trace_id: trace_id.clone(),
            };
            match run_release(agent.as_ref(), shell.as_ref(), input).await {
                Ok(output) => {
                    let event = release_outcome_event(&project, throttle, "manual", None, &output);
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

    /// The git answers a release reads before the agent runs, for a clean
    /// checkout at `/repo` on `main` that is `behind` commits behind
    /// `origin/main`: the work-tree probe, the branch check, the sync (status,
    /// fetch, measure, and the fast-forward when behind) and the dirt
    /// snapshot (status and unpushed count).
    fn synced_checkout(behind: u32) -> Vec<crate::shell::CommandResult> {
        let mut results = vec![
            ok("/repo\n"),
            ok("main\n"),
            ok(""),
            ok(""),
            ok(&format!("0\t{behind}\n")),
        ];
        if behind > 0 {
            results.push(ok("Updating\n"));
        }
        results.extend([ok(""), ok("0\n")]);
        results
    }

    /// The git calls [`synced_checkout`] answers, in order.
    fn synced_calls(behind: u32) -> Vec<&'static str> {
        let mut calls = vec![
            "rev-parse --show-toplevel",
            "rev-parse --abbrev-ref HEAD",
            "status --porcelain",
            "fetch origin main",
            "rev-list --left-right --count HEAD...origin/main",
        ];
        if behind > 0 {
            calls.push("merge --ff-only origin/main");
        }
        calls.extend([
            "status --porcelain=v1 -z",
            "rev-list --count @{upstream}..HEAD",
        ]);
        calls
    }

    /// A clean, level checkout, then tag verification's two `git rev-parse`
    /// calls, then a clean checkout again for a release that fails.
    fn git_revparse_sequence(
        tag_commit: &str,
        head_commit: &str,
    ) -> Arc<dyn crate::gateway::ShellGateway> {
        use crate::gateway::fakes::FakeShellGateway;
        let mut results = synced_checkout(0);
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
        let mut results = synced_checkout(0);
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
        let mut expected = synced_calls(0);
        expected.extend([
            "status --porcelain=v1 -z",
            "rev-list --count @{upstream}..HEAD",
        ]);
        assert_eq!(git_calls(&shell), expected);
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
            let mut results = synced_checkout(0);
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
        assert_eq!(result.events[0].payload["fast_forwarded"], 0);
        let mut expected = synced_calls(0);
        expected.extend(["rev-parse v3.0.0^{commit}", "rev-parse HEAD"]);
        assert_eq!(git_calls(&shell), expected);
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
        let mut results = synced_checkout(0);
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

    // -- Sync before the agent runs (scripted shell) --

    /// Git subcommands a refused release must never reach.
    const MUTATING: [&str; 5] = ["merge", "rebase", "commit", "tag", "push"];

    fn assert_no_mutating_calls(shell: &crate::gateway::fakes::FakeShellGateway) {
        for call in git_calls(shell) {
            let sub = call.split_whitespace().next().unwrap_or_default();
            assert!(!MUTATING.contains(&sub), "unexpected mutating git call: git {call}");
        }
    }

    /// The mojentic-ru shape of 2026-10-18: a clean registered checkout that
    /// never pulled the last release. The release fetches and fast-forwards
    /// it before the agent is asked to bump anything, and tells the agent so.
    #[tokio::test]
    async fn a_clean_checkout_behind_origin_is_fast_forwarded_before_the_agent_runs() {
        use crate::gateway::fakes::FakeShellGateway;
        let shell = FakeShellGateway::sequence(synced_checkout(2));
        let agent = FakeAgentGateway::success_with("Release complete!\nv3.0.1");
        let (block, _dir) = release_block(
            agent.clone(),
            Arc::clone(&shell) as Arc<dyn crate::gateway::ShellGateway>,
        );

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(result.success, "{}", result.summary);
        // Every git call the release made is the sync; the agent ran after
        // all of them (it makes none of its own here).
        assert_eq!(git_calls(&shell), synced_calls(2));
        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert!(
            invocations[0]
                .prompt
                .contains("already been fetched and fast-forwarded to origin/main"),
            "prompt: {}",
            invocations[0].prompt
        );
        assert!(
            invocations[0].prompt.contains("do not pull, rebase, or switch branches"),
            "prompt: {}",
            invocations[0].prompt
        );
        let payload = &result.events[0].payload;
        assert_eq!(payload["success"], true);
        assert_eq!(payload["new_tag"], "v3.0.1");
        assert_eq!(payload["fast_forwarded"], 2);
        assert!(payload.get("sync_failure").is_none());
    }

    #[tokio::test]
    async fn a_diverged_checkout_refuses_the_release_without_invoking_the_agent() {
        use crate::gateway::fakes::FakeShellGateway;
        let shell = FakeShellGateway::sequence(vec![
            ok("/repo\n"),
            ok("main\n"),
            ok(""),
            ok(""),
            ok("1\t1\n"),
        ]);
        let agent = FakeAgentGateway::success_with("v9.9.9");
        let (block, _dir) = release_block(
            agent.clone(),
            Arc::clone(&shell) as Arc<dyn crate::gateway::ShellGateway>,
        );

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(!result.success);
        assert!(agent.invocations().is_empty(), "no agent session may be spent");
        assert_no_mutating_calls(&shell);
        assert_eq!(result.events.len(), 1);
        let payload = &result.events[0].payload;
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(payload["success"], false);
        assert_eq!(payload["sync_failure"], "diverged");
        assert_eq!(payload["new_tag"], serde_json::Value::Null);
        assert!(payload.get("fast_forwarded").is_none());
        assert!(payload.get("left_uncommitted").is_none());
        assert!(
            result
                .summary
                .starts_with("Release refused before any version bump: diverged: "),
            "summary: {}",
            result.summary
        );
        assert!(
            result
                .summary
                .contains("1 commit(s) ahead of and 1 commit(s) behind origin/main"),
            "summary: {}",
            result.summary
        );
        assert!(
            result.summary.contains("nothing was committed, tagged or published"),
            "summary: {}",
            result.summary
        );
        assert!(result.raw_output.is_none());
        assert!(result.exit_code.is_none());
    }

    #[tokio::test]
    async fn a_dirty_checkout_refuses_the_release_before_fetching() {
        use crate::gateway::fakes::FakeShellGateway;
        let shell =
            FakeShellGateway::sequence(vec![ok("/repo\n"), ok("main\n"), ok(" M mix.exs\n")]);
        let agent = FakeAgentGateway::success_with("v9.9.9");
        let (block, _dir) = release_block(
            agent.clone(),
            Arc::clone(&shell) as Arc<dyn crate::gateway::ShellGateway>,
        );

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(!result.success);
        assert!(agent.invocations().is_empty(), "no agent session may be spent");
        assert_eq!(
            git_calls(&shell),
            [
                "rev-parse --show-toplevel",
                "rev-parse --abbrev-ref HEAD",
                "status --porcelain"
            ]
        );
        let payload = &result.events[0].payload;
        assert_eq!(payload["success"], false);
        assert_eq!(payload["sync_failure"], "dirty_tree");
        assert_eq!(payload["new_tag"], serde_json::Value::Null);
        assert!(
            result
                .summary
                .starts_with("Release refused before any version bump: dirty_tree: "),
            "summary: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn a_checkout_on_another_branch_refuses_the_release_without_fetching() {
        use crate::gateway::fakes::FakeShellGateway;
        let shell = FakeShellGateway::sequence(vec![ok("/repo\n"), ok("release-prep\n")]);
        let agent = FakeAgentGateway::success_with("v9.9.9");
        let (block, _dir) = release_block(
            agent.clone(),
            Arc::clone(&shell) as Arc<dyn crate::gateway::ShellGateway>,
        );

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(!result.success);
        assert!(agent.invocations().is_empty(), "no agent session may be spent");
        assert_eq!(git_calls(&shell), ["rev-parse --show-toplevel", "rev-parse --abbrev-ref HEAD"]);
        let payload = &result.events[0].payload;
        assert_eq!(payload["sync_failure"], "wrong_branch");
        assert_eq!(payload["new_tag"], serde_json::Value::Null);
        assert!(
            result
                .summary
                .contains("wrong_branch: wrong branch: release-prep, expected main"),
            "summary: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn an_unreachable_remote_refuses_the_release() {
        use crate::gateway::fakes::FakeShellGateway;
        let shell = FakeShellGateway::sequence(vec![
            ok("/repo\n"),
            ok("main\n"),
            ok(""),
            failed("fatal: could not read from remote repository"),
        ]);
        let agent = FakeAgentGateway::success_with("v9.9.9");
        let (block, _dir) = release_block(
            agent.clone(),
            Arc::clone(&shell) as Arc<dyn crate::gateway::ShellGateway>,
        );

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(!result.success);
        assert!(agent.invocations().is_empty(), "no agent session may be spent");
        assert_no_mutating_calls(&shell);
        assert_eq!(git_calls(&shell).last().map(String::as_str), Some("fetch origin main"));
        let payload = &result.events[0].payload;
        assert_eq!(payload["sync_failure"], "remote_unavailable");
        assert_eq!(payload["new_tag"], serde_json::Value::Null);
    }

    /// Git inside a work tree that cannot say which branch is checked out is a
    /// checkout a human must look at; the block fails without an event, as it
    /// does for a missing `AGENTS.md`.
    #[tokio::test]
    async fn a_checkout_whose_branch_cannot_be_read_fails_the_block_without_an_event() {
        use crate::gateway::fakes::FakeShellGateway;
        let shell = FakeShellGateway::sequence(vec![ok("/repo\n"), failed("fatal: bad revision")]);
        let agent = FakeAgentGateway::success_with("v9.9.9");
        let (block, _dir) = release_block(
            agent.clone(),
            Arc::clone(&shell) as Arc<dyn crate::gateway::ShellGateway>,
        );

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(!result.success);
        assert!(agent.invocations().is_empty(), "no agent session may be spent");
        assert!(result.events.is_empty(), "{:?}", result.events);
        assert!(
            result.summary.contains("cannot verify the release checkout is on main"),
            "summary: {}",
            result.summary
        );
    }

    // -- Sync before the agent runs (real git) --

    /// A registered checkout with `AGENTS.md` committed and pushed, so its
    /// tree is clean, and a second clone that stands in for another host
    /// pushing to origin.
    fn release_fixture() -> (test_helpers::git_repo::Repo, std::path::PathBuf) {
        use test_helpers::git_repo::{commit, git, repo};
        let fixture = repo();
        commit(&fixture.work, "AGENTS.md", "# Release process\n", "add agent guidance");
        git(&fixture.work, &["push", "-q", "origin", "main"]);
        let root = fixture.remote.parent().unwrap().to_path_buf();
        git(&root, &["clone", "-q", fixture.remote.to_str().unwrap(), "other"]);
        let other = root.join("other");
        git(&other, &["config", "user.email", "other@example.com"]);
        git(&other, &["config", "user.name", "Other"]);
        (fixture, other)
    }

    fn real_git_block(
        checkout: &std::path::Path,
        agent: Arc<dyn crate::gateway::AgentGateway>,
    ) -> ExecuteRelease {
        let mut entry = test_helpers::project_entry("my-project", checkout.to_str().unwrap());
        entry.actions = release_actions();
        ExecuteRelease::with_gateways(
            agent,
            test_helpers::registry_with_entry(entry),
            Arc::new(test_helpers::git_repo::CleanProcessShellGateway),
        )
    }

    #[tokio::test]
    async fn real_git_a_checkout_two_behind_origin_is_fast_forwarded_before_the_release() {
        use test_helpers::git_repo::{commit, git};
        let (fixture, other) = release_fixture();
        commit(&other, "CHANGELOG.md", "## [3.0.0]\n", "Release 3.0.0");
        commit(&other, "README.md", "after the release", "follow-up");
        git(&other, &["push", "-q", "origin", "main"]);
        let origin_head = git(&other, &["rev-parse", "HEAD"]);
        let agent = FakeAgentGateway::success_with("done");
        let block = real_git_block(&fixture.work, agent.clone());

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(result.success, "{}", result.summary);
        assert_eq!(agent.invocations().len(), 1);
        assert_eq!(git(&fixture.work, &["rev-parse", "HEAD"]), origin_head);
        assert_eq!(
            git(&fixture.work, &["rev-parse", "HEAD"]),
            git(&fixture.work, &["rev-parse", "origin/main"])
        );
        assert_eq!(result.events[0].payload["fast_forwarded"], 2);
        assert!(result.events[0].payload.get("sync_failure").is_none());
    }

    #[tokio::test]
    async fn real_git_a_diverged_checkout_is_refused_and_left_untouched() {
        use test_helpers::git_repo::{commit, git};
        let (fixture, other) = release_fixture();
        commit(&fixture.work, "notes.md", "local only", "local commit never pushed");
        commit(&other, "README.md", "remote moved", "remote commit");
        git(&other, &["push", "-q", "origin", "main"]);
        let head_before = git(&fixture.work, &["rev-parse", "HEAD"]);
        let agent = FakeAgentGateway::success_with("v9.9.9");
        let block = real_git_block(&fixture.work, agent.clone());

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(!result.success);
        assert!(agent.invocations().is_empty(), "no agent session may be spent");
        assert_eq!(result.events[0].payload["sync_failure"], "diverged");
        assert_eq!(result.events[0].payload["new_tag"], serde_json::Value::Null);
        assert_eq!(git(&fixture.work, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(git(&fixture.work, &["tag"]), "");
        assert_eq!(git(&fixture.work, &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn real_git_a_dirty_checkout_is_refused_with_the_edit_intact() {
        use test_helpers::git_repo::git;
        let (fixture, _other) = release_fixture();
        std::fs::write(fixture.work.join("README.md"), "unstaged edit").unwrap();
        let agent = FakeAgentGateway::success_with("v9.9.9");
        let block = real_git_block(&fixture.work, agent.clone());

        let result = block.execute(&release_trigger(serde_json::json!({}))).await.unwrap();

        assert!(!result.success);
        assert!(agent.invocations().is_empty(), "no agent session may be spent");
        assert_eq!(result.events[0].payload["sync_failure"], "dirty_tree");
        assert_eq!(
            std::fs::read_to_string(fixture.work.join("README.md")).unwrap(),
            "unstaged edit"
        );
        assert_eq!(git(&fixture.work, &["status", "--porcelain"]), "M README.md");
        assert_eq!(git(&fixture.work, &["tag"]), "");
    }
}
