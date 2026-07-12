use std::path::PathBuf;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::MainBranchAuditedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockFuture, BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentGateway, ProcessShellGateway, ShellGateway};

use super::{ReleaseInput, SimulatedSuccess, release_completed_event, run_release};

#[derive(Debug, PartialEq)]
enum CutDecision {
    SkipDirty,
    Proceed { cve: String },
}

fn decide_cut(trigger: &Event) -> CutDecision {
    let p = trigger.parse_payload::<MainBranchAuditedPayload>().ok();
    // Conservative: treat unknown/unparseable payload as dirty.
    let dirty = p.as_ref().is_none_or(|p| p.dirty);
    if dirty {
        return CutDecision::SkipDirty;
    }
    let cve = p.as_ref().map_or("unknown", |p| p.cve.as_str()).to_string();
    CutDecision::Proceed { cve }
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
        match decide_cut(trigger) {
            CutDecision::SkipDirty => None,
            CutDecision::Proceed { cve } => Some(cve),
        }
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
        matches!(decide_cut(trigger), CutDecision::Proceed { .. })
    }

    dry_run_via_simulation!();

    fn execute(&self, trigger: &Event) -> BlockFuture<'_> {
        let CutDecision::Proceed { cve } = decide_cut(trigger) else {
            // Defensive: accepts() filters SkipDirty before dispatch.
            return skip!("Skipped: main branch is dirty");
        };

        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);
            let prompt = format!(
                "Cut a patch release for {project} fixing {cve}. \
                 Create a changelog entry, bump the patch version, tag the release, and push."
            );
            tracing::info!(%project, %cve, "cutting patch release");

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
                        "patch",
                        output.new_tag.as_deref(),
                        output.success,
                        Some(&cve),
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
        assert!(result.events.is_empty());
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
        assert!(result.events.is_empty());
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
}
