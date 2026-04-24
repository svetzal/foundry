use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use foundry_core::event::{Event, EventType};
use foundry_core::gates::GateDefinition;
use foundry_core::payload::{
    ChainContext, GateResolutionCompletedPayload, PreflightCompletedPayload,
};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::workflow::WorkflowType;

use crate::gateway::ShellGateway;

use super::TriggerContext;

/// Runs preflight quality gates before the main execution phase.
///
/// Observer — sinks on `GateResolutionCompleted`.
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
    fn with_shell(shell: Arc<dyn ShellGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, shell }
    }
}

impl TaskBlock for RunPreflightGates {
    task_block_meta! {
        name: "Run Preflight Gates",
        kind: Observer,
        sinks_on: [GateResolutionCompleted],
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

        let p = parse_payload!(trigger, GateResolutionCompletedPayload);
        let workflow = WorkflowType::from_payload(&payload);

        let registry = Arc::clone(&self.registry);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let chain = ChainContext::extract_from(&payload);

            // Maintain workflows skip preflight; iterate and validate run gates
            if workflow != WorkflowType::Iterate && workflow != WorkflowType::Validate {
                tracing::info!(project = %project, workflow = %workflow, "skipping preflight for non-iterate/validate workflow");

                return super::emit_result(
                    format!("{project}: preflight skipped for {workflow} workflow"),
                    EventType::PreflightCompleted,
                    &project,
                    throttle,
                    &PreflightCompletedPayload {
                        project: project.clone(),
                        workflow: workflow.to_string(),
                        all_passed: true,
                        required_passed: true,
                        skipped: Some(true),
                        results: vec![],
                        chain,
                    },
                );
            }

            // Parse gate definitions from typed payload
            let gates = parse_gates_from_value(p.gates.as_array());

            // No gates defined — emit success
            if gates.is_empty() {
                tracing::info!(project = %project, "no gates defined, preflight passes");

                return super::emit_result(
                    format!("{project}: no gates defined, preflight passes"),
                    EventType::PreflightCompleted,
                    &project,
                    throttle,
                    &PreflightCompletedPayload {
                        project: project.clone(),
                        workflow: workflow.to_string(),
                        all_passed: true,
                        required_passed: true,
                        skipped: None,
                        results: vec![],
                        chain,
                    },
                );
            }

            let entry = match super::require_project(&registry, &project) {
                Ok(e) => e,
                Err(result) => return Ok(result),
            };

            let working_dir = std::path::PathBuf::from(&entry.path);
            let run_result =
                crate::gate_runner::run_gates(&gates, &working_dir, shell.as_ref()).await?;

            Ok(build_preflight_result(&project, workflow, &run_result, chain, throttle))
        })
    }
}

fn build_preflight_result(
    project: &str,
    workflow: WorkflowType,
    run_result: &foundry_core::gates::GatesRunResult,
    chain: ChainContext,
    throttle: foundry_core::throttle::Throttle,
) -> TaskBlockResult {
    let results = super::gate_results_to_json(&run_result.results);
    let event_payload = Event::serialize_payload(&PreflightCompletedPayload {
        project: project.to_string(),
        workflow: workflow.to_string(),
        all_passed: run_result.all_passed,
        required_passed: run_result.required_passed,
        skipped: None,
        results,
        chain,
    })
    .expect("PreflightCompletedPayload is infallibly serializable");

    let success = run_result.required_passed;
    super::build_gate_block_result(
        project,
        EventType::PreflightCompleted,
        success,
        "preflight gates",
        throttle,
        event_payload,
    )
}

/// Parse gate definitions from a gates array value.
fn parse_gates_from_value(gates_array: Option<&Vec<serde_json::Value>>) -> Vec<GateDefinition> {
    let Some(gates_array) = gates_array else {
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
    use foundry_core::registry::Registry;
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeShellGateway;

    use super::super::test_helpers;
    use super::RunPreflightGates;

    fn gate_resolution_completed_event(
        project: &str,
        workflow: &str,
        gates: &serde_json::Value,
    ) -> Event {
        Event::new(
            EventType::GateResolutionCompleted,
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
    fn sinks_on_gate_resolution_completed() {
        let shell = FakeShellGateway::success();
        let block = RunPreflightGates::new(
            shell,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.sinks_on(), &[EventType::GateResolutionCompleted]);
    }

    #[tokio::test]
    async fn skips_preflight_for_maintain_workflow() {
        let shell = FakeShellGateway::success();
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![],
        });
        let block = RunPreflightGates::with_shell(shell.clone(), registry);
        let trigger = gate_resolution_completed_event(
            "my-project",
            "maintain",
            &serde_json::json!([{"name": "fmt", "command": "cargo fmt", "required": true}]),
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
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunPreflightGates::with_shell(shell.clone(), registry);
        let trigger = gate_resolution_completed_event(
            "my-project",
            "iterate",
            &serde_json::json!([{"name": "fmt", "command": "cargo fmt --check", "required": true}]),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::PreflightCompleted);
        assert_eq!(result.events[0].payload["all_passed"], true);
        assert!(!shell.invocations().is_empty());
    }

    #[tokio::test]
    async fn runs_gates_for_validate_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let shell = FakeShellGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunPreflightGates::with_shell(shell.clone(), registry);
        let trigger = gate_resolution_completed_event(
            "my-project",
            "validate",
            &serde_json::json!([{"name": "fmt", "command": "cargo fmt --check", "required": true}]),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::PreflightCompleted);
        assert_eq!(result.events[0].payload["all_passed"], true);
        assert_eq!(result.events[0].payload["workflow"], "validate");
        assert!(!shell.invocations().is_empty());
    }

    #[tokio::test]
    async fn reports_failure_when_gate_fails() {
        let dir = tempfile::tempdir().unwrap();
        let shell = FakeShellGateway::failure("check failed");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunPreflightGates::with_shell(shell, registry);
        let trigger = gate_resolution_completed_event(
            "my-project",
            "iterate",
            &serde_json::json!([{"name": "fmt", "command": "cargo fmt --check", "required": true}]),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events[0].payload["all_passed"], false);
    }

    #[tokio::test]
    async fn optional_gate_failure_still_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let shell = FakeShellGateway::sequence(vec![
            crate::shell::CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            crate::shell::CommandResult {
                stdout: String::new(),
                stderr: "optional lint warning".to_string(),
                exit_code: 1,
                success: false,
            },
        ]);
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunPreflightGates::with_shell(shell, registry);
        let trigger = gate_resolution_completed_event(
            "my-project",
            "iterate",
            &serde_json::json!([
                {"name": "fmt", "command": "cargo fmt --check", "required": true},
                {"name": "lint-optional", "command": "cargo clippy", "required": false}
            ]),
        );

        let result = block.execute(&trigger).await.unwrap();

        // Optional gate failure should NOT block success
        assert!(result.success, "optional gate failure should not block success");
        assert_eq!(result.events[0].payload["required_passed"], true);
        assert_eq!(result.events[0].payload["all_passed"], false);
    }

    #[tokio::test]
    async fn no_gates_emits_success() {
        let shell = FakeShellGateway::success();
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![],
        });
        let block = RunPreflightGates::with_shell(shell, registry);
        let trigger =
            gate_resolution_completed_event("my-project", "iterate", &serde_json::json!([]));

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["all_passed"], true);
    }
}
