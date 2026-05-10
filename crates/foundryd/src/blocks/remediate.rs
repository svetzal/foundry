use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::MainBranchAuditedPayload;
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway};

use super::{AgentBlockSpec, invoke_agent};

agent_block_new!(
    /// Attempts to fix a vulnerability on the main branch.
    /// Mutator — events logged but not delivered at `audit_only`;
    /// simulated success at `dry_run`.
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
        // Respect the self-filter: only remediate when dirty.
        let p = trigger.parse_payload::<MainBranchAuditedPayload>().ok();
        let dirty = p.as_ref().is_none_or(|p| p.dirty);
        if !dirty {
            return vec![];
        }

        let cve = p.as_ref().map_or("unknown", |p| p.cve.as_str()).to_string();
        super::dry_run_remediation_event(trigger, Some(cve), None)
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let audit_payload = parse_payload!(trigger, MainBranchAuditedPayload);

        // Self-filter: only remediate when main branch is dirty.
        if !audit_payload.dirty {
            tracing::info!("main branch is clean, skipping remediation");
            return skip!("Skipped: main branch is clean");
        }

        let cve = audit_payload.cve.clone();

        // Resolve project agent and path from registry.
        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);

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

            let outcome = invoke_agent(
                &*agent,
                AgentBlockSpec {
                    prompt,
                    working_dir: project_path,
                    access: AgentAccess::Full,
                    capability: AgentCapability::Coding,
                    agent_file,
                    timeout: entry.timeout(),
                },
                &format!("remediate {cve}"),
                &project,
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
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::Registry;
    use foundry_core::task_block::TaskBlock;

    use crate::gateway::AgentCapability;
    use crate::gateway::fakes::FakeAgentGateway;

    use super::super::test_helpers;
    use super::RemediateVulnerability;

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
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
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
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
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
        assert_eq!(invocations[0].capability, AgentCapability::Coding);
    }
}
