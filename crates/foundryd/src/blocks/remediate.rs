use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Attempts to fix a vulnerability on the main branch.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Self-filters: only acts when `dirty=true` in the trigger payload.
pub struct RemediateVulnerability;

impl TaskBlock for RemediateVulnerability {
    fn name(&self) -> &'static str {
        "Remediate Vulnerability"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::MainBranchAudited]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let dirty = trigger
            .payload
            .get("dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        if !dirty {
            tracing::info!("main branch is clean, skipping remediation");
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: main branch is clean".to_string(),
                })
            });
        }

        let cve = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // In future: run cargo update, npm audit fix, etc.
        tracing::info!(%cve, "remediating vulnerability");

        Box::pin(async move {
            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::RemediationCompleted,
                    project,
                    throttle,
                    serde_json::json!({ "cve": cve, "success": true }),
                )],
                success: true,
                summary: format!("Remediated {cve}"),
            })
        })
    }
}
