use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{
    ExecutionCompletedPayload, GateVerificationCompletedPayload, LoopContext,
};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::workflow::WorkflowType;

use crate::gateway::ShellGateway;

use super::TriggerContext;

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

        let p = parse_payload!(trigger, ExecutionCompletedPayload);
        let retry_count = p.retry_count.unwrap_or(0);

        let workflow = WorkflowType::from_payload(&payload);

        let entry = require_project!(self, project);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let project_path = std::path::Path::new(&entry.path);

            // Re-read gates from disk (not from earlier resolution)
            let gates = crate::gate_file::read_gates(project_path)?;

            if gates.is_empty() {
                tracing::info!(project = %project, "no gates defined, verification passes");

                let context = LoopContext::extract_from(&payload);
                return super::emit_result(
                    format!("{project}: no gates defined, verification passes"),
                    EventType::GateVerificationCompleted,
                    &project,
                    throttle,
                    &GateVerificationCompletedPayload {
                        project: project.clone(),
                        workflow: workflow.to_string(),
                        all_passed: true,
                        required_passed: true,
                        retry_count,
                        results: vec![],
                        execution_output: None,
                        context,
                    },
                );
            }

            let run_result =
                crate::gate_runner::run_gates(&gates, project_path, shell.as_ref()).await?;

            Ok(build_verification_result(
                &project,
                workflow,
                retry_count,
                &run_result,
                &payload,
                throttle,
            ))
        })
    }
}

fn build_verification_result(
    project: &str,
    workflow: WorkflowType,
    retry_count: u64,
    run_result: &foundry_core::gates::GatesRunResult,
    payload: &serde_json::Value,
    throttle: foundry_core::throttle::Throttle,
) -> TaskBlockResult {
    let success = run_result.all_passed;

    tracing::info!(
        project = %project,
        all_passed = run_result.all_passed,
        required_passed = run_result.required_passed,
        retry_count = retry_count,
        "gate verification completed"
    );

    let results = super::gate_results_to_json(&run_result.results);
    let execution_output = payload
        .get("execution_output")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let context = LoopContext::extract_from(payload);
    let event_payload = Event::serialize_payload(&GateVerificationCompletedPayload {
        project: project.to_string(),
        workflow: workflow.to_string(),
        all_passed: run_result.all_passed,
        required_passed: run_result.required_passed,
        retry_count,
        results,
        execution_output,
        context,
    })
    .expect("GateVerificationCompletedPayload is infallibly serializable");

    super::build_gate_block_result(
        project,
        EventType::GateVerificationCompleted,
        success,
        "gate verification",
        throttle,
        event_payload,
    )
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
    use super::RunVerifyGates;

    fn execution_completed_event(project: &str, retry_count: u64, workflow: &str) -> Event {
        Event::new(
            EventType::ExecutionCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "retry_count": retry_count,
                "workflow": workflow,
                "success": true,
                "summary": "",
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
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
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
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
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
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
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
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunVerifyGates::with_shell(shell, registry);
        let trigger = execution_completed_event("my-project", 0, "iterate");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["all_passed"], true);
    }
}
