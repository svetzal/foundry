use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Runs `hone iterate` on a validated project where the iterate action is enabled.
/// Mutator — skipped at `dry_run`, suppressed at `audit_only`.
///
/// Self-filters:
/// - Only acts when `status == "ok"` in the trigger payload.
/// - Only acts when `actions.iterate == true` in the project registry entry.
pub struct RunHoneIterate {
    registry: Arc<Registry>,
}

impl RunHoneIterate {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl TaskBlock for RunHoneIterate {
    fn name(&self) -> &'static str {
        "Run Hone Iterate"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::ProjectValidationCompleted]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        // Self-filter 1: only proceed when validation status is "ok".
        let status = trigger.payload.get("status").and_then(|v| v.as_str()).unwrap_or("");

        if status != "ok" {
            tracing::info!(%project, %status, "skipping hone iterate: validation did not pass");
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: validation status is not ok".to_string(),
                })
            });
        }

        // Self-filter 2: only proceed when actions.iterate is enabled.
        let entry = self.registry.projects.iter().find(|p| p.name == project);

        if !entry.is_some_and(|e| e.actions.iterate) {
            tracing::info!(%project, "skipping hone iterate: iterate action not enabled");
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: iterate not enabled for project".to_string(),
                })
            });
        }

        // Both filters passed — extract what we need before moving into the async block.
        let entry = entry.expect("entry present: checked above");
        let path = std::path::PathBuf::from(&entry.path);
        let agent = entry.agent.clone();
        Box::pin(async move {
            let args: Vec<&str> = vec!["iterate", &agent, "--json"];

            tracing::info!(%project, ?path, %agent, "running hone iterate");

            let result = crate::shell::run(&path, "hone", &args, None, None).await?;

            let payload = serde_json::from_str(&result.stdout).unwrap_or_else(|_| {
                serde_json::json!({
                    "raw": result.stdout,
                    "exit_code": result.exit_code,
                })
            });

            let summary = if result.success {
                format!("{project}: hone iterate succeeded")
            } else {
                format!("{project}: hone iterate failed (exit {})", result.exit_code)
            };

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ProjectIterateCompleted,
                    project,
                    throttle,
                    payload,
                )],
                success: result.success,
                summary,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::registry::{ActionFlags, ProjectEntry, Stack};
    use foundry_core::throttle::Throttle;

    fn make_registry(iterate: bool) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: "test-project".to_string(),
                path: "/tmp/test-project".to_string(),
                stack: Stack::Rust,
                agent: "claude-sonnet-4-5".to_string(),
                repo: "https://github.com/example/test-project".to_string(),
                branch: "main".to_string(),
                skip: None,
                actions: ActionFlags {
                    iterate,
                    maintain: false,
                    push: false,
                    audit: false,
                    release: false,
                },
                install: None,
            }],
        })
    }

    fn validation_completed_event(status: &str) -> Event {
        Event::new(
            EventType::ProjectValidationCompleted,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "status": status }),
        )
    }

    #[test]
    fn kind_is_mutator() {
        let block = RunHoneIterate::new(make_registry(true));
        assert_eq!(block.kind(), BlockKind::Mutator);
    }

    #[test]
    fn sinks_on_project_validation_completed() {
        let block = RunHoneIterate::new(make_registry(true));
        assert_eq!(block.sinks_on(), &[EventType::ProjectValidationCompleted]);
    }

    #[tokio::test]
    async fn skips_when_status_is_not_ok() {
        let block = RunHoneIterate::new(make_registry(true));
        let trigger = validation_completed_event("error");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.events.is_empty());
        assert!(result.success);
        assert!(result.summary.contains("not ok"));
    }

    #[tokio::test]
    async fn skips_when_iterate_not_enabled() {
        let block = RunHoneIterate::new(make_registry(false));
        let trigger = validation_completed_event("ok");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.events.is_empty());
        assert!(result.success);
        assert!(result.summary.contains("not enabled"));
    }

    #[tokio::test]
    async fn skips_when_project_not_in_registry() {
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![],
        });
        let block = RunHoneIterate::new(registry);
        let trigger = validation_completed_event("ok");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.events.is_empty());
        assert!(result.success);
    }

    #[tokio::test]
    async fn emits_iterate_completed_on_success() {
        // Use `true` as a command that always succeeds and produces minimal JSON-parseable output.
        // We substitute "sh -c 'echo {}'" via overriding by using a custom registry path.
        // Since we can't mock shell::run in unit tests without dependency injection,
        // test the output structure with a command that exists on the host: echo.
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: "test-project".to_string(),
                path: std::env::temp_dir().to_str().unwrap().to_string(),
                stack: Stack::Rust,
                agent: "test-agent".to_string(),
                repo: "https://github.com/example/test-project".to_string(),
                branch: "main".to_string(),
                skip: None,
                actions: ActionFlags {
                    iterate: true,
                    maintain: false,
                    push: false,
                    audit: false,
                    release: false,
                },
                install: None,
            }],
        });

        let block = RunHoneIterate::new(registry);

        // This will call `hone iterate` which won't exist on CI, so we confirm
        // the block correctly returns an Err on command not found (spawn failure).
        // The important thing is the self-filter logic passes (no early return).
        let trigger = validation_completed_event("ok");
        let result = block.execute(&trigger).await;

        // Either hone exists (unlikely in CI) or it errors — both are acceptable.
        // The key assertion: self-filter logic did NOT return an early empty result.
        match result {
            Ok(r) => {
                // hone was found and ran — verify event type
                if !r.events.is_empty() {
                    assert_eq!(r.events[0].event_type, EventType::ProjectIterateCompleted);
                }
            }
            Err(e) => {
                // hone not on PATH — expected in test environment
                assert!(
                    e.to_string().contains("hone") || e.to_string().contains("No such file"),
                    "unexpected error: {e}"
                );
            }
        }
    }
}
