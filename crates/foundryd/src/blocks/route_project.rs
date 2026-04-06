use std::pin::Pin;

use foundry_core::event::{Event, EventType, PayloadExt};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Routes a validated project to the correct maintenance sub-workflow.
///
/// Observer — sinks on `ProjectValidationCompleted` and emits either
/// `IterationRequested` or `MaintenanceRequested` based on the action flags
/// forwarded in the validation payload.
///
/// When validation did not succeed (`status != "ok"`) the block emits nothing,
/// stopping the chain.  When both `actions.iterate` and `actions.maintain` are
/// false the block also emits nothing (no automation is enabled for the
/// project).
pub struct RouteProjectWorkflow;

impl TaskBlock for RouteProjectWorkflow {
    task_block_meta! {
        name: "Route Project Workflow",
        kind: Observer,
        sinks_on: [ProjectValidationCompleted],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let status = trigger.payload.str_or("status", "").to_string();

        let iterate = trigger
            .payload
            .get("actions")
            .and_then(|a| a.get("iterate"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let maintain = trigger
            .payload
            .get("actions")
            .and_then(|a| a.get("maintain"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        Box::pin(async move {
            if status != "ok" {
                tracing::info!(%project, %status, "skipping routing: validation did not succeed");
                return Ok(TaskBlockResult::success(
                    format!("{project}: skipped — validation status={status}"),
                    vec![],
                ));
            }

            if iterate {
                tracing::info!(%project, "routing to iteration workflow");
                Ok(TaskBlockResult::success(
                    format!("{project}: routing to iteration workflow"),
                    vec![Event::new(
                        EventType::IterationRequested,
                        project.clone(),
                        throttle,
                        serde_json::json!({
                            "project": project,
                            "actions": { "maintain": maintain },
                        }),
                    )],
                ))
            } else if maintain {
                tracing::info!(%project, "routing to maintenance workflow");
                Ok(TaskBlockResult::success(
                    format!("{project}: routing to maintenance workflow"),
                    vec![Event::new(
                        EventType::MaintenanceRequested,
                        project.clone(),
                        throttle,
                        serde_json::json!({ "project": project }),
                    )],
                ))
            } else {
                tracing::info!(%project, "no automation actions enabled");
                Ok(TaskBlockResult::success(
                    format!("{project}: no automation actions enabled"),
                    vec![],
                ))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::task_block::TaskBlock;
    use foundry_core::throttle::Throttle;

    fn validation_event(status: &str, iterate: bool, maintain: bool) -> Event {
        Event::new(
            EventType::ProjectValidationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "status": status,
                "actions": { "iterate": iterate, "maintain": maintain },
            }),
        )
    }

    #[test]
    fn sinks_on_project_validation_completed() {
        assert_eq!(RouteProjectWorkflow.sinks_on(), &[EventType::ProjectValidationCompleted]);
    }

    #[test]
    fn kind_is_observer() {
        assert_eq!(RouteProjectWorkflow.kind(), BlockKind::Observer);
    }

    #[tokio::test]
    async fn status_ok_iterate_true_emits_iteration_requested() {
        let trigger = validation_event("ok", true, false);
        let result = RouteProjectWorkflow.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::IterationRequested);
        assert_eq!(result.events[0].project, "my-project");
        // maintain=false forwarded in payload
        let maintain = result.events[0]
            .payload
            .get("actions")
            .and_then(|a| a.get("maintain"))
            .and_then(serde_json::Value::as_bool)
            .unwrap();
        assert!(!maintain);
    }

    #[tokio::test]
    async fn status_ok_iterate_true_maintain_true_emits_iteration_requested_with_maintain_flag() {
        let trigger = validation_event("ok", true, true);
        let result = RouteProjectWorkflow.execute(&trigger).await.unwrap();

        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::IterationRequested);
        // maintain=true forwarded so iterate chain can route to maintain
        let maintain = result.events[0]
            .payload
            .get("actions")
            .and_then(|a| a.get("maintain"))
            .and_then(serde_json::Value::as_bool)
            .unwrap();
        assert!(maintain);
    }

    #[tokio::test]
    async fn status_ok_maintain_only_emits_maintenance_requested() {
        let trigger = validation_event("ok", false, true);
        let result = RouteProjectWorkflow.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::MaintenanceRequested);
        assert_eq!(result.events[0].project, "my-project");
    }

    #[tokio::test]
    async fn status_ok_no_actions_emits_nothing() {
        let trigger = validation_event("ok", false, false);
        let result = RouteProjectWorkflow.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("no automation actions enabled"));
    }

    #[tokio::test]
    async fn status_error_emits_nothing() {
        let trigger = validation_event("error", true, true);
        let result = RouteProjectWorkflow.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("validation status=error"));
    }

    #[tokio::test]
    async fn status_skipped_emits_nothing() {
        let trigger = validation_event("skipped", true, true);
        let result = RouteProjectWorkflow.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn missing_status_field_emits_nothing() {
        let trigger = Event::new(
            EventType::ProjectValidationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let result = RouteProjectWorkflow.execute(&trigger).await.unwrap();

        // Status defaults to "" which is not "ok"
        assert!(result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn missing_actions_field_treated_as_no_actions() {
        let trigger = Event::new(
            EventType::ProjectValidationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "status": "ok" }),
        );
        let result = RouteProjectWorkflow.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("no automation actions enabled"));
    }
}
