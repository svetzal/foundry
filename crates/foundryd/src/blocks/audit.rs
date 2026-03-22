use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Scans a release tag for known vulnerabilities.
/// Observer — always runs regardless of throttle.
pub struct AuditReleaseTag;

impl TaskBlock for AuditReleaseTag {
    fn name(&self) -> &'static str {
        "Audit Release Tag"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }

    fn sinks_on(&self) -> &[EventType] {
        // Future: also sink on ProjectChangesPushed for re-audit after fix,
        // once real scanning can determine vulnerability status from code.
        &[EventType::VulnerabilityDetected]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        // In future: shell out to cargo-audit, npm audit, etc.
        // For now: read vulnerability info from the trigger payload.
        let cve = trigger.payload.get("cve").and_then(|v| v.as_str()).unwrap_or("unknown");
        let vulnerable = trigger
            .payload
            .get("vulnerable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        // Forward additional payload fields for downstream blocks.
        let dirty = trigger.payload.get("dirty").and_then(serde_json::Value::as_bool);

        let cve = cve.to_string();
        tracing::info!(%cve, %vulnerable, "audited release tag");

        Box::pin(async move {
            let mut payload = serde_json::json!({ "cve": cve, "vulnerable": vulnerable });
            if let Some(dirty) = dirty {
                payload["dirty"] = serde_json::Value::Bool(dirty);
            }
            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ReleaseTagAudited,
                    project,
                    throttle,
                    payload,
                )],
                success: true,
                summary: format!("Release tag audited: {cve} vulnerable={vulnerable}"),
            })
        })
    }
}

/// Checks whether the main branch still contains a detected vulnerability.
/// Observer — always runs regardless of throttle.
///
/// Self-filters: only acts when the trigger payload has `vulnerable: true`.
/// When the release tag is not vulnerable, returns an empty result to stop the chain.
pub struct AuditMainBranch;

impl TaskBlock for AuditMainBranch {
    fn name(&self) -> &'static str {
        "Audit Main Branch"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::ReleaseTagAudited]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let vulnerable = trigger
            .payload
            .get("vulnerable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        if !vulnerable {
            tracing::info!("release tag not vulnerable, skipping main branch audit");
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: release tag not vulnerable".to_string(),
                })
            });
        }

        // In future: check if main branch has the same vulnerability.
        // For now: read dirty flag from payload, defaulting to true.
        let cve = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let dirty = trigger
            .payload
            .get("dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        tracing::info!(%cve, %dirty, "audited main branch");

        Box::pin(async move {
            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::MainBranchAudited,
                    project,
                    throttle,
                    serde_json::json!({ "cve": cve, "dirty": dirty }),
                )],
                success: true,
                summary: format!("Main branch audited: {cve} dirty={dirty}"),
            })
        })
    }
}
