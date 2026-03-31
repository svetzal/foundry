use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType, PayloadExt};
use foundry_core::loop_context::forward_chain_context;
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Validates that a project has intent documentation before the iterate workflow proceeds.
///
/// Observer — sinks on `IterationRequested`.
/// Emits `CharterCheckCompleted` with `success: true/false`.
/// If the charter check fails, the chain stops (`ResolveGates` checks for `success=true`).
pub struct CheckCharter {
    registry: Arc<Registry>,
}

impl CheckCharter {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl TaskBlock for CheckCharter {
    task_block_meta! {
        name: "Check Charter",
        kind: Observer,
        sinks_on: [IterationRequested, PromptExecutionRequested],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let payload = trigger.payload.clone();
        let event_type = trigger.event_type.clone();

        // Self-filter: when strategic=true, StrategicAssessor handles the event instead.
        let strategic = trigger.payload_bool_or("strategic", false);
        if strategic {
            return Box::pin(async {
                Ok(TaskBlockResult::success(
                    "Skipped: strategic iteration handled by StrategicAssessor",
                    vec![],
                ))
            });
        }

        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();

        Box::pin(async move {
            let Some(entry) = entry else {
                tracing::warn!(project = %project, "project not found in registry");
                return Ok(TaskBlockResult::project_not_found(&project));
            };

            let project_path = std::path::Path::new(&entry.path);
            let result = crate::charter::check_charter(project_path);

            tracing::info!(
                project = %project,
                passed = result.passed,
                sources = ?result.sources,
                "charter check completed"
            );

            let mut event_payload = serde_json::json!({
                "project": project,
                "success": result.passed,
                "sources": result.sources,
                "guidance": result.guidance,
            });
            // Derive workflow from trigger event type, with payload override.
            let workflow = payload.str_or(
                "workflow",
                match event_type {
                    EventType::PromptExecutionRequested => "prompt",
                    _ => "iterate",
                },
            );
            event_payload["workflow"] = serde_json::json!(workflow);
            forward_chain_context(&payload, &mut event_payload);

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
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use super::CheckCharter;

    fn registry_with_project(name: &str, path: &str) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: name.to_string(),
                path: path.to_string(),
                stack: Stack::Rust,
                agent: "claude".to_string(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                notes: None,
                actions: ActionFlags::default(),
                install: None,
                timeout_secs: None,
            }],
        })
    }

    #[test]
    fn kind_is_observer() {
        let block = CheckCharter::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_expected_events() {
        let block = CheckCharter::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let sinks = block.sinks_on();
        assert!(sinks.contains(&EventType::IterationRequested));
        assert!(sinks.contains(&EventType::PromptExecutionRequested));
    }

    #[tokio::test]
    async fn passes_when_charter_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();

        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CheckCharter::new(registry);
        let trigger = Event::new(
            EventType::IterationRequested,
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

        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CheckCharter::new(registry);
        let trigger = Event::new(
            EventType::IterationRequested,
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

        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CheckCharter::new(registry);
        let trigger = Event::new(
            EventType::IterationRequested,
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
        let block = CheckCharter::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let trigger = Event::new(
            EventType::IterationRequested,
            "unknown".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "unknown"}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
    }
}
