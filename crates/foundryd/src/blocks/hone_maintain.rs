use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Runs `hone maintain` for a project.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Sinks on `MaintenanceRequested` only.  The routing decision (direct
/// maintain-only path via `RouteProjectWorkflow`, or post-iterate chain via
/// `RunHoneIterate`) has already been made before this event was emitted.
/// This block simply runs `hone maintain` and emits `ProjectMaintainCompleted`.
pub struct RunHoneMaintain;

impl TaskBlock for RunHoneMaintain {
    fn name(&self) -> &'static str {
        "Run Hone Maintain"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::MaintenanceRequested]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        // TODO: Look up agent and path from registry.
        // TODO: Run real command: `hone maintain <agent> <path> --json`
        // The stub unconditionally reports success so the chain can be exercised.
        tracing::info!(%project, "running hone maintain (stub)");

        Box::pin(async move {
            // TODO: Parse real hone JSON output and surface it in the payload.
            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ProjectMaintainCompleted,
                    project.clone(),
                    throttle,
                    serde_json::json!({ "project": project, "success": true }),
                )],
                success: true,
                summary: format!("{project}: hone maintain completed (stub)"),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::task_block::TaskBlock;
    use foundry_core::throttle::Throttle;

    fn maintenance_event() -> Event {
        Event::new(
            EventType::MaintenanceRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "project": "my-project" }),
        )
    }

    #[test]
    fn sinks_on_maintenance_requested_only() {
        let sinks = RunHoneMaintain.sinks_on();
        assert_eq!(sinks, &[EventType::MaintenanceRequested]);
    }

    #[test]
    fn kind_is_mutator() {
        assert_eq!(RunHoneMaintain.kind(), BlockKind::Mutator);
    }

    #[tokio::test]
    async fn emits_project_maintain_completed_on_success() {
        let trigger = maintenance_event();
        let result = RunHoneMaintain.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectMaintainCompleted);
        assert_eq!(result.events[0].project, "my-project");
        assert_eq!(
            result.events[0].payload.get("success").and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[tokio::test]
    async fn does_not_sink_on_project_validation_completed() {
        // Verify the old dual-sink path is no longer registered
        assert!(!RunHoneMaintain.sinks_on().contains(&EventType::ProjectValidationCompleted));
    }

    #[tokio::test]
    async fn does_not_sink_on_project_iterate_completed() {
        // Verify the old dual-sink path is no longer registered
        assert!(!RunHoneMaintain.sinks_on().contains(&EventType::ProjectIterateCompleted));
    }
}
