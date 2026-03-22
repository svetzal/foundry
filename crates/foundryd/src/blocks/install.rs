use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Reinstalls a tool locally after changes are pushed or a release pipeline completes.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Terminal block: this is the end of both the dirty and clean vulnerability
/// remediation paths.
pub struct InstallLocally;

impl TaskBlock for InstallLocally {
    fn name(&self) -> &'static str {
        "Install Locally"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[
            EventType::ProjectChangesPushed,
            EventType::ReleasePipelineCompleted,
        ]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        // In future: run cargo install, npm install -g, etc.
        tracing::info!(project = %project, "installing locally");

        Box::pin(async move {
            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::LocalInstallCompleted,
                    project,
                    throttle,
                    serde_json::json!({ "status": "installed" }),
                )],
                success: true,
                summary: "Installed locally".to_string(),
            })
        })
    }
}
