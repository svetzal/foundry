use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::loop_context::forward_loop_context;
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

/// Runs quality gates after the execution phase to verify changes.
///
/// Observer — sinks on `ExecutionCompleted`.
/// Re-reads `.hone-gates.json` from the project directory (does not rely on
/// earlier resolution) and runs all gates, emitting `GateVerificationCompleted`.
pub struct RunVerifyGates {
    registry: Arc<Registry>,
    shell: Arc<dyn ShellGateway>,
}

impl RunVerifyGates {
    pub fn new(shell: Arc<dyn ShellGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, shell }
    }

    #[cfg(test)]
    fn with_shell(shell: Arc<dyn ShellGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, shell }
    }
}

impl TaskBlock for RunVerifyGates {
    task_block_meta! {
        name: "Run Verify Gates",
        kind: Observer,
        sinks_on: [ExecutionCompleted],
    }

    #[allow(clippy::too_many_lines)]
    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let payload = trigger.payload.clone();

        let retry_count = trigger.payload_u64_or("retry_count", 0);

        let workflow = trigger.payload_str_or("workflow", "unknown").to_string();

        let entry = self.registry.find_project(&project).cloned();
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let Some(entry) = entry else {
                return Ok(super::project_not_found_result(&project));
            };

            let project_path = std::path::Path::new(&entry.path);

            // Re-read gates from disk (not from earlier resolution)
            let gates = crate::gate_file::read_gates(project_path)?;

            if gates.is_empty() {
                tracing::info!(project = %project, "no gates defined, verification passes");

                let mut event_payload = serde_json::json!({
                    "project": project,
                    "workflow": workflow,
                    "all_passed": true,
                    "required_passed": true,
                    "retry_count": retry_count,
                    "results": [],
                });
                if let Some(actions) = payload.get("actions") {
                    event_payload["actions"] = actions.clone();
                }
                forward_loop_context(&payload, &mut event_payload);

                return Ok(TaskBlockResult::success(
                    format!("{project}: no gates defined, verification passes"),
                    vec![Event::new(
                        EventType::GateVerificationCompleted,
                        project.clone(),
                        throttle,
                        event_payload,
                    )],
                ));
            }

            let run_result =
                crate::gate_runner::run_gates(&gates, project_path, shell.as_ref()).await?;

            let results_json: Vec<serde_json::Value> = run_result
                .results
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "name": r.name,
                        "command": r.command,
                        "passed": r.passed,
                        "required": r.required,
                        "output": r.output,
                        "exit_code": r.exit_code,
                    })
                })
                .collect();

            let success = run_result.all_passed;

            tracing::info!(
                project = %project,
                all_passed = run_result.all_passed,
                required_passed = run_result.required_passed,
                retry_count = retry_count,
                "gate verification completed"
            );

            let mut event_payload = serde_json::json!({
                "project": project,
                "workflow": workflow,
                "all_passed": run_result.all_passed,
                "required_passed": run_result.required_passed,
                "retry_count": retry_count,
                "results": results_json,
            });
            if let Some(actions) = payload.get("actions") {
                event_payload["actions"] = actions.clone();
            }
            forward_loop_context(&payload, &mut event_payload);

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::GateVerificationCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
                success,
                summary: if success {
                    format!("{project}: gate verification passed")
                } else {
                    format!("{project}: gate verification failed")
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

    use crate::gateway::fakes::FakeShellGateway;

    use super::RunVerifyGates;

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

    fn execution_completed_event(project: &str, retry_count: u64, workflow: &str) -> Event {
        Event::new(
            EventType::ExecutionCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "retry_count": retry_count,
                "workflow": workflow,
            }),
        )
    }

    #[test]
    fn kind_is_observer() {
        let shell = FakeShellGateway::success();
        let block = RunVerifyGates::new(
            shell,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_execution_completed() {
        let shell = FakeShellGateway::success();
        let block = RunVerifyGates::new(
            shell,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.sinks_on(), &[EventType::ExecutionCompleted]);
    }

    #[tokio::test]
    async fn passes_when_all_gates_succeed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
        )
        .unwrap();

        let shell = FakeShellGateway::success();
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunVerifyGates::with_shell(shell, registry);
        let trigger = execution_completed_event("my-project", 0, "iterate");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::GateVerificationCompleted);
        assert_eq!(result.events[0].payload["all_passed"], true);
        assert_eq!(result.events[0].payload["retry_count"], 0);
    }

    #[tokio::test]
    async fn fails_when_gate_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"test","command":"cargo test","required":true}]}"#,
        )
        .unwrap();

        let shell = FakeShellGateway::failure("test failed");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunVerifyGates::with_shell(shell, registry);
        let trigger = execution_completed_event("my-project", 1, "iterate");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events[0].payload["all_passed"], false);
        assert_eq!(result.events[0].payload["retry_count"], 1);
    }

    #[tokio::test]
    async fn includes_retry_count_from_payload() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
        )
        .unwrap();

        let shell = FakeShellGateway::success();
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunVerifyGates::with_shell(shell, registry);
        let trigger = execution_completed_event("my-project", 2, "iterate");

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events[0].payload["retry_count"], 2);
    }

    #[tokio::test]
    async fn no_gates_file_emits_success() {
        let dir = tempfile::tempdir().unwrap();
        // No .hone-gates.json written

        let shell = FakeShellGateway::success();
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunVerifyGates::with_shell(shell, registry);
        let trigger = execution_completed_event("my-project", 0, "iterate");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["all_passed"], true);
    }
}
