use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Validates that a project has intent documentation (charter) before proceeding.
///
/// Observer — sinks on `IterationRequested`.
/// Pure filesystem check — no agent invocation.
/// Emits `CharterCheckCompleted` with pass/fail and sources.
pub struct CheckCharter {
    registry: Arc<Registry>,
}

impl CheckCharter {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl TaskBlock for CheckCharter {
    fn name(&self) -> &'static str {
        "Check Charter"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer
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
        let payload = trigger.payload.clone();

        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();

        Box::pin(async move {
            let Some(entry) = entry else {
                tracing::warn!(project = %project, "project not found in registry");
                return Ok(TaskBlockResult {
                    events: vec![],
                    success: false,
                    summary: format!("Project '{project}' not found in registry"),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                });
            };

            let project_path = PathBuf::from(&entry.path);
            let charter_result = crate::charter::check_charter(&project_path);

            tracing::info!(
                project = %project,
                passed = charter_result.passed,
                sources = ?charter_result.sources,
                "charter check completed"
            );

            let mut event_payload = serde_json::json!({
                "project": project,
                "passed": charter_result.passed,
                "sources": charter_result.sources,
            });

            if let Some(ref guidance) = charter_result.guidance {
                event_payload["guidance"] = serde_json::json!(guidance);
            }

            // Forward actions from the trigger
            if let Some(actions) = payload.get("actions") {
                event_payload["actions"] = actions.clone();
            }

            let success = charter_result.passed;
            let summary = if success {
                format!(
                    "{project}: charter check passed (sources: {})",
                    charter_result.sources.join(", ")
                )
            } else {
                format!(
                    "{project}: charter check failed — {}",
                    charter_result.guidance.as_deref().unwrap_or("no intent documentation found")
                )
            };

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::CharterCheckCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
                success,
                summary,
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

    fn iteration_event(project: &str) -> Event {
        Event::new(
            EventType::IterationRequested,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "actions": {"iterate": true, "maintain": true},
            }),
        )
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
    fn sinks_on_iteration_requested() {
        let block = CheckCharter::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert_eq!(block.sinks_on(), &[EventType::IterationRequested]);
    }

    #[tokio::test]
    async fn charter_present_passes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CHARTER.md"),
            format!("# Project Charter\n\n{}", "x".repeat(100)),
        )
        .unwrap();

        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CheckCharter::new(registry);
        let trigger = iteration_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::CharterCheckCompleted);
        assert_eq!(result.events[0].payload["passed"], true);

        let sources = result.events[0].payload["sources"].as_array().unwrap();
        assert!(sources.iter().any(|s| s == "CHARTER.md"));
    }

    #[tokio::test]
    async fn charter_missing_fails() {
        let dir = tempfile::tempdir().unwrap();

        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CheckCharter::new(registry);
        let trigger = iteration_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::CharterCheckCompleted);
        assert_eq!(result.events[0].payload["passed"], false);
        assert!(result.events[0].payload["guidance"].is_string());
    }

    #[tokio::test]
    async fn forwards_actions_from_trigger() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CHARTER.md"), format!("# Charter\n\n{}", "x".repeat(100)))
            .unwrap();

        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CheckCharter::new(registry);
        let trigger = iteration_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        let actions = result.events[0].payload.get("actions").unwrap();
        assert_eq!(actions["iterate"], true);
        assert_eq!(actions["maintain"], true);
    }

    #[tokio::test]
    async fn project_not_in_registry_returns_failure() {
        let block = CheckCharter::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let trigger = iteration_event("unknown-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("not found"));
    }
}
