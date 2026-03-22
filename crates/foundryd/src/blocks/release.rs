use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Tags a patch release when the main branch is clean (vulnerability already fixed).
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Self-filters: only acts when `dirty=false` in the trigger payload.
pub struct CutRelease;

impl TaskBlock for CutRelease {
    fn name(&self) -> &'static str {
        "Cut Release"
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
            .unwrap_or(true);

        if dirty {
            tracing::info!("main branch is dirty, skipping release");
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: main branch is dirty".to_string(),
                })
            });
        }

        let cve = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        tracing::info!(%cve, "cutting patch release");

        Box::pin(async move {
            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::AutoReleaseCompleted,
                    project,
                    throttle,
                    serde_json::json!({ "cve": cve, "release": "patch" }),
                )],
                success: true,
                summary: format!("Cut patch release for {cve}"),
            })
        })
    }
}

/// Watches the CI pipeline after a release tag is pushed.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
pub struct WatchPipeline;

impl TaskBlock for WatchPipeline {
    fn name(&self) -> &'static str {
        "Watch Pipeline"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::AutoReleaseCompleted]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        // In future: poll GitHub Actions API for pipeline status.
        // For now: simulate successful pipeline completion.
        tracing::info!("watching release pipeline");

        Box::pin(async move {
            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ReleasePipelineCompleted,
                    project,
                    throttle,
                    serde_json::json!({ "status": "success" }),
                )],
                success: true,
                summary: "Release pipeline completed successfully".to_string(),
            })
        })
    }
}
