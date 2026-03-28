use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use foundry_core::event::{Event, EventType};
use foundry_core::gates::GateDefinition;
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

/// Runs preflight quality gates before the main execution phase.
///
/// Observer — sinks on `GatesResolved`.
/// Only runs gates when `workflow == "iterate"`; maintenance workflows skip
/// preflight and immediately emit `PreflightCompleted` with `all_passed: true`.
pub struct RunPreflightGates {
    registry: Arc<Registry>,
    shell: Arc<dyn ShellGateway>,
}

impl RunPreflightGates {
    pub fn new(shell: Arc<dyn ShellGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, shell }
    }

    #[cfg(test)]
    fn with_shell(registry: Arc<Registry>, shell: Arc<dyn ShellGateway>) -> Self {
        Self { registry, shell }
    }
}

impl TaskBlock for RunPreflightGates {
    fn name(&self) -> &'static str {
        "Run Preflight Gates"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::GatesResolved]
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

        let workflow = payload
            .get("workflow")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            // Maintain workflows skip preflight
            if workflow != "iterate" {
                tracing::info!(project = %project, workflow = %workflow, "skipping preflight for non-iterate workflow");

                let mut event_payload = serde_json::json!({
                    "project": project,
                    "workflow": workflow,
                    "all_passed": true,
                    "required_passed": true,
                    "skipped": true,
                    "results": [],
                });
                if let Some(actions) = payload.get("actions") {
                    event_payload["actions"] = actions.clone();
                }
                if let Some(gates) = payload.get("gates") {
                    event_payload["gates"] = gates.clone();
                }

                return Ok(TaskBlockResult {
                    events: vec![Event::new(
                        EventType::PreflightCompleted,
                        project.clone(),
                        throttle,
                        event_payload,
                    )],
                    success: true,
                    summary: format!("{project}: preflight skipped for {workflow} workflow"),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                });
            }

            // Parse gate definitions from payload
            let gates = parse_gates_from_payload(&payload);

            // No gates defined — emit success
            if gates.is_empty() {
                tracing::info!(project = %project, "no gates defined, preflight passes");

                let mut event_payload = serde_json::json!({
                    "project": project,
                    "workflow": workflow,
                    "all_passed": true,
                    "required_passed": true,
                    "results": [],
                });
                if let Some(actions) = payload.get("actions") {
                    event_payload["actions"] = actions.clone();
                }

                return Ok(TaskBlockResult {
                    events: vec![Event::new(
                        EventType::PreflightCompleted,
                        project.clone(),
                        throttle,
                        event_payload,
                    )],
                    success: true,
                    summary: format!("{project}: no gates defined, preflight passes"),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                });
            }

            let Some(entry) = entry else {
                tracing::warn!(project = %project, "project not in registry");
                return Ok(TaskBlockResult {
                    events: vec![],
                    success: false,
                    summary: format!("Project '{project}' not found in registry"),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                });
            };

            let working_dir = std::path::PathBuf::from(&entry.path);
            let run_result =
                crate::gate_runner::run_gates(&gates, &working_dir, shell.as_ref()).await?;

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

            let mut event_payload = serde_json::json!({
                "project": project,
                "workflow": workflow,
                "all_passed": run_result.all_passed,
                "required_passed": run_result.required_passed,
                "results": results_json,
            });
            if let Some(actions) = payload.get("actions") {
                event_payload["actions"] = actions.clone();
            }

            let success = run_result.all_passed;

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::PreflightCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
                success,
                summary: if success {
                    format!("{project}: preflight gates passed")
                } else {
                    format!("{project}: preflight gates failed")
                },
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
    }
}

/// Parse gate definitions from a `GatesResolved` event payload.
fn parse_gates_from_payload(payload: &serde_json::Value) -> Vec<GateDefinition> {
    let Some(gates_array) = payload.get("gates").and_then(serde_json::Value::as_array) else {
        return vec![];
    };

    gates_array
        .iter()
        .filter_map(|g| {
            let name = g.get("name")?.as_str()?.to_string();
            let command = g.get("command")?.as_str()?.to_string();
            let required = g.get("required")?.as_bool()?;
            let timeout = g
                .get("timeout_secs")
                .and_then(serde_json::Value::as_u64)
                .map(Duration::from_secs);
            Some(GateDefinition {
                name,
                command,
                required,
                timeout,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeShellGateway;

    use super::RunPreflightGates;

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

    fn gates_resolved_event(project: &str, workflow: &str, gates: serde_json::Value) -> Event {
        Event::new(
            EventType::GatesResolved,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "workflow": workflow,
                "gates": gates,
            }),
        )
    }

    #[test]
    fn kind_is_observer() {
        let shell = FakeShellGateway::success();
        let block = RunPreflightGates::new(
            shell,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_gates_resolved() {
        let shell = FakeShellGateway::success();
        let block = RunPreflightGates::new(
            shell,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.sinks_on(), &[EventType::GatesResolved]);
    }

    #[tokio::test]
    async fn skips_preflight_for_maintain_workflow() {
        let shell = FakeShellGateway::success();
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![],
        });
        let block = RunPreflightGates::with_shell(registry, shell.clone());
        let trigger = gates_resolved_event(
            "my-project",
            "maintain",
            serde_json::json!([{"name": "fmt", "command": "cargo fmt", "required": true}]),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::PreflightCompleted);
        assert_eq!(result.events[0].payload["skipped"], true);
        // Shell should NOT have been invoked
        assert!(shell.invocations().is_empty());
    }

    #[tokio::test]
    async fn runs_gates_for_iterate_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let shell = FakeShellGateway::success();
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunPreflightGates::with_shell(registry, shell.clone());
        let trigger = gates_resolved_event(
            "my-project",
            "iterate",
            serde_json::json!([{"name": "fmt", "command": "cargo fmt --check", "required": true}]),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::PreflightCompleted);
        assert_eq!(result.events[0].payload["all_passed"], true);
        assert!(!shell.invocations().is_empty());
    }

    #[tokio::test]
    async fn reports_failure_when_gate_fails() {
        let dir = tempfile::tempdir().unwrap();
        let shell = FakeShellGateway::failure("check failed");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunPreflightGates::with_shell(registry, shell);
        let trigger = gates_resolved_event(
            "my-project",
            "iterate",
            serde_json::json!([{"name": "fmt", "command": "cargo fmt --check", "required": true}]),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events[0].payload["all_passed"], false);
    }

    #[tokio::test]
    async fn no_gates_emits_success() {
        let shell = FakeShellGateway::success();
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![],
        });
        let block = RunPreflightGates::with_shell(registry, shell);
        let trigger = gates_resolved_event("my-project", "iterate", serde_json::json!([]));

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["all_passed"], true);
    }
}
