use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType, PayloadExt};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

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

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let entry = match super::require_project(&self.registry, &project) {
            Ok(e) => e,
            Err(result) => return Box::pin(async { Ok(result) }),
        };
        let shell = Arc::clone(&self.shell);

        Box::pin(run_check(project, throttle, entry, shell))
    }
}

async fn run_check(
    project: String,
    throttle: foundry_core::throttle::Throttle,
    entry: foundry_core::registry::ProjectEntry,
    shell: Arc<dyn ShellGateway>,
) -> anyhow::Result<TaskBlockResult> {
    if entry.repo.is_empty() {
        tracing::info!(project = %project, "no repo configured, skipping pipeline check");
        return Ok(make_no_repo_result(project, throttle));
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
        return Ok(TaskBlockResult::success(
            "no completed runs found",
            vec![Event::new(
                EventType::PipelineChecked,
                project,
                throttle,
                serde_json::json!({
                    "passing": true,
                    "conclusion": "no_runs",
                    "run_id": null,
                    "run_name": null,
                }),
            )],
        ));
    };

    let conclusion = run.str_or("conclusion", "unknown").to_string();
    let run_id = run.u64_or("databaseId", 0);
    let run_name = run.str_or("name", "unknown").to_string();

    let passing = conclusion == "success";

    let failure_logs = if passing {
        None
    } else {
        fetch_failure_logs(run_id, repo, shell.as_ref()).await
    };

    tracing::info!(project = %project, %passing, %conclusion, "pipeline check complete");

    Ok(build_pipeline_result(
        project,
        &conclusion,
        run_id,
        &run_name,
        passing,
        failure_logs.as_deref(),
        throttle,
    ))
}

fn make_no_repo_result(
    project: String,
    throttle: foundry_core::throttle::Throttle,
) -> TaskBlockResult {
    TaskBlockResult::success(
        "no repo configured",
        vec![Event::new(
            EventType::PipelineChecked,
            project,
            throttle,
            serde_json::json!({
                "passing": true,
                "conclusion": "skipped",
                "run_id": null,
                "run_name": null,
            }),
        )],
    )
}

fn build_pipeline_result(
    project: String,
    conclusion: &str,
    run_id: u64,
    run_name: &str,
    passing: bool,
    failure_logs: Option<&str>,
    throttle: foundry_core::throttle::Throttle,
) -> TaskBlockResult {
    let mut payload = serde_json::json!({
        "passing": passing,
        "conclusion": conclusion,
        "run_id": run_id,
        "run_name": run_name,
    });

    if let Some(logs) = failure_logs {
        payload["failure_logs"] = serde_json::Value::String(logs.to_string());
    }

    let summary = if passing {
        format!("Pipeline passing: {run_name} (#{run_id})")
    } else {
        format!("Pipeline failing: {run_name} (#{run_id}) conclusion={conclusion}")
    };

    TaskBlockResult::success(
        summary,
        vec![Event::new(
            EventType::PipelineChecked,
            project,
            throttle,
            payload,
        )],
    )
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
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::CheckPipeline;

    fn empty_registry() -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![],
        })
    }

    fn registry_with_repo(name: &str, repo: &str) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: name.to_string(),
                path: String::new(),
                stack: Stack::Rust,
                agent: String::new(),
                repo: repo.to_string(),
                branch: "main".to_string(),
                skip: None,
                notes: None,
                actions: ActionFlags::default(),
                install: None,
                timeout_secs: None,
            }],
        })
    }

    fn trigger(project: &str) -> Event {
        Event::new(
            EventType::PipelineCheckRequested,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
    }

    #[test]
    fn kind_is_observer() {
        let block = CheckPipeline::new(empty_registry());
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[tokio::test]
    async fn skips_when_no_repo_configured() {
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: "my-project".to_string(),
                path: String::new(),
                stack: Stack::Rust,
                agent: String::new(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                notes: None,
                actions: ActionFlags::default(),
                install: None,
                timeout_secs: None,
            }],
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
}
