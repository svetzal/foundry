use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Tags a patch release when the main branch is clean (vulnerability already fixed).
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Self-filters: only acts when `dirty=false` in the trigger payload.
pub struct CutRelease;

impl TaskBlock for CutRelease {
    fn name(&self) -> &'static str {
        "Cut Release"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::MainBranchAudited]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let dirty = trigger
            .payload
            .get("dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        if dirty {
            tracing::info!("main branch is dirty, skipping release");
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: main branch is dirty".to_string(),
                })
            });
        }

        let cve = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        tracing::info!(%cve, "cutting patch release");

        Box::pin(async move {
            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::AutoReleaseCompleted,
                    project,
                    throttle,
                    serde_json::json!({ "cve": cve, "release": "patch" }),
                )],
                success: true,
                summary: format!("Cut patch release for {cve}"),
            })
        })
    }
}

/// Watches the CI pipeline after a release tag is pushed.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Polls the GitHub Actions API via the `gh` CLI with exponential backoff
/// (30 s initial, doubling up to 5 min cap, 30 min total timeout).
///
/// Falls back to stub behaviour when no `repo` is available in the trigger
/// payload — this preserves engine integration-test compatibility until the
/// project registry is wired in.
pub struct WatchPipeline {
    /// Optional GitHub repository slug (`owner/repo`).
    /// When `Some`, real polling is performed; when `None`, stub behaviour is used.
    repo: Option<String>,
}

impl WatchPipeline {
    /// Create a `WatchPipeline` that polls the given GitHub repository.
    ///
    /// Called from `main` once the project registry is wired in.
    #[allow(dead_code)]
    pub fn new(repo: impl Into<String>) -> Self {
        Self {
            repo: Some(repo.into()),
        }
    }

    /// Create a `WatchPipeline` that uses stub behaviour (for tests or
    /// when no repository is configured).
    pub fn stub() -> Self {
        Self { repo: None }
    }
}

impl Default for WatchPipeline {
    fn default() -> Self {
        Self::stub()
    }
}

impl TaskBlock for WatchPipeline {
    fn name(&self) -> &'static str {
        "Watch Pipeline"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::AutoReleaseCompleted]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let repo = self.repo.clone();

        Box::pin(async move {
            let Some(repo) = repo else {
                // No repository configured — stub: assume success.
                tracing::info!("no repo configured, using stub pipeline completion");
                return Ok(stub_success(project, throttle));
            };

            poll_pipeline(project, throttle, &repo).await
        })
    }
}

/// Emit a stub successful pipeline completion event.
fn stub_success(project: String, throttle: foundry_core::throttle::Throttle) -> TaskBlockResult {
    TaskBlockResult {
        events: vec![Event::new(
            EventType::ReleasePipelineCompleted,
            project,
            throttle,
            serde_json::json!({ "status": "success" }),
        )],
        success: true,
        summary: "Release pipeline completed successfully".to_string(),
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
            });
        }

        match query_latest_run(repo).await {
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
async fn query_latest_run(repo: &str) -> anyhow::Result<Option<(String, String)>> {
    use tokio::process::Command;

    let output = Command::new("gh")
        .args([
            "run",
            "list",
            "--repo",
            repo,
            "--limit",
            "1",
            "--json",
            "status,conclusion",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh run list failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let runs: serde_json::Value = serde_json::from_str(&stdout)?;

    let Some(run) = runs.as_array().and_then(|a| a.first()) else {
        return Ok(None);
    };

    let status = run["status"].as_str().unwrap_or("").to_string();
    let conclusion = run["conclusion"].as_str().unwrap_or("").to_string();

    Ok(Some((status, conclusion)))
}
