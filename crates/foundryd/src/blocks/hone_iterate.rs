use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Runs `hone iterate` for a validated project.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Sinks on `IterationRequested` (emitted by `RouteProjectWorkflow` after
/// successful validation when `actions.iterate=true`).  No action-flag
/// self-filtering needed — the router guarantees iterate is enabled.
///
/// After a successful iteration the block checks the forwarded
/// `actions.maintain` flag.  When `true` it also emits `MaintenanceRequested`
/// so the maintain sub-workflow starts automatically without an extra routing
/// step.
pub struct RunHoneIterate;

impl TaskBlock for RunHoneIterate {
    fn name(&self) -> &'static str {
        "Run Hone Iterate"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::IterationRequested]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let maintain = trigger
            .payload
            .get("actions")
            .and_then(|a| a.get("maintain"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        // TODO: Run real command: `hone iterate <agent> <path> --json`
        // For now the stub unconditionally reports success so the chain can be exercised.
        tracing::info!(%project, %maintain, "running hone iterate (stub)");

        Box::pin(async move {
            // TODO: Parse real hone JSON output and surface it in the payload.
            let mut events = vec![Event::new(
                EventType::ProjectIterateCompleted,
                project.clone(),
                throttle,
                serde_json::json!({ "project": project, "success": true }),
            )];

            if maintain {
                tracing::info!(%project, "iteration done, chaining to maintenance workflow");
                events.push(Event::new(
                    EventType::MaintenanceRequested,
                    project.clone(),
                    throttle,
                    serde_json::json!({ "project": project }),
                ));
            }

            Ok(TaskBlockResult {
                events,
                success: true,
                summary: format!("{project}: hone iterate completed (stub)"),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::task_block::TaskBlock;
    use foundry_core::throttle::Throttle;

    fn iteration_event(maintain: bool) -> Event {
        Event::new(
            EventType::IterationRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "actions": { "maintain": maintain },
            }),
        )
    }

    #[test]
    fn sinks_on_iteration_requested() {
        assert_eq!(RunHoneIterate.sinks_on(), &[EventType::IterationRequested]);
    }

    #[test]
    fn kind_is_mutator() {
        assert_eq!(RunHoneIterate.kind(), BlockKind::Mutator);
    }

    #[tokio::test]
    async fn emits_project_iterate_completed() {
        let trigger = iteration_event(false);
        let result = RunHoneIterate.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(!result.events.is_empty());
        assert_eq!(result.events[0].event_type, EventType::ProjectIterateCompleted);
        assert_eq!(result.events[0].project, "my-project");
    }

    #[tokio::test]
    async fn maintain_false_emits_only_iterate_completed() {
        let trigger = iteration_event(false);
        let result = RunHoneIterate.execute(&trigger).await.unwrap();

        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterateCompleted);
    }

    #[tokio::test]
    async fn maintain_true_also_emits_maintenance_requested() {
        let trigger = iteration_event(true);
        let result = RunHoneIterate.execute(&trigger).await.unwrap();

        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterateCompleted);
        assert_eq!(result.events[1].event_type, EventType::MaintenanceRequested);
        assert_eq!(result.events[1].project, "my-project");
    }

    #[tokio::test]
    async fn missing_actions_field_treats_maintain_as_false() {
        let trigger = Event::new(
            EventType::IterationRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "project": "my-project" }),
        );
        let result = RunHoneIterate.execute(&trigger).await.unwrap();

        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterateCompleted);
    }
}
