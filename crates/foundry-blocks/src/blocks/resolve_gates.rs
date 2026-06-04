use std::pin::Pin;
use std::sync::{Arc, RwLock};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    CharterCheckCompletedPayload, GateResolutionCompletedPayload, ProjectCompletedPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use super::TriggerContext;

/// Reads `.hone-gates.json` from the project directory and emits `GateResolutionCompleted`
/// with the gate definitions and workflow type.
///
/// Observer — sinks on `CharterCheckCompleted`, `ProjectMaintenanceRequested`, and `ValidationRequested`.
/// For iterate workflow: triggered by `CharterCheckCompleted` (checks `success=true`).
/// For maintain/validate workflows: triggered directly by request events.
pub struct ResolveGates {
    registry: Arc<RwLock<Registry>>,
}

impl ResolveGates {
    pub fn new(registry: Arc<RwLock<Registry>>) -> Self {
        Self { registry }
    }
}

/// Build the result for a failed charter check.
///
/// For the `iterate` workflow, emits `ProjectIterationCompleted { success: false }` so
/// the trace has a truthful terminal event.  Other workflows (prompt, validate) have no
/// matching terminal event type, so the chain stops silently for those.
fn charter_failure_result(
    project: &str,
    workflow: &str,
    throttle: foundry_sdk::throttle::Throttle,
) -> TaskBlockResult {
    tracing::info!(%project, %workflow, "charter check failed, skipping gate resolution");
    if workflow == "iterate" {
        return super::emit_event_result(
            format!("{project}: charter check failed, no gates to resolve"),
            false,
            EventType::ProjectIterationCompleted,
            project,
            throttle,
            &ProjectCompletedPayload {
                project: project.to_string(),
                success: false,
                summary: "charter check failed".to_string(),
                workflow: workflow.to_string(),
                loop_context: None,
                changes: None,
            },
        )
        .expect("ProjectCompletedPayload is infallibly serializable");
    }
    TaskBlockResult::success(
        format!("{project}: charter check failed, no gates to resolve"),
        vec![],
    )
}

impl TaskBlock for ResolveGates {
    task_block_meta! {
        name: "Resolve Gates",
        kind: Observer,
        sinks_on: [CharterCheckCompleted, ProjectMaintenanceRequested, ValidationRequested],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let TriggerContext {
            project,
            throttle,
            payload,
        } = TriggerContext::from_trigger(trigger);
        let event_type = trigger.event_type.clone();

        // Parse typed payload for CharterCheckCompleted before entering the async block.
        let charter_payload = if event_type == EventType::CharterCheckCompleted {
            trigger.parse_payload::<CharterCheckCompletedPayload>().ok()
        } else {
            None
        };

        let entry = require_project!(self, project);

        Box::pin(async move {
            // CharterCheckCompleted: only proceed if charter passed.
            if event_type == EventType::CharterCheckCompleted {
                let charter_success = charter_payload.as_ref().is_some_and(|p| p.success);
                if !charter_success {
                    let workflow = charter_payload
                        .as_ref()
                        .map_or_else(|| "iterate".to_string(), |p| p.workflow.clone());
                    return Ok(charter_failure_result(&project, &workflow, throttle));
                }
            }

            // Payload workflow overrides the event-type default — this allows
            // the prompt formation to carry workflow="prompt" through CharterCheckCompleted.
            let workflow = if event_type == EventType::CharterCheckCompleted {
                charter_payload.map_or_else(|| "iterate".to_string(), |p| p.workflow)
            } else {
                match event_type {
                    EventType::ProjectMaintenanceRequested => "maintain".to_string(),
                    EventType::ValidationRequested => "validate".to_string(),
                    _ => "unknown".to_string(),
                }
            };

            let project_path = std::path::Path::new(&entry.path);
            let gates = crate::gate_file::read_gates(project_path)?;

            let gates_json: Vec<serde_json::Value> = gates
                .iter()
                .map(|g| {
                    let mut val = serde_json::json!({
                        "name": g.name,
                        "command": g.command,
                        "required": g.required,
                    });
                    if let Some(timeout) = g.timeout {
                        val["timeout_secs"] = serde_json::json!(timeout.as_secs());
                    }
                    val
                })
                .collect();

            tracing::info!(
                project = %project,
                workflow = workflow,
                gate_count = gates.len(),
                "gates resolved"
            );

            let chain = foundry_sdk::payload::ChainContext::extract_from(&payload);
            super::emit_result(
                format!("{project}: resolved {} gates for {workflow} workflow", gates.len()),
                EventType::GateResolutionCompleted,
                &project,
                throttle,
                &GateResolutionCompletedPayload {
                    project: project.clone(),
                    workflow: workflow.clone(),
                    gates: serde_json::json!(gates_json),
                    chain,
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::Registry;
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use super::super::test_helpers;
    use super::ResolveGates;

    assert_block_meta!(
        ResolveGates::new(Arc::new(RwLock::new(Registry { version: 2, projects: vec![] }))),
        kind: Observer,
        sinks_on: [CharterCheckCompleted, ProjectMaintenanceRequested, ValidationRequested],
    );

    #[tokio::test]
    async fn resolves_gates_from_file_on_charter_check_completed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
        )
        .unwrap();

        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ResolveGates::new(registry);
        let trigger = Event::new(
            EventType::CharterCheckCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "my-project", "success": true}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::GateResolutionCompleted);
        let gates = result.events[0].payload.get("gates").unwrap().as_array().unwrap();
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0]["name"], "fmt");
        assert_eq!(result.events[0].payload["workflow"], "iterate");
    }

    #[tokio::test]
    async fn charter_check_failed_emits_terminal_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
        )
        .unwrap();

        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ResolveGates::new(registry);
        let trigger = Event::new(
            EventType::CharterCheckCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "my-project", "success": false}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success, "charter failure should produce a failing result");
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        assert_eq!(result.events[0].payload["success"], false);
    }

    #[tokio::test]
    async fn missing_gates_file_emits_empty_gates() {
        let dir = tempfile::tempdir().unwrap();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ResolveGates::new(registry);
        let trigger = Event::new(
            EventType::ProjectMaintenanceRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "my-project"}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::GateResolutionCompleted);
        let gates = result.events[0].payload.get("gates").unwrap().as_array().unwrap();
        assert!(gates.is_empty());
        assert_eq!(result.events[0].payload["workflow"], "maintain");
    }

    #[tokio::test]
    async fn project_not_in_registry_returns_failure() {
        let block = ResolveGates::new(Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        })));
        test_helpers::assert_missing_project_fails(&block, EventType::CharterCheckCompleted).await;
    }

    #[tokio::test]
    async fn validation_requested_resolves_gates_with_validate_workflow() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
        )
        .unwrap();

        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ResolveGates::new(registry);
        let trigger = Event::new(
            EventType::ValidationRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "my-project"}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::GateResolutionCompleted);
        assert_eq!(result.events[0].payload["workflow"], "validate");
        let gates = result.events[0].payload.get("gates").unwrap().as_array().unwrap();
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0]["name"], "fmt");
    }

    #[tokio::test]
    async fn forwards_actions_from_trigger_payload() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hone-gates.json"), r#"{"gates":[]}"#).unwrap();

        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ResolveGates::new(registry);
        let trigger = Event::new(
            EventType::CharterCheckCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "my-project", "success": true, "actions": {"maintain": true}}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let actions = result.events[0].payload.get("actions").unwrap();
        assert_eq!(actions["maintain"], true);
    }
}
