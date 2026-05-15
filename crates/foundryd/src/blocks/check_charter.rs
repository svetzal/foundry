use std::pin::Pin;
use std::sync::{Arc, RwLock};

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{
    ChainContext, CharterCheckCompletedPayload, ProjectIterationRequestedPayload,
};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use super::TriggerContext;

/// Validates that a project has intent documentation before the iterate workflow proceeds.
///
/// Observer — sinks on `ProjectIterationRequested`.
/// Emits `CharterCheckCompleted` with `success: true/false`.
/// If the charter check fails, the chain stops (`ResolveGates` checks for `success=true`).
pub struct CheckCharter {
    registry: Arc<RwLock<Registry>>,
}

impl CheckCharter {
    pub fn new(registry: Arc<RwLock<Registry>>) -> Self {
        Self { registry }
    }
}

impl TaskBlock for CheckCharter {
    task_block_meta! {
        name: "Check Charter",
        kind: Observer,
        sinks_on: [ProjectIterationRequested, PromptExecutionRequested],
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

        // Self-filter: when strategic=true, StrategicAssessor handles the event instead.
        // Use .ok() — this block sinks on multiple event types with different payload
        // shapes (ProjectIterationRequested and PromptExecutionRequested).
        let iter_payload = trigger.parse_payload::<ProjectIterationRequestedPayload>().ok();
        let strategic = iter_payload.as_ref().and_then(|p| p.strategic).unwrap_or(false);
        if strategic {
            return skip!("Skipped: strategic iteration handled by StrategicAssessor");
        }

        // Derive workflow from typed payload if available; fall back to event type.
        let workflow = iter_payload.as_ref().map_or_else(
            || match event_type {
                EventType::PromptExecutionRequested => "prompt".to_string(),
                _ => "iterate".to_string(),
            },
            |p| p.workflow.clone(),
        );

        let entry = require_project!(self, project);

        Box::pin(async move {
            let project_path = std::path::Path::new(&entry.path);
            let result = crate::charter::check_charter(project_path);

            tracing::info!(
                project = %project,
                passed = result.passed,
                sources = ?result.sources,
                "charter check completed"
            );

            let sources_json: Vec<serde_json::Value> =
                result.sources.iter().map(|s| serde_json::json!(s)).collect();
            let chain = ChainContext::extract_from(&payload);
            let event_payload = Event::serialize_payload(&CharterCheckCompletedPayload {
                project: project.clone(),
                success: result.passed,
                sources: sources_json,
                guidance: result.guidance.clone(),
                workflow,
                chain,
            })?;

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::CharterCheckCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
                success: result.passed,
                summary: if result.passed {
                    format!("{project}: charter validated from {}", result.sources.join(", "))
                } else {
                    format!("{project}: charter check failed — {}", result.guidance)
                },
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::Registry;
    use foundry_core::task_block::TaskBlock;
    use foundry_core::throttle::Throttle;

    use super::super::test_helpers;
    use super::CheckCharter;

    assert_block_meta!(
        CheckCharter::new(Arc::new(RwLock::new(Registry { version: 2, projects: vec![] }))),
        kind: Observer,
        sinks_on: [ProjectIterationRequested, PromptExecutionRequested],
    );

    #[tokio::test]
    async fn passes_when_charter_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();

        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CheckCharter::new(registry);
        let trigger = Event::new(
            EventType::ProjectIterationRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "my-project"}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::CharterCheckCompleted);
        assert_eq!(result.events[0].payload["success"], true);
    }

    #[tokio::test]
    async fn fails_when_no_charter() {
        let dir = tempfile::tempdir().unwrap();

        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CheckCharter::new(registry);
        let trigger = Event::new(
            EventType::ProjectIterationRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "my-project"}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::CharterCheckCompleted);
        assert_eq!(result.events[0].payload["success"], false);
    }

    #[tokio::test]
    async fn forwards_actions_from_payload() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();

        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CheckCharter::new(registry);
        let trigger = Event::new(
            EventType::ProjectIterationRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "my-project", "actions": {"maintain": true}}),
        );

        let result = block.execute(&trigger).await.unwrap();

        let actions = result.events[0].payload.get("actions").unwrap();
        assert_eq!(actions["maintain"], true);
    }

    #[tokio::test]
    async fn project_not_in_registry_returns_failure() {
        let block = CheckCharter::new(Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        })));
        test_helpers::assert_missing_project_fails(&block, EventType::ProjectIterationRequested)
            .await;
    }
}
