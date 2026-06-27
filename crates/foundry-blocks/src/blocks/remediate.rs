use std::path::PathBuf;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::MainBranchAuditedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock};

use crate::gateway::AgentGateway;

use super::TriggerContext;

/// Decision outcome for a remediate-vulnerability trigger.
///
/// Centralises the `dirty` guard shared between `dry_run_events` and `execute`.
#[derive(Debug, PartialEq)]
enum RemediateDecision {
    /// Main branch is clean — nothing to remediate.
    SkipClean,
    /// Main branch is dirty; CVE identifier extracted from payload (or "unknown").
    Proceed { cve: String },
}

/// Evaluate the trigger and decide what `RemediateVulnerability` should do.
fn decide_remediate(trigger: &Event) -> RemediateDecision {
    let p = trigger.parse_payload::<MainBranchAuditedPayload>().ok();
    let dirty = p.as_ref().is_none_or(|p| p.dirty);
    if !dirty {
        return RemediateDecision::SkipClean;
    }
    let cve = p.as_ref().map_or("unknown", |p| p.cve.as_str()).to_string();
    RemediateDecision::Proceed { cve }
}

agent_block_new!(
    /// Attempts to fix a vulnerability on the main branch.
    /// Mutator — simulated success at `dry_run`.
    ///
    /// Self-filters: only acts when `dirty=true` in the trigger payload.
    ///
    /// Uses `AgentGateway` with `Coding` capability and `Full` access to fix
    /// the vulnerable dependency.
    pub struct RemediateVulnerability
);

impl TaskBlock for RemediateVulnerability {
    task_block_meta! {
        name: "Remediate Vulnerability",
        kind: Mutator,
        sinks_on: [MainBranchAudited],
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        match decide_remediate(trigger) {
            RemediateDecision::SkipClean => vec![],
            RemediateDecision::Proceed { cve } => {
                super::dry_run_remediation_event(trigger, Some(cve), None)
            }
        }
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project,
            throttle,
            payload,
        } = TriggerContext::from_trigger(trigger);

        // Self-filter: only remediate when main branch is dirty.
        // CVE is also extracted here so both dry_run_events and execute share the same logic.
        let RemediateDecision::Proceed { cve } = decide_remediate(trigger) else {
            tracing::info!("main branch is clean, skipping remediation");
            return skip!("Skipped: main branch is clean");
        };

        // Resolve project agent and path from registry.
        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);
        let provider = super::chain_agent_provider(&payload);

        tracing::info!(%cve, "remediating vulnerability");

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);

            let prompt = format!(
                "You are remediating vulnerability {cve} in project '{project}'. \
                 Update the affected dependencies to patched versions, \
                 fix any breaking changes caused by the updates, \
                 and ensure the project builds and passes its quality gates."
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
                    timeout: entry.timeout(),
                },
                &format!("remediate {cve}"),
            )
            .await;

            let success_label = format!("Remediated {cve}");
            let failure_label = format!("Remediation of {cve} failed");
            Ok(super::build_agent_remediation_result(
                &project,
                throttle,
                outcome,
                Some(cve),
                None,
                &success_label,
                &failure_label,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::Registry;
    use foundry_sdk::task_block::TaskBlock;

    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{ModelTier, ReasoningEffort};

    use super::super::test_helpers;
    use super::{RemediateDecision, RemediateVulnerability, decide_remediate};

    fn dirty_trigger(project: &str, cve: &str) -> Event {
        test_event!(EventType::MainBranchAudited, project, { "dirty": true, "cve": cve })
    }

    fn clean_trigger(project: &str) -> Event {
        test_event!(EventType::MainBranchAudited, project, { "dirty": false, "cve": "CVE-2026-9999" })
    }

    #[tokio::test]
    async fn skips_when_main_branch_is_clean() {
        let agent = FakeAgentGateway::success();
        let block = RemediateVulnerability::new(
            agent,
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        let trigger = clean_trigger("any-project");

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("clean"));
    }

    #[tokio::test]
    async fn fails_when_project_not_in_registry() {
        let agent = FakeAgentGateway::success();
        let block = RemediateVulnerability::new(
            agent,
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        let trigger = dirty_trigger("unknown-project", "CVE-2026-1234");

        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("not found in registry"));
    }

    #[tokio::test]
    async fn emits_remediation_completed_on_agent_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "claude",
        ));
        let agent = FakeAgentGateway::success_with("Fixed dependency");
        let block = RemediateVulnerability::new(agent, registry);
        let trigger = dirty_trigger("my-project", "CVE-2026-9999");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::RemediationCompleted);
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-9999");
        assert_eq!(result.events[0].payload["success"], true);
    }

    #[tokio::test]
    async fn emits_remediation_completed_on_agent_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "claude",
        ));
        let agent = FakeAgentGateway::failure("agent exited with code 1");
        let block = RemediateVulnerability::new(agent, registry);
        let trigger = dirty_trigger("my-project", "CVE-2026-1234");

        let result = block.execute(&trigger).await.unwrap();

        // Block still emits the event even on failure (with success=false).
        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::RemediationCompleted);
        assert_eq!(result.events[0].payload["success"], false);
        assert!(result.summary.contains("failed"));
    }

    #[tokio::test]
    async fn records_agent_invocation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            dir.path().to_str().unwrap(),
            "claude",
        ));
        let agent = FakeAgentGateway::success();
        let block = RemediateVulnerability::new(agent.clone(), registry);
        let trigger = dirty_trigger("my-project", "CVE-2026-0001");

        block.execute(&trigger).await.unwrap();

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert!(invocations[0].prompt.contains("CVE-2026-0001"));
        assert_eq!(invocations[0].tier, ModelTier::Balanced);
        assert_eq!(invocations[0].effort, ReasoningEffort::Medium);
    }

    // --- decide_remediate pure function tests ---

    #[test]
    fn decide_remediate_skips_when_clean() {
        let trigger = clean_trigger("proj");
        assert_eq!(decide_remediate(&trigger), RemediateDecision::SkipClean);
    }

    #[test]
    fn decide_remediate_proceeds_when_dirty() {
        let trigger = dirty_trigger("proj", "CVE-2026-1111");
        assert!(
            matches!(decide_remediate(&trigger), RemediateDecision::Proceed { cve } if cve == "CVE-2026-1111")
        );
    }

    #[test]
    fn decide_remediate_uses_empty_string_cve_when_field_absent() {
        // MainBranchAuditedPayload has `#[serde(default)] pub cve: String`, so when absent
        // the field defaults to an empty string rather than "unknown".
        let trigger = test_event!(EventType::MainBranchAudited, "proj", { "dirty": true });
        assert!(
            matches!(decide_remediate(&trigger), RemediateDecision::Proceed { cve } if cve.is_empty())
        );
    }

    #[test]
    fn dry_run_returns_empty_when_clean() {
        let agent = FakeAgentGateway::success();
        let block = RemediateVulnerability::new(
            agent,
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        let trigger = clean_trigger("proj");
        assert!(block.dry_run_events(&trigger).is_empty());
    }

    #[test]
    fn dry_run_and_execute_agree_on_skip_for_clean() {
        // dry_run_events and execute must both skip when branch is clean.
        let agent = FakeAgentGateway::success();
        let block = RemediateVulnerability::new(
            agent,
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        let trigger = clean_trigger("proj");
        assert!(block.dry_run_events(&trigger).is_empty(), "dry_run must skip when clean");
        // execute path is tested in skips_when_main_branch_is_clean
    }
}
