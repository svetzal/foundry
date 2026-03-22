use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Runs `hone maintain` to handle dependency updates and housekeeping.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Dual-sink self-filter logic:
/// - Triggered by `ProjectIterateCompleted`: runs if `actions.maintain == true`
/// - Triggered by `ProjectValidationCompleted`: runs only if `actions.iterate == false`
///   AND `actions.maintain == true` (when iterate is enabled, the iterate path
///   handles the chain and this block will fire from `ProjectIterateCompleted` instead)
pub struct RunHoneMaintain;

impl TaskBlock for RunHoneMaintain {
    fn name(&self) -> &'static str {
        "Run Hone Maintain"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[
            EventType::ProjectIterateCompleted,
            EventType::ProjectValidationCompleted,
        ]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let event_type = trigger.event_type.clone();

        let actions = trigger.payload.get("actions").cloned().unwrap_or_default();

        let maintain =
            actions.get("maintain").and_then(serde_json::Value::as_bool).unwrap_or(false);

        let iterate = actions.get("iterate").and_then(serde_json::Value::as_bool).unwrap_or(false);

        let agent = trigger
            .payload
            .get("agent")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default")
            .to_string();

        let path = trigger
            .payload
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(".")
            .to_string();

        let audit_dir = trigger
            .payload
            .get("audit_dir")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();

        let should_run = match event_type {
            EventType::ProjectIterateCompleted => maintain,
            EventType::ProjectValidationCompleted => !iterate && maintain,
            _ => false,
        };

        if !should_run {
            let reason = if maintain {
                "iterate is enabled — maintain will run after iterate completes"
            } else {
                "maintain is disabled"
            };
            tracing::info!(%reason, "skipping hone maintain");
            return Box::pin(async move {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: format!("Skipped: {reason}"),
                })
            });
        }

        tracing::info!(%agent, %path, "running hone maintain");

        Box::pin(async move {
            // TODO: Shell out to `hone maintain <agent> <path> --audit-dir <audit_dir> --json`
            // TODO: Parse JSON output for success/failure details
            // TODO: Handle hone not installed / not on PATH gracefully

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ProjectMaintainCompleted,
                    project,
                    throttle,
                    serde_json::json!({
                        "agent": agent,
                        "path": path,
                        "audit_dir": audit_dir,
                        "success": true,
                    }),
                )],
                success: true,
                summary: format!("hone maintain completed for {agent}"),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::throttle::Throttle;

    fn make_event(event_type: EventType, iterate: bool, maintain: bool) -> Event {
        Event::new(
            event_type,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "agent": "my-agent",
                "path": "/workspace/project",
                "audit_dir": "/workspace/audit",
                "actions": {
                    "iterate": iterate,
                    "maintain": maintain,
                }
            }),
        )
    }

    #[test]
    fn sinks_on_both_event_types() {
        let block = RunHoneMaintain;
        let sinks = block.sinks_on();
        assert!(sinks.contains(&EventType::ProjectIterateCompleted));
        assert!(sinks.contains(&EventType::ProjectValidationCompleted));
    }

    #[test]
    fn kind_is_mutator() {
        let block = RunHoneMaintain;
        assert_eq!(block.kind(), BlockKind::Mutator);
    }

    #[tokio::test]
    async fn from_iterate_completed_with_maintain_true_runs() {
        let block = RunHoneMaintain;
        let trigger = make_event(EventType::ProjectIterateCompleted, true, true);
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectMaintainCompleted);
    }

    #[tokio::test]
    async fn from_iterate_completed_with_maintain_false_skips() {
        let block = RunHoneMaintain;
        let trigger = make_event(EventType::ProjectIterateCompleted, true, false);
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("Skipped"));
    }

    #[tokio::test]
    async fn from_validation_completed_with_iterate_false_and_maintain_true_runs() {
        let block = RunHoneMaintain;
        let trigger = make_event(EventType::ProjectValidationCompleted, false, true);
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectMaintainCompleted);
    }

    #[tokio::test]
    async fn from_validation_completed_with_iterate_true_skips() {
        let block = RunHoneMaintain;
        // iterate is enabled: maintain should not run from validation path —
        // it will fire after ProjectIterateCompleted instead
        let trigger = make_event(EventType::ProjectValidationCompleted, true, true);
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("Skipped"));
    }

    #[tokio::test]
    async fn both_disabled_skips() {
        let block = RunHoneMaintain;
        let trigger = make_event(EventType::ProjectValidationCompleted, false, false);
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn emitted_event_carries_agent_and_path() {
        let block = RunHoneMaintain;
        let trigger = make_event(EventType::ProjectIterateCompleted, false, true);
        let result = block.execute(&trigger).await.unwrap();
        assert_eq!(result.events.len(), 1);
        let payload = &result.events[0].payload;
        assert_eq!(payload["agent"], "my-agent");
        assert_eq!(payload["path"], "/workspace/project");
        assert_eq!(payload["success"], true);
    }
}
