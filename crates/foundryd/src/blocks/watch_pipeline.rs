use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

/// Watches the CI pipeline after a release tag is pushed.
/// Mutator — events logged but not delivered at `audit_only`;
/// simulated success at `dry_run`.
///
/// Polls the GitHub Actions API via the `gh` CLI with exponential backoff
/// (30 s initial, doubling up to 5 min cap, 30 min total timeout).
///
/// Looks up the GitHub repository slug (`owner/repo`) from the project registry.
/// Falls back to stub behaviour when the project has no `repo` configured.
pub struct WatchPipeline {
    registry: Arc<Registry>,
    shell: Arc<dyn ShellGateway>,
}

impl WatchPipeline {
    /// Create a `WatchPipeline` that resolves the repository from the registry.
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            shell: Arc::new(crate::gateway::ProcessShellGateway),
        }
    }

    /// Create a `WatchPipeline` backed by an empty registry (for tests).
    #[cfg(test)]
    pub fn stub() -> Self {
        Self {
            registry: Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
            shell: Arc::new(crate::gateway::ProcessShellGateway),
        }
    }

    /// Create a `WatchPipeline` with injected registry and shell gateway (for tests).
    #[cfg(test)]
    fn with_gateways(registry: Arc<Registry>, shell: Arc<dyn ShellGateway>) -> Self {
        Self { registry, shell }
    }
}

impl TaskBlock for WatchPipeline {
    task_block_meta! {
        name: "Watch Pipeline",
        kind: Mutator,
        sinks_on: [ReleaseCompleted],
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        vec![Event::new(
            EventType::ReleasePipelineCompleted,
            trigger.project.clone(),
            trigger.throttle,
            serde_json::json!({ "status": "success", "dry_run": true }),
        )]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let repo = self
            .registry
            .find_project(&project)
            .map(|p| p.repo.clone())
            .filter(|r| !r.is_empty());

        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let Some(repo) = repo else {
                // No repository configured — stub: assume success.
                tracing::info!("no repo configured, using stub pipeline completion");
                return Ok(super::stub_event_result(
                    "Release pipeline completed successfully",
                    EventType::ReleasePipelineCompleted,
                    project,
                    throttle,
                    serde_json::json!({ "status": "success" }),
                ));
            };

            poll_pipeline(project, throttle, &repo, shell.as_ref()).await
        })
    }
}

/// Poll GitHub Actions for the latest workflow run on `repo` until it
/// completes, times out, or encounters a non-recoverable error.
///
/// Backoff: 30 s initial, doubling each iteration, capped at 5 min.
/// Total timeout: 30 min.
async fn poll_pipeline(
    project: String,
    throttle: foundry_core::throttle::Throttle,
    repo: &str,
    shell: &dyn ShellGateway,
) -> anyhow::Result<TaskBlockResult> {
    use std::time::{Duration, Instant};

    let timeout = Duration::from_secs(30 * 60);
    let start = Instant::now();
    let mut delay = Duration::from_secs(30);
    let max_delay = Duration::from_secs(300);

    tracing::info!(%repo, "watching release pipeline via GitHub Actions");

    loop {
        if start.elapsed() >= timeout {
            tracing::warn!(%repo, "pipeline watch timed out after 30 minutes");
            return Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ReleasePipelineCompleted,
                    project,
                    throttle,
                    serde_json::json!({ "status": "failure", "conclusion": "timed_out" }),
                )],
                success: false,
                summary: "Pipeline watch timed out after 30 minutes".to_string(),
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            });
        }

        match query_latest_run(repo, shell).await {
            Ok(Some((status, conclusion))) => match status.as_str() {
                "completed" => {
                    let success = conclusion == "success";
                    tracing::info!(%repo, %conclusion, "pipeline completed");
                    return Ok(TaskBlockResult {
                        events: vec![Event::new(
                            EventType::ReleasePipelineCompleted,
                            project,
                            throttle,
                            serde_json::json!({
                                "status": if success { "success" } else { "failure" },
                                "conclusion": conclusion,
                            }),
                        )],
                        success,
                        summary: format!("Release pipeline completed: {conclusion}"),
                        raw_output: None,
                        exit_code: None,
                        audit_artifacts: vec![],
                    });
                }
                s @ ("in_progress" | "queued" | "waiting") => {
                    tracing::info!(%repo, status = s, "pipeline still running, waiting...");
                }
                other => {
                    tracing::info!(%repo, status = other, "unknown pipeline status, waiting...");
                }
            },
            Ok(None) => {
                tracing::info!(%repo, "no workflow runs found yet, waiting...");
            }
            Err(err) => {
                // API errors are non-fatal — log and retry.
                tracing::warn!(%repo, error = %err, "error querying pipeline status, retrying");
            }
        }

        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(max_delay);
    }
}

/// Query the most recent workflow run for `repo` via the `gh` CLI.
///
/// Returns `Ok(Some((status, conclusion)))` on success, `Ok(None)` when no
/// runs exist, and `Err` on CLI or JSON parse failure.
async fn query_latest_run(
    repo: &str,
    shell: &dyn ShellGateway,
) -> anyhow::Result<Option<(String, String)>> {
    let result = shell
        .run(
            Path::new("."),
            "gh",
            &[
                "run",
                "list",
                "--repo",
                repo,
                "--limit",
                "1",
                "--json",
                "status,conclusion",
            ],
            None,
            None,
        )
        .await?;

    if !result.success {
        anyhow::bail!("gh run list failed: {}", result.stderr);
    }

    let runs: serde_json::Value = serde_json::from_str(&result.stdout)?;

    let Some(run) = runs.as_array().and_then(|a| a.first()) else {
        return Ok(None);
    };

    let status = run["status"].as_str().unwrap_or("").to_string();
    let conclusion = run["conclusion"].as_str().unwrap_or("").to_string();

    Ok(Some((status, conclusion)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::TaskBlock;
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::*;

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
                installs_skill: None,
                timeout_secs: None,
            }],
        })
    }

    fn trigger() -> Event {
        Event::new(
            EventType::ReleaseCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "success": true }),
        )
    }

    fn completed_run(conclusion: &str) -> CommandResult {
        CommandResult {
            stdout: serde_json::json!([{"status": "completed", "conclusion": conclusion}])
                .to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    #[tokio::test]
    async fn watch_pipeline_stubs_when_project_not_in_registry() {
        let block = WatchPipeline::stub();
        let trigger = Event::new(
            EventType::ReleaseCompleted,
            "some-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "success": true }),
        );

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleasePipelineCompleted);
        assert_eq!(result.events[0].payload["status"], "success");
    }

    #[tokio::test]
    async fn watch_pipeline_stubs_when_project_has_empty_repo() {
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: "my-project".to_string(),
                path: String::new(),
                stack: Stack::Rust,
                agent: String::new(),
                repo: String::new(), // empty — no GitHub repo configured
                branch: "main".to_string(),
                skip: None,
                notes: None,
                actions: ActionFlags::default(),
                install: None,
                installs_skill: None,
                timeout_secs: None,
            }],
        });
        let block = WatchPipeline::new(registry);
        let trigger = Event::new(
            EventType::ReleaseCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "success": true }),
        );

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleasePipelineCompleted);
        assert_eq!(result.events[0].payload["status"], "success");
    }

    #[tokio::test(start_paused = true)]
    async fn pipeline_completed_success() {
        let registry = registry_with_repo("my-project", "owner/my-project");
        let shell = FakeShellGateway::always(completed_run("success"));
        let block = WatchPipeline::with_gateways(registry, shell);

        let result = block.execute(&trigger()).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleasePipelineCompleted);
        assert_eq!(result.events[0].payload["status"], "success");
        assert_eq!(result.events[0].payload["conclusion"], "success");
    }

    #[tokio::test(start_paused = true)]
    async fn pipeline_completed_failure() {
        let registry = registry_with_repo("my-project", "owner/my-project");
        let shell = FakeShellGateway::always(completed_run("failure"));
        let block = WatchPipeline::with_gateways(registry, shell);

        let result = block.execute(&trigger()).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleasePipelineCompleted);
        assert_eq!(result.events[0].payload["status"], "failure");
        assert_eq!(result.events[0].payload["conclusion"], "failure");
    }

    #[tokio::test(start_paused = true)]
    async fn no_workflow_runs_retries_then_completes() {
        let registry = registry_with_repo("my-project", "owner/my-project");
        let shell = FakeShellGateway::sequence(vec![
            // First poll: no runs yet
            CommandResult {
                stdout: "[]".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // Second poll: completed successfully
            completed_run("success"),
        ]);
        let block = WatchPipeline::with_gateways(registry, shell);

        let result = block.execute(&trigger()).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["status"], "success");
    }

    #[tokio::test(start_paused = true)]
    async fn gh_cli_error_is_non_fatal() {
        let registry = registry_with_repo("my-project", "owner/my-project");
        let shell = FakeShellGateway::sequence(vec![
            // First poll: gh CLI fails
            CommandResult {
                stdout: String::new(),
                stderr: "gh: not authenticated".to_string(),
                exit_code: 1,
                success: false,
            },
            // Second poll: succeeds
            completed_run("success"),
        ]);
        let block = WatchPipeline::with_gateways(registry, shell);

        let result = block.execute(&trigger()).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["status"], "success");
    }

    #[test]
    fn dry_run_events_returns_success_stub() {
        let block = WatchPipeline::stub();
        let t = trigger();

        let events = block.dry_run_events(&t);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::ReleasePipelineCompleted);
        assert_eq!(events[0].payload["status"], "success");
        assert_eq!(events[0].payload["dry_run"], true);
    }
}
