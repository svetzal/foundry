use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    ChainContext, ProjectIterationRequestedPayload, ProjectMaintenanceRequestedPayload,
    ProjectRunCompletedPayload, ProjectValidationCompletedPayload,
};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Routes a validated project to the correct maintenance sub-workflow.
///
/// Observer — sinks on `ProjectValidationCompleted` and emits either
/// `ProjectIterationRequested` or `ProjectMaintenanceRequested` based on the action flags
/// forwarded in the validation payload.
///
/// When validation did not succeed (`status != "ok"`), or when neither
/// `actions.iterate` nor `actions.maintain` is enabled, the per-project chain
/// reaches no work. In that case, if the run is part of a scattered
/// maintenance cycle (the trigger carries a `gather_id`), the block emits a
/// `ProjectRunCompleted` so the cycle's gather still counts this project.
/// Outside a cycle these no-work paths emit nothing, stopping the chain as
/// before.
pub struct RouteProjectWorkflow;

impl TaskBlock for RouteProjectWorkflow {
    task_block_meta! {
        name: "Route Project Workflow",
        kind: Observer,
        sinks_on: [ProjectValidationCompleted],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let in_cycle = trigger.gather_id.is_some();

        let p = parse_payload!(trigger, ProjectValidationCompletedPayload);
        let status = p.status.clone();
        let iterate = p
            .actions
            .as_ref()
            .and_then(|a| a.get("iterate"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let maintain = p
            .actions
            .as_ref()
            .and_then(|a| a.get("maintain"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        Box::pin(async move {
            // Domain skip: stays in execute() because it emits ProjectRunCompleted { success: false }
            // when in a cycle — a meaningful domain fact that the cycle gather must count.
            if status != "ok" {
                tracing::info!(%project, %status, "skipping routing: validation did not succeed");
                if in_cycle {
                    return super::emit_result(
                        format!("{project}: validation status={status} — project run failed"),
                        EventType::ProjectRunCompleted,
                        &project,
                        throttle,
                        &ProjectRunCompletedPayload {
                            success: false,
                            root_event_id: None,
                        },
                    );
                }
                return Ok(TaskBlockResult::success(
                    format!("{project}: skipped — validation status={status}"),
                    vec![],
                ));
            }

            if iterate {
                tracing::info!(%project, "routing to iteration workflow");
                super::emit_result(
                    format!("{project}: routing to iteration workflow"),
                    EventType::ProjectIterationRequested,
                    &project,
                    throttle,
                    &ProjectIterationRequestedPayload {
                        project: project.clone(),
                        workflow: "iterate".to_string(),
                        chain: ChainContext {
                            actions: Some(serde_json::json!({ "maintain": maintain })),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
            } else if maintain {
                tracing::info!(%project, "routing to maintenance workflow");
                super::emit_result(
                    format!("{project}: routing to maintenance workflow"),
                    EventType::ProjectMaintenanceRequested,
                    &project,
                    throttle,
                    &ProjectMaintenanceRequestedPayload {
                        project: project.clone(),
                        workflow: "maintain".to_string(),
                        chain: ChainContext::default(),
                    },
                )
            } else if in_cycle {
                tracing::info!(%project, "no automation actions enabled — completing project run");
                super::emit_result(
                    format!("{project}: no automation actions enabled — project run completed"),
                    EventType::ProjectRunCompleted,
                    &project,
                    throttle,
                    &ProjectRunCompletedPayload {
                        success: true,
                        root_event_id: None,
                    },
                )
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
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

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

    assert_block_meta!(
        RouteProjectWorkflow,
        kind: Observer,
        sinks_on: [ProjectValidationCompleted],
    );

    #[tokio::test]
    async fn status_ok_iterate_true_emits_iteration_requested() {
        let trigger = validation_event("ok", true, false);
        let result = RouteProjectWorkflow.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationRequested);
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
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationRequested);
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
        assert_eq!(result.events[0].event_type, EventType::ProjectMaintenanceRequested);
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

    fn validation_event_in_cycle(status: &str, iterate: bool, maintain: bool) -> Event {
        validation_event(status, iterate, maintain).with_gather_id(Some("gth_cycle".to_string()))
    }

    #[tokio::test]
    async fn in_cycle_status_error_emits_project_run_completed_failure() {
        let trigger = validation_event_in_cycle("error", true, true);
        let result = RouteProjectWorkflow.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectRunCompleted);
        let payload: foundry_sdk::payload::ProjectRunCompletedPayload =
            result.events[0].parse_payload().unwrap();
        assert!(!payload.success, "a failed validation completes the run unsuccessfully");
    }

    #[tokio::test]
    async fn in_cycle_no_actions_emits_project_run_completed_success() {
        let trigger = validation_event_in_cycle("ok", false, false);
        let result = RouteProjectWorkflow.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectRunCompleted);
        let payload: foundry_sdk::payload::ProjectRunCompletedPayload =
            result.events[0].parse_payload().unwrap();
        assert!(payload.success, "validated-but-no-work still completes the run");
    }

    #[tokio::test]
    async fn in_cycle_status_ok_with_actions_still_routes_normally() {
        // A gather_id present must not short-circuit a project that has work:
        // routing to the iterate/maintain sub-workflow still happens.
        let trigger = validation_event_in_cycle("ok", true, false);
        let result = RouteProjectWorkflow.execute(&trigger).await.unwrap();

        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationRequested);
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
