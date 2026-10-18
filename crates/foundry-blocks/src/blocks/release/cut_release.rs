use std::path::PathBuf;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::MainBranchAuditedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockFuture, BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentGateway, ProcessShellGateway, ShellGateway};

use super::{
    FOREGROUND_INSTRUCTION, ReleaseInput, SimulatedSuccess, release_completed_event,
    release_outcome_event, run_release,
};

#[derive(Debug, PartialEq)]
enum CutDecision {
    SkipDirty,
    Proceed { cve: String },
}

fn decide_cut(trigger: &Event) -> CutDecision {
    let p = match trigger.parse_payload::<MainBranchAuditedPayload>() {
        Ok(p) => Some(p),
        Err(e) => {
            // Best-effort: conservative — treat unknown/unparseable payload as dirty
            // (see is_none_or below), same fallback as the previous .ok() behaviour;
            // log so unexpected payload drift is visible.
            tracing::debug!(error = %e, "failed to parse MainBranchAuditedPayload in decide_cut");
            None
        }
    };
    // Conservative: treat unknown/unparseable payload as dirty.
    let dirty = p.as_ref().is_none_or(|p| p.dirty);
    if dirty {
        return CutDecision::SkipDirty;
    }
    let cve = p.as_ref().map_or("unknown", |p| p.cve.as_str()).to_string();
    CutDecision::Proceed { cve }
}

/// The CVE a cut will target, or `None` when `CutRelease` will not run.
pub(super) fn cut_target(trigger: &Event) -> Option<String> {
    match decide_cut(trigger) {
        CutDecision::SkipDirty => None,
        CutDecision::Proceed { cve } => Some(cve),
    }
}

agent_execution_block! {
    /// Cuts a patch release in response to a vulnerability detected on the main branch.
    ///
    /// Mutator — simulated success at `dry_run`.
    ///
    /// Self-filters via `accepts()`: only dispatched when `dirty=false` in the trigger payload.
    pub struct CutRelease
}

impl SimulatedSuccess for CutRelease {
    /// `Some(cve)` → proceed with dry-run release event; `None` → skip (dirty).
    type Outcome = Option<String>;

    fn simulate(&self, trigger: &Event) -> Option<String> {
        cut_target(trigger)
    }

    fn success_events(&self, trigger: &Event, outcome: &Option<String>) -> Vec<Event> {
        match outcome {
            None => vec![],
            Some(cve) => vec![release_completed_event(
                &trigger.project,
                trigger.throttle,
                "patch",
                None,
                true,
                Some(cve),
                Some(true),
            )],
        }
    }
}

impl TaskBlock for CutRelease {
    task_block_meta! {
        name: "Cut Release",
        kind: Mutator,
        sinks_on: [MainBranchAudited],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        cut_target(trigger).is_some()
    }

    dry_run_via_simulation!();

    fn execute(&self, trigger: &Event) -> BlockFuture<'_> {
        let CutDecision::Proceed { cve } = decide_cut(trigger) else {
            // Defensive: accepts() filters SkipDirty before dispatch.
            return skip!("Skipped: main branch is dirty");
        };

        let project = trigger.project.clone();
        let trace_id = trigger.trace_id.clone();
        let throttle = trigger.throttle;
        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);
            let prompt = format!(
                "Cut a patch release for {project} fixing {cve}. \
                 Create a changelog entry, bump the patch version, tag the release, and push. \
                 {FOREGROUND_INSTRUCTION}"
            );
            tracing::info!(%project, %cve, "cutting patch release");

            let input = ReleaseInput {
                project: project.clone(),
                project_path,
                branch: entry.branch.clone(),
                prompt,
                trace_id: trace_id.clone(),
            };
            match run_release(agent.as_ref(), shell.as_ref(), input).await {
                Ok(output) => {
                    let event =
                        release_outcome_event(&project, throttle, "patch", Some(&cve), &output);
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
    use foundry_sdk::registry::Registry;
    use foundry_sdk::task_block::TaskBlock;

    use crate::blocks::SimulatedSuccess;
    use crate::blocks::test_helpers;
    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{AgentAccess, ModelTier, ReasoningEffort};

    use super::{CutDecision, CutRelease, decide_cut};

    fn empty_registry() -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        }))
    }

    fn dirty_trigger() -> Event {
        test_event!(EventType::MainBranchAudited, "my-project", { "dirty": true, "cve": "CVE-2026-1234" })
    }

    fn clean_trigger() -> Event {
        test_event!(EventType::MainBranchAudited, "my-project", { "dirty": false, "cve": "CVE-2026-1234" })
    }

    assert_block_meta!(
        CutRelease::new(FakeAgentGateway::success(), Arc::new(RwLock::new(Registry { version: 2, projects: vec![] }))),
        kind: Mutator,
        sinks_on: [MainBranchAudited],
    );

    #[test]
    fn decide_cut_skips_when_dirty() {
        assert_eq!(decide_cut(&dirty_trigger()), CutDecision::SkipDirty);
    }

    #[test]
    fn decide_cut_proceeds_when_clean() {
        assert!(
            matches!(decide_cut(&clean_trigger()), CutDecision::Proceed { cve } if cve == "CVE-2026-1234")
        );
    }

    #[test]
    fn accepts_returns_false_when_dirty() {
        let block = CutRelease::new(FakeAgentGateway::success(), empty_registry());
        assert!(!block.accepts(&dirty_trigger()));
    }

    #[test]
    fn accepts_returns_true_when_clean() {
        let block = CutRelease::new(FakeAgentGateway::success(), empty_registry());
        assert!(block.accepts(&clean_trigger()));
    }

    #[test]
    fn dry_run_and_accepts_agree_on_skip_for_dirty_branch() {
        let block = CutRelease::new(FakeAgentGateway::success(), empty_registry());
        let trigger = dirty_trigger();
        assert!(!block.accepts(&trigger), "accepts() must reject dirty trigger");
        assert!(
            block.dry_run_events(&trigger).is_empty(),
            "dry_run_events must skip for dirty trigger"
        );
        assert!(
            block.simulate(&trigger).is_none(),
            "simulate() must return None for dirty trigger"
        );
    }

    #[tokio::test]
    async fn skips_when_dirty_in_execute() {
        // Verifies the Defensive guard inside execute() — normally accepts() would filter first.
        let block = CutRelease::new(FakeAgentGateway::success(), empty_registry());
        let result = block.execute(&dirty_trigger()).await.unwrap();
        assert!(result.success);
        assert!(
            result.events.is_empty(),
            "expected `result.events` to be empty: {:?}",
            result.events
        );
        assert!(result.summary.contains("Skipped"));
    }

    #[tokio::test]
    async fn fails_when_project_not_in_registry() {
        let block = CutRelease::new(FakeAgentGateway::success(), empty_registry());
        let result = block.execute(&clean_trigger()).await.unwrap();
        assert!(!result.success);
        assert!(result.summary.contains("not found in registry"));
    }

    #[tokio::test]
    async fn fails_when_agents_md_missing() {
        let (entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", false);
        let registry = test_helpers::registry_with_entry(entry);
        let block = CutRelease::new(FakeAgentGateway::success(), registry);
        let result = block.execute(&clean_trigger()).await.unwrap();
        assert!(!result.success);
        assert!(
            result.events.is_empty(),
            "expected `result.events` to be empty: {:?}",
            result.events
        );
        assert!(result.summary.contains("AGENTS.md not found"));
    }

    #[tokio::test]
    async fn successful_release_emits_release_completed() {
        let (entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        let registry = test_helpers::registry_with_entry(entry);
        let agent =
            FakeAgentGateway::success_with("Release complete! Tagged as v1.2.3 and pushed.");
        let block = CutRelease::new(agent.clone(), registry);
        let result = block.execute(&clean_trigger()).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(result.events[0].payload["new_tag"], "v1.2.3");
        assert_eq!(result.events[0].payload["success"], true);
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-1234");
        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].tier, ModelTier::Balanced);
        assert_eq!(invocations[0].effort, ReasoningEffort::Medium);
        assert_eq!(invocations[0].access, AgentAccess::Full);
        assert!(invocations[0].prompt.contains("CVE-2026-1234"));
    }

    #[tokio::test]
    async fn release_failure_emits_release_completed_with_success_false() {
        let (entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::failure("Claude CLI failed");
        let block = CutRelease::new(agent, registry);
        let result = block.execute(&clean_trigger()).await.unwrap();
        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(result.events[0].payload["success"], false);
    }

    fn ok(stdout: &str) -> crate::shell::CommandResult {
        crate::shell::CommandResult {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    /// The git answers a release reads before the agent runs, for a clean
    /// checkout at `/repo` on `main` that is `behind` commits behind
    /// `origin/main` (see the same helper in `execute_release`).
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

    fn git_calls(shell: &crate::gateway::fakes::FakeShellGateway) -> Vec<String> {
        shell.invocations().iter().map(|call| call.args.join(" ")).collect()
    }

    fn patch_release_block(
        agent: Arc<dyn crate::gateway::AgentGateway>,
        shell: Arc<dyn crate::gateway::ShellGateway>,
    ) -> (CutRelease, Option<tempfile::TempDir>) {
        let (entry, dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        let registry = test_helpers::registry_with_entry(entry);
        (CutRelease::with_gateways(agent, registry, shell), dir)
    }

    #[tokio::test]
    async fn a_failed_patch_release_reports_what_it_left_in_the_checkout() {
        use crate::gateway::fakes::FakeShellGateway;
        let mut results = synced_checkout(0);
        results.extend([ok(" M Cargo.toml\0"), ok("0\n")]);
        let (block, _dir) = patch_release_block(
            FakeAgentGateway::failure("Claude CLI failed"),
            FakeShellGateway::sequence(results),
        );

        let result = block.execute(&clean_trigger()).await.unwrap();

        assert!(!result.success);
        assert!(
            result.summary.ends_with("Left uncommitted in /repo: Cargo.toml (1 file)."),
            "summary: {}",
            result.summary
        );
        assert_eq!(result.events[0].payload["left_uncommitted"], serde_json::json!(["Cargo.toml"]));
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-1234");
    }

    #[tokio::test]
    async fn a_clean_checkout_behind_origin_is_fast_forwarded_before_the_patch_release() {
        use crate::gateway::fakes::FakeShellGateway;
        let shell = FakeShellGateway::sequence(synced_checkout(3));
        let agent = FakeAgentGateway::success_with("Tagged as v1.2.4 and pushed.");
        let (block, _dir) = patch_release_block(
            agent.clone(),
            Arc::clone(&shell) as Arc<dyn crate::gateway::ShellGateway>,
        );

        let result = block.execute(&clean_trigger()).await.unwrap();

        assert!(result.success, "{}", result.summary);
        assert_eq!(agent.invocations().len(), 1);
        let calls = git_calls(&shell);
        assert_eq!(calls[3], "fetch origin main");
        assert_eq!(calls[5], "merge --ff-only origin/main");
        let payload = &result.events[0].payload;
        assert_eq!(payload["success"], true);
        assert_eq!(payload["new_tag"], "v1.2.4");
        assert_eq!(payload["cve"], "CVE-2026-1234");
        assert_eq!(payload["fast_forwarded"], 3);
        assert!(payload.get("sync_failure").is_none());
    }

    #[tokio::test]
    async fn a_dirty_checkout_refuses_the_patch_release_without_invoking_the_agent() {
        use crate::gateway::fakes::FakeShellGateway;
        let shell =
            FakeShellGateway::sequence(vec![ok("/repo\n"), ok("main\n"), ok("?? scratch.rs\n")]);
        let agent = FakeAgentGateway::success_with("v9.9.9");
        let (block, _dir) = patch_release_block(
            agent.clone(),
            Arc::clone(&shell) as Arc<dyn crate::gateway::ShellGateway>,
        );

        let result = block.execute(&clean_trigger()).await.unwrap();

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
        assert_eq!(result.events.len(), 1);
        let payload = &result.events[0].payload;
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(payload["success"], false);
        assert_eq!(payload["sync_failure"], "dirty_tree");
        assert_eq!(payload["new_tag"], serde_json::Value::Null);
        assert_eq!(payload["cve"], "CVE-2026-1234");
        assert!(
            result
                .summary
                .starts_with("Release refused before any version bump: dirty_tree: "),
            "summary: {}",
            result.summary
        );
    }
}
