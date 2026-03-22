use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Commits staged changes and pushes to the remote.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Shared across workflows: vulnerability remediation and maintenance both
/// use this block to persist their work.
pub struct CommitAndPush;

impl TaskBlock for CommitAndPush {
    fn name(&self) -> &'static str {
        "Commit and Push"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::RemediationCompleted]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let cve = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // In future: run git add, git commit, git push.
        tracing::info!(%cve, "committing and pushing changes");

        Box::pin(async move {
            Ok(TaskBlockResult {
                events: vec![
                    Event::new(
                        EventType::ProjectChangesCommitted,
                        project.clone(),
                        throttle,
                        serde_json::json!({ "cve": cve, "message": format!("fix: remediate {cve}") }),
                    ),
                    Event::new(
                        EventType::ProjectChangesPushed,
                        project,
                        throttle,
                        serde_json::json!({ "cve": cve }),
                    ),
                ],
                success: true,
                summary: format!("Committed and pushed fix for {cve}"),
            })
        })
    }
}
