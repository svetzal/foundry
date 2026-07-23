use std::path::Path;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::PipelineCheckedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

use super::TriggerContext;

task_block_new! {
    /// Checks whether a project's GitHub Actions pipeline is passing.
    /// Observer -- always runs regardless of throttle.
    ///
    /// Sinks on `PipelineCheckRequested` and emits `PipelineChecked` with the
    /// current pass/fail status and optional failure logs.
    pub struct CheckPipeline {
        shell: ShellGateway = crate::gateway::ProcessShellGateway
    }
}

impl TaskBlock for CheckPipeline {
    task_block_meta! {
        name: "Check Pipeline",
        kind: Observer,
        sinks_on: [PipelineCheckRequested],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project, throttle, ..
        } = TriggerContext::from_trigger(trigger);

        let entry = require_project!(self, project);
        let shell = Arc::clone(&self.shell);

        Box::pin(run_check(project, throttle, entry, shell))
    }
}

async fn run_check(
    project: String,
    throttle: foundry_sdk::throttle::Throttle,
    entry: foundry_sdk::registry::ProjectEntry,
    shell: Arc<dyn ShellGateway>,
) -> anyhow::Result<TaskBlockResult> {
    if entry.repo.is_empty() {
        tracing::info!(project = %project, "no repo configured, skipping pipeline check");
        return super::emit_result(
            "no repo configured".to_string(),
            EventType::PipelineChecked,
            &project,
            throttle,
            &PipelineCheckedPayload {
                passing: true,
                conclusion: "skipped".to_string(),
                run_id: None,
                run_name: None,
                failure_logs: None,
            },
        );
    }

    let repo = &entry.repo;
    let branch = &entry.branch;

    // Query the most recent workflow runs
    let list_result = shell
        .run(
            Path::new("."),
            "gh",
            &[
                "run",
                "list",
                "--repo",
                repo,
                "--branch",
                branch,
                "--limit",
                "5",
                "--json",
                "status,conclusion,name,databaseId",
            ],
            None,
            None,
        )
        .await?;

    if !list_result.success {
        tracing::warn!(project = %project, stderr = %list_result.stderr, "gh run list failed");
        return Ok(TaskBlockResult::failure(format!(
            "gh run list failed: {}",
            list_result.stderr.lines().next().unwrap_or("unknown error")
        )));
    }

    let runs: serde_json::Value = serde_json::from_str(&list_result.stdout)?;
    let runs = runs.as_array().map_or(&[] as &[_], Vec::as_slice);

    // Find the most recent completed run
    let completed = runs.iter().find(|r| {
        r.get("status")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| s == "completed")
    });

    let Some(run) = completed else {
        tracing::info!(project = %project, "no completed runs found");
        return super::emit_result(
            "no completed runs found".to_string(),
            EventType::PipelineChecked,
            &project,
            throttle,
            &PipelineCheckedPayload {
                passing: true,
                conclusion: "no_runs".to_string(),
                run_id: None,
                run_name: None,
                failure_logs: None,
            },
        );
    };

    let conclusion = run
        .get("conclusion")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let run_id = run.get("databaseId").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let run_name = run
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let passing = conclusion == "success";

    let failure_logs = if passing {
        None
    } else {
        fetch_failure_logs(run_id, repo, shell.as_ref()).await
    };

    tracing::info!(project = %project, %passing, %conclusion, "pipeline check complete");

    Ok(build_pipeline_result(
        &project,
        &conclusion,
        run_id,
        &run_name,
        passing,
        failure_logs.as_deref(),
        throttle,
    ))
}

fn build_pipeline_result(
    project: &str,
    conclusion: &str,
    run_id: u64,
    run_name: &str,
    passing: bool,
    failure_logs: Option<&str>,
    throttle: foundry_sdk::throttle::Throttle,
) -> TaskBlockResult {
    let summary = if passing {
        format!("Pipeline passing: {run_name} (#{run_id})")
    } else {
        format!("Pipeline failing: {run_name} (#{run_id}) conclusion={conclusion}")
    };

    #[allow(
        clippy::expect_used,
        reason = "PipelineCheckedPayload is infallibly serializable (Payload Conventions, AGENTS.md)"
    )]
    super::emit_result(
        summary,
        EventType::PipelineChecked,
        project,
        throttle,
        &PipelineCheckedPayload {
            passing,
            conclusion: conclusion.to_string(),
            run_id: Some(run_id),
            run_name: Some(run_name.to_string()),
            failure_logs: failure_logs.map(str::to_string),
        },
    )
    .expect("PipelineCheckedPayload is infallibly serializable")
}

/// Fetch the failure logs for a specific run, truncated to 4000 characters.
async fn fetch_failure_logs(run_id: u64, repo: &str, shell: &dyn ShellGateway) -> Option<String> {
    let run_id_str = run_id.to_string();
    let log_result = shell
        .run(
            Path::new("."),
            "gh",
            &["run", "view", &run_id_str, "--repo", repo, "--log-failed"],
            None,
            None,
        )
        .await;

    match log_result {
        Ok(r) if r.success => {
            let logs = r.stdout;
            if logs.len() > 4000 {
                Some(logs[..4000].to_string())
            } else {
                Some(logs)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::{ProjectEntry, Registry};
    use foundry_sdk::task_block::TaskBlock;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::super::test_helpers;
    use super::CheckPipeline;

    fn registry_with_repo(name: &str, repo: &str) -> Arc<RwLock<Registry>> {
        test_helpers::registry_with_entry(ProjectEntry {
            agent: String::new(),
            repo: repo.to_string(),
            ..test_helpers::project_entry(name, "")
        })
    }

    fn trigger(project: &str) -> Event {
        test_event!(EventType::PipelineCheckRequested, project, {})
    }

    assert_block_meta!(
        CheckPipeline::new(Arc::new(RwLock::new(Registry { version: 2, projects: vec![] }))),
        kind: Observer,
        sinks_on: [PipelineCheckRequested],
    );

    #[tokio::test]
    async fn skips_when_no_repo_configured() {
        let registry = test_helpers::registry_with_entry(ProjectEntry {
            agent: String::new(),
            ..test_helpers::project_entry("my-project", "")
        });
        let shell = FakeShellGateway::success();
        let block = CheckPipeline::with_gateways(registry, shell);
        let t = trigger("my-project");

        let result = block.execute(&t).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::PipelineChecked);
        assert_eq!(result.events[0].payload["passing"], true);
        assert!(result.summary.contains("no repo configured"));
    }

    #[tokio::test]
    async fn passing_pipeline_emits_pipeline_checked_with_passing_true() {
        let registry = registry_with_repo("my-project", "owner/my-project");
        let gh_output = serde_json::json!([
            {
                "status": "completed",
                "conclusion": "success",
                "name": "CI",
                "databaseId": 12345
            }
        ]);
        let shell = FakeShellGateway::always(CommandResult {
            stdout: gh_output.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let block = CheckPipeline::with_gateways(registry, shell);
        let t = trigger("my-project");

        let result = block.execute(&t).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::PipelineChecked);
        assert_eq!(result.events[0].payload["passing"], true);
        assert_eq!(result.events[0].payload["conclusion"], "success");
        assert_eq!(result.events[0].payload["run_id"], 12345);
        assert_eq!(result.events[0].payload["run_name"], "CI");
    }

    #[tokio::test]
    async fn failing_pipeline_includes_failure_logs() {
        let registry = registry_with_repo("my-project", "owner/my-project");
        let gh_list_output = serde_json::json!([
            {
                "status": "completed",
                "conclusion": "failure",
                "name": "CI",
                "databaseId": 99999
            }
        ]);
        let shell = FakeShellGateway::sequence(vec![
            // First call: gh run list
            CommandResult {
                stdout: gh_list_output.to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // Second call: gh run view --log-failed
            CommandResult {
                stdout: "error: test failed in src/lib.rs".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = CheckPipeline::with_gateways(registry, shell);
        let t = trigger("my-project");

        let result = block.execute(&t).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::PipelineChecked);
        assert_eq!(result.events[0].payload["passing"], false);
        assert_eq!(result.events[0].payload["conclusion"], "failure");
        assert_eq!(result.events[0].payload["run_id"], 99999);
        assert_eq!(result.events[0].payload["failure_logs"], "error: test failed in src/lib.rs");
    }

    #[tokio::test]
    async fn gh_run_list_failure_returns_failure_result() {
        let registry = registry_with_repo("my-project", "owner/my-project");
        let shell = FakeShellGateway::failure("error: authentication required");
        let block = CheckPipeline::with_gateways(registry, shell);
        let t = trigger("my-project");

        let result = block.execute(&t).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("gh run list failed"));
    }

    #[tokio::test]
    async fn no_completed_runs_emits_no_runs_conclusion() {
        let registry = registry_with_repo("my-project", "owner/my-project");
        let gh_output = serde_json::json!([
            {
                "status": "in_progress",
                "conclusion": null,
                "name": "CI",
                "databaseId": 11111
            }
        ]);
        let shell = FakeShellGateway::always(CommandResult {
            stdout: gh_output.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let block = CheckPipeline::with_gateways(registry, shell);
        let t = trigger("my-project");

        let result = block.execute(&t).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::PipelineChecked);
        assert_eq!(result.events[0].payload["conclusion"], "no_runs");
    }

    #[tokio::test]
    async fn failure_logs_truncated_at_4000_chars() {
        let registry = registry_with_repo("my-project", "owner/my-project");
        let gh_list_output = serde_json::json!([
            {
                "status": "completed",
                "conclusion": "failure",
                "name": "CI",
                "databaseId": 77777
            }
        ]);
        let long_logs = "x".repeat(5000);
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: gh_list_output.to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: long_logs,
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = CheckPipeline::with_gateways(registry, shell);
        let t = trigger("my-project");

        let result = block.execute(&t).await.unwrap();

        let logs = result.events[0].payload["failure_logs"].as_str().unwrap();
        assert_eq!(logs.len(), 4000);
    }

    #[tokio::test]
    async fn unknown_project_emits_stub() {
        let block = CheckPipeline::new(test_helpers::empty_registry());
        let t = trigger("unknown-project");

        let result = block.execute(&t).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("not found in registry"));
    }
}
