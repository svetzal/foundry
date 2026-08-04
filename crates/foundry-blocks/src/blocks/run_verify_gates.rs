use std::sync::{Arc, RwLock};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gates::{GateResult, GatesRunResult};
use foundry_sdk::gateway::AgentFailureMetadata;
use foundry_sdk::payload::{
    ExecutionCompletedPayload, GateVerificationCompletedPayload, LoopContext,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::ShellGateway;

use super::TriggerContext;

/// Runs quality gates after the execution phase to verify changes.
///
/// Observer — sinks on `ExecutionCompleted`.
/// Re-reads `.hone-gates.json` from the project directory (does not rely on
/// earlier resolution) and runs all gates, emitting `GateVerificationCompleted`.
pub struct RunVerifyGates {
    registry: Arc<RwLock<Registry>>,
    shell: Arc<dyn ShellGateway>,
}

impl RunVerifyGates {
    pub fn new(shell: Arc<dyn ShellGateway>, registry: Arc<RwLock<Registry>>) -> Self {
        Self { registry, shell }
    }

    #[cfg(test)]
    fn with_shell(shell: Arc<dyn ShellGateway>, registry: Arc<RwLock<Registry>>) -> Self {
        Self { registry, shell }
    }
}

impl TaskBlock for RunVerifyGates {
    task_block_meta! {
        name: "Run Verify Gates",
        kind: Observer,
        sinks_on: [ExecutionCompleted],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project,
            throttle,
            payload,
            ..
        } = TriggerContext::from_trigger(trigger);

        let p = parse_payload!(trigger, ExecutionCompletedPayload);
        let retry_count = p.retry_count.unwrap_or(0);
        let failure = p.failure.clone();

        let workflow = WorkflowType::from_payload(&payload);

        let entry = require_project!(self, project);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let task_worktree = payload.get("task_worktree").and_then(serde_json::Value::as_str);
            let project_path = task_worktree
                .map_or_else(|| std::path::Path::new(&entry.path), std::path::Path::new);

            // Short-circuit: if the upstream execution block already failed (e.g. a
            // silent no-op override), skip running the real gates and emit a synthetic
            // failed verification so the retry / route logic fires correctly.
            if !p.success {
                tracing::info!(
                    project = %project,
                    upstream_summary = %p.summary,
                    "upstream execution failed — short-circuiting gate verification"
                );
                let synthetic = GateResult {
                    name: "agent_execution".to_string(),
                    command: String::new(),
                    passed: false,
                    required: true,
                    output: p.summary.clone(),
                    exit_code: 1,
                    duration_ms: None,
                    fix_applied: false,
                };
                let run_result = GatesRunResult {
                    all_passed: false,
                    required_passed: false,
                    results: vec![synthetic],
                };
                return Ok(build_verification_result(
                    &project,
                    workflow,
                    retry_count,
                    &run_result,
                    &payload,
                    throttle,
                    &failure,
                ));
            }

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
                        failure: AgentFailureMetadata::default(),
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
                &AgentFailureMetadata::default(),
            ))
        })
    }
}

fn build_verification_result(
    project: &str,
    workflow: WorkflowType,
    retry_count: u64,
    run_result: &foundry_sdk::gates::GatesRunResult,
    payload: &serde_json::Value,
    throttle: foundry_sdk::throttle::Throttle,
    failure: &foundry_sdk::gateway::AgentFailureMetadata,
) -> TaskBlockResult {
    let success = run_result.required_passed;

    tracing::info!(
        project = %project,
        all_passed = run_result.all_passed,
        required_passed = run_result.required_passed,
        retry_count = retry_count,
        "gate verification completed"
    );

    let results = run_result.results.clone();
    let execution_output = payload
        .get("execution_output")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let context = LoopContext::extract_from(payload);
    super::build_gate_result_from_payload(
        project,
        EventType::GateVerificationCompleted,
        success,
        "gate verification",
        throttle,
        &GateVerificationCompletedPayload {
            project: project.to_string(),
            workflow: workflow.to_string(),
            all_passed: run_result.all_passed,
            required_passed: run_result.required_passed,
            retry_count,
            results,
            execution_output,
            failure: failure.clone(),
            context,
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::gates::{GateResult, GatesRunResult};
    use foundry_sdk::gateway::AgentFailureMetadata;
    use foundry_sdk::registry::Registry;
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::workflow::WorkflowType;

    use crate::gateway::fakes::FakeShellGateway;

    use super::super::test_helpers;
    use super::{RunVerifyGates, build_verification_result};

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

    fn failed_execution_completed_event(project: &str, summary: &str) -> Event {
        Event::new(
            EventType::ExecutionCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "retry_count": 0,
                "workflow": "iterate",
                "success": false,
                "summary": summary,
            }),
        )
    }

    fn terminal_failed_execution_completed_event(project: &str) -> Event {
        Event::new(
            EventType::ExecutionCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "retry_count": 0,
                "workflow": "iterate",
                "success": false,
                "summary": "agent account limit reached: You've hit your monthly spend limit - raise it at claude.ai/settings/usage",
                "api_error_status": 429,
                "failure_kind": "account_limit",
                "terminal": true,
                "message": "You've hit your monthly spend limit - raise it at claude.ai/settings/usage",
            }),
        )
    }

    assert_block_meta!(
        RunVerifyGates::new(
            FakeShellGateway::success(),
            Arc::new(RwLock::new(Registry { version: 2, projects: vec![] })),
        ),
        kind: Observer,
        sinks_on: [ExecutionCompleted],
    );

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

    #[test]
    fn optional_gate_failure_preserves_observability_without_failing_verification() {
        let run_result = GatesRunResult {
            all_passed: false,
            required_passed: true,
            results: vec![GateResult {
                name: "dialyzer".to_string(),
                command: "mix dialyzer".to_string(),
                passed: false,
                required: false,
                output: "pre-existing warnings".to_string(),
                exit_code: 2,
                duration_ms: Some(100),
                fix_applied: false,
            }],
        };

        let result = build_verification_result(
            "my-project",
            WorkflowType::Task,
            0,
            &run_result,
            &serde_json::json!({}),
            Throttle::DryRun,
            &AgentFailureMetadata::default(),
        );

        assert!(result.success);
        assert_eq!(result.events[0].payload["all_passed"], false);
        assert_eq!(result.events[0].payload["required_passed"], true);
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

    #[tokio::test]
    async fn upstream_failure_short_circuits_to_failed_verification() {
        let dir = tempfile::tempdir().unwrap();
        // Even with a gate file present, we should not run gates if upstream failed
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
        )
        .unwrap();

        let shell = FakeShellGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunVerifyGates::with_shell(shell, registry);
        let trigger = failed_execution_completed_event(
            "my-project",
            "agent did not modify any files (silent no-op)",
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::GateVerificationCompleted);
        assert_eq!(result.events[0].payload["all_passed"], false);
        assert_eq!(result.events[0].payload["required_passed"], false);

        // Should contain the synthetic agent_execution gate result
        let results = result.events[0].payload["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "expected exactly one synthetic gate result");
        assert_eq!(results[0]["name"], "agent_execution");
        assert_eq!(results[0]["passed"], false);
        assert_eq!(results[0]["required"], true);
    }

    #[tokio::test]
    async fn terminal_upstream_failure_propagates_failure_metadata() {
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
        let trigger = terminal_failed_execution_completed_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events[0].payload["failure_kind"], "account_limit");
        assert_eq!(result.events[0].payload["terminal"], true);
        assert_eq!(result.events[0].payload["api_error_status"], 429);
    }

    #[tokio::test]
    async fn upstream_success_runs_gates_normally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
        )
        .unwrap();

        // Gates pass
        let shell = FakeShellGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = RunVerifyGates::with_shell(shell, registry);
        let trigger = execution_completed_event("my-project", 0, "iterate");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["all_passed"], true);
        // Real gate should appear in results (not the synthetic one)
        let results = result.events[0].payload["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["name"], "fmt");
    }
}
