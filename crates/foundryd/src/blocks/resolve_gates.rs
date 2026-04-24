use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{CharterCheckCompletedPayload, GateResolutionCompletedPayload};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use super::TriggerContext;

/// Reads `.hone-gates.json` from the project directory and emits `GateResolutionCompleted`
/// with the gate definitions and workflow type.
///
/// Observer — sinks on `CharterCheckCompleted`, `MaintenanceRequested`, and `ValidationRequested`.
/// For iterate workflow: triggered by `CharterCheckCompleted` (checks `success=true`).
/// For maintain/validate workflows: triggered directly by request events.
pub struct ResolveGates {
    registry: Arc<Registry>,
}

impl ResolveGates {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl TaskBlock for ResolveGates {
    task_block_meta! {
        name: "Resolve Gates",
        kind: Observer,
        sinks_on: [CharterCheckCompleted, MaintenanceRequested, ValidationRequested],
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
            // CharterCheckCompleted: only proceed if charter passed
            if event_type == EventType::CharterCheckCompleted {
                let charter_success = charter_payload.as_ref().is_some_and(|p| p.success);
                if !charter_success {
                    tracing::info!(project = %project, "charter check failed, skipping gate resolution");
                    return Ok(TaskBlockResult::success(
                        format!("{project}: charter check failed, no gates to resolve"),
                        vec![],
                    ));
                }
            }

            // Payload workflow overrides the event-type default — this allows
            // the prompt formation to carry workflow="prompt" through CharterCheckCompleted.
            let workflow = if event_type == EventType::CharterCheckCompleted {
                charter_payload.map_or_else(|| "iterate".to_string(), |p| p.workflow)
            } else {
                match event_type {
                    EventType::MaintenanceRequested => "maintain".to_string(),
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

            let chain = foundry_core::payload::ChainContext::extract_from(&payload);
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
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::Registry;
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use super::super::test_helpers;
    use super::ResolveGates;

    #[test]
    fn kind_is_observer() {
        let block = ResolveGates::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_charter_check_maintenance_and_validation() {
        let block = ResolveGates::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let sinks = block.sinks_on();
        assert!(sinks.contains(&EventType::CharterCheckCompleted));
        assert!(sinks.contains(&EventType::MaintenanceRequested));
        assert!(sinks.contains(&EventType::ValidationRequested));
        assert!(!sinks.contains(&EventType::IterationRequested));
    }

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
    async fn charter_check_failed_returns_empty_events() {
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

        assert!(result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn missing_gates_file_emits_empty_gates() {
        let dir = tempfile::tempdir().unwrap();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ResolveGates::new(registry);
        let trigger = Event::new(
            EventType::MaintenanceRequested,
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
        let block = ResolveGates::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let trigger = Event::new(
            EventType::CharterCheckCompleted,
            "unknown".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "unknown", "success": true}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
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
