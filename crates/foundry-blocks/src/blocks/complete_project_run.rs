use std::pin::Pin;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{ProjectCompletedPayload, ProjectRunCompletedPayload};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Emits a uniform `ProjectRunCompleted` terminal for a per-project run that
/// is part of a scattered maintenance cycle.
///
/// Observer — sinks on the genuine per-project terminal events
/// (`ProjectIterationCompleted`, `ProjectMaintenanceCompleted`). When the
/// trigger carries a `gather_id` — i.e. it descends from a `FanOutMaintenance`
/// scatter — the block emits a `ProjectRunCompleted` so the cycle's gather can
/// count this project's run.
///
/// Outside a scattered cycle (no `gather_id`) the block emits nothing:
/// standalone `foundry iterate` / `foundry maintain` runs, and direct
/// single-project `foundry run --project X` runs, are unaffected.
///
/// The "this project's run reached no work" terminals — validation failure or
/// no automation actions enabled — are emitted by `RouteProjectWorkflow`,
/// which already owns that decision. Between the two blocks every per-project
/// chain in a cycle emits exactly one `ProjectRunCompleted`.
pub struct CompleteProjectRun;

impl TaskBlock for CompleteProjectRun {
    task_block_meta! {
        name: "Complete Project Run",
        kind: Observer,
        sinks_on: [ProjectIterationCompleted, ProjectMaintenanceCompleted],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let in_cycle = trigger.gather_id.is_some();
        let event_type = trigger.event_type.clone();

        let p = parse_payload!(trigger, ProjectCompletedPayload);

        Box::pin(async move {
            if !in_cycle {
                return Ok(TaskBlockResult::success(
                    format!(
                        "{project}: {event_type} outside a maintenance cycle — no run terminal"
                    ),
                    vec![],
                ));
            }
            super::emit_result(
                format!("{project}: project run completed (success={})", p.success),
                EventType::ProjectRunCompleted,
                &project,
                throttle,
                &ProjectRunCompletedPayload {
                    success: p.success,
                    root_event_id: None,
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    fn terminal(event_type: EventType, success: bool, gather_id: Option<&str>) -> Event {
        Event::new(
            event_type,
            "alpha".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "alpha",
                "success": success,
                "summary": "done",
                "workflow": "iterate",
            }),
        )
        .with_gather_id(gather_id.map(str::to_string))
    }

    assert_block_meta!(
        CompleteProjectRun,
        kind: Observer,
        sinks_on: [ProjectIterationCompleted, ProjectMaintenanceCompleted],
    );

    #[tokio::test]
    async fn emits_project_run_completed_when_in_a_cycle() {
        let trigger = terminal(EventType::ProjectIterationCompleted, true, Some("gth_x"));
        let result = CompleteProjectRun.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        let emitted = &result.events[0];
        assert_eq!(emitted.event_type, EventType::ProjectRunCompleted);
        assert_eq!(emitted.project, "alpha");
        let payload: ProjectRunCompletedPayload = emitted.parse_payload().unwrap();
        assert!(payload.success);
    }

    #[tokio::test]
    async fn propagates_a_failed_terminal_as_an_unsuccessful_run() {
        let trigger = terminal(EventType::ProjectMaintenanceCompleted, false, Some("gth_x"));
        let result = CompleteProjectRun.execute(&trigger).await.unwrap();

        assert_eq!(result.events.len(), 1);
        let payload: ProjectRunCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(!payload.success);
    }

    #[tokio::test]
    async fn emits_nothing_outside_a_cycle() {
        // No gather_id — a standalone iterate/maintain run, not a cycle.
        let trigger = terminal(EventType::ProjectIterationCompleted, true, None);
        let result = CompleteProjectRun.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
    }
}
