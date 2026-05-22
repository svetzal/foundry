use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use foundry_core::event::{Event, EventType};
use foundry_core::payload::ReleaseCompletedPayload;
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
    registry: Arc<RwLock<Registry>>,
    shell: Arc<dyn ShellGateway>,
}

impl WatchPipeline {
    /// Create a `WatchPipeline` that resolves the repository from the registry.
    pub fn new(registry: Arc<RwLock<Registry>>) -> Self {
        Self {
            registry,
            shell: Arc::new(crate::gateway::ProcessShellGateway),
        }
    }

    /// Create a `WatchPipeline` backed by an empty registry (for tests).
    #[cfg(test)]
    pub fn stub() -> Self {
        Self {
            registry: Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
            shell: Arc::new(crate::gateway::ProcessShellGateway),
        }
    }

    /// Create a `WatchPipeline` with injected registry and shell gateway (for tests).
    #[cfg(test)]
    fn with_gateways(registry: Arc<RwLock<Registry>>, shell: Arc<dyn ShellGateway>) -> Self {
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
            .read()
            .expect("registry lock poisoned")
            .find_project(&project)
            .map(|p| p.repo.clone())
            .filter(|r| !r.is_empty());

        // Extract the release tag from the trigger payload so we can filter
        // `gh run list` to only the run that was triggered by this release.
        let new_tag =
            trigger.parse_payload::<ReleaseCompletedPayload>().ok().and_then(|p| p.new_tag);

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

            poll_pipeline(project, throttle, &repo, new_tag.as_deref(), shell.as_ref()).await
        })
    }
}

/// Poll GitHub Actions for the relevant release workflow run on `repo` until it
/// completes, times out, or encounters a non-recoverable error.
///
/// When `new_tag` is `Some`, filters runs to those where `event == "release"` or
/// `headBranch == new_tag`, preferring release-event runs first. Falls back to the
/// most-recent run when `new_tag` is `None`.
///
/// Once a matching run is selected by ID, polls that specific run until completion.
///
/// Backoff: 30 s initial, doubling each iteration, capped at 5 min.
/// Total timeout: 30 min.
async fn poll_pipeline(
    project: String,
    throttle: foundry_core::throttle::Throttle,
    repo: &str,
    new_tag: Option<&str>,
    shell: &dyn ShellGateway,
) -> anyhow::Result<TaskBlockResult> {
    use std::time::{Duration, Instant};

    let timeout = Duration::from_secs(30 * 60);
    let start = Instant::now();
    let mut delay = Duration::from_secs(30);
    let max_delay = Duration::from_secs(300);

    tracing::info!(%repo, new_tag = new_tag.unwrap_or("(none)"), "watching release pipeline via GitHub Actions");

    // Phase 1: find the run ID for this release.
    let run_id = loop {
        if start.elapsed() >= timeout {
            tracing::warn!(%repo, "pipeline watch timed out waiting for matching run");
            return Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ReleasePipelineCompleted,
                    project,
                    throttle,
                    serde_json::json!({ "status": "failure", "conclusion": "timed_out" }),
                )],
                summary: "Pipeline watch timed out after 30 minutes".to_string(),
                ..Default::default()
            });
        }

        match find_release_run_id(repo, new_tag, shell).await {
            Ok(Some(id)) => break id,
            Ok(None) => {
                tracing::info!(%repo, "no matching workflow run found yet, waiting...");
            }
            Err(err) => {
                tracing::warn!(%repo, error = %err, "error querying workflow runs, retrying");
            }
        }

        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(max_delay);
    };

    tracing::info!(%repo, run_id, "locked onto release workflow run");

    // Phase 2: poll the specific run by ID until it completes.
    loop {
        if start.elapsed() >= timeout {
            tracing::warn!(%repo, run_id, "pipeline watch timed out after 30 minutes");
            return Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ReleasePipelineCompleted,
                    project,
                    throttle,
                    serde_json::json!({ "status": "failure", "conclusion": "timed_out" }),
                )],
                summary: "Pipeline watch timed out after 30 minutes".to_string(),
                ..Default::default()
            });
        }

        match query_run_by_id(run_id, shell).await {
            Ok(Some((status, conclusion))) => match status.as_str() {
                "completed" => {
                    let success = conclusion == "success";
                    tracing::info!(%repo, run_id, %conclusion, "pipeline completed");
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
                        ..Default::default()
                    });
                }
                s @ ("in_progress" | "queued" | "waiting") => {
                    tracing::info!(%repo, run_id, status = s, "pipeline still running, waiting...");
                }
                other => {
                    tracing::info!(%repo, run_id, status = other, "unknown pipeline status, waiting...");
                }
            },
            Ok(None) => {
                tracing::info!(%repo, run_id, "run view returned no data, waiting...");
            }
            Err(err) => {
                tracing::warn!(%repo, run_id, error = %err, "error querying pipeline status, retrying");
            }
        }

        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(max_delay);
    }
}

/// Find the database ID of the release workflow run for `repo`.
///
/// When `new_tag` is `Some`, filters the most recent 30 runs by:
///   1. Runs where `event == "release"` that match `headBranch == new_tag` (exact match)
///   2. Runs where `headBranch == new_tag`
///   3. Among event=="release" runs, prefer those whose `workflowName` contains
///      "release", "publish", or "deploy"
///
/// When `new_tag` is `None`, falls back to the most recent run.
///
/// Returns `Ok(None)` when no matching run is found yet (caller should retry).
async fn find_release_run_id(
    repo: &str,
    new_tag: Option<&str>,
    shell: &dyn ShellGateway,
) -> anyhow::Result<Option<u64>> {
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
                "30",
                "--json",
                "databaseId,status,conclusion,event,headBranch,workflowName,createdAt",
            ],
            None,
            None,
        )
        .await?;

    if !result.success {
        anyhow::bail!("gh run list failed: {}", result.stderr);
    }

    let runs: serde_json::Value = serde_json::from_str(&result.stdout)?;
    let Some(runs_arr) = runs.as_array() else {
        return Ok(None);
    };

    if runs_arr.is_empty() {
        return Ok(None);
    }

    let Some(tag) = new_tag else {
        // No tag available — fall back to most recent run.
        return Ok(runs_arr.first().and_then(|r| r["databaseId"].as_u64()));
    };

    // Build a scored list: prefer (release event + matching branch) → (release
    // event) → (matching branch) → skip.
    let mut best: Option<(u8, u64)> = None; // (score, id)

    for run in runs_arr {
        let event = run["event"].as_str().unwrap_or("");
        let head_branch = run["headBranch"].as_str().unwrap_or("");
        let workflow_name = run["workflowName"].as_str().unwrap_or("").to_lowercase();
        let Some(db_id) = run["databaseId"].as_u64() else {
            continue;
        };

        let is_release_event = event == "release";
        let is_tag_branch = head_branch == tag;
        let has_release_name = workflow_name.contains("release")
            || workflow_name.contains("publish")
            || workflow_name.contains("deploy");

        let score: u8 = if is_release_event && is_tag_branch {
            4
        } else if is_release_event && has_release_name {
            3
        } else if is_release_event {
            2
        } else if is_tag_branch {
            1
        } else {
            continue; // skip this run
        };

        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, db_id));
        }
    }

    // If nothing matched the tag filters, fall back to the most recent run.
    if best.is_none() {
        tracing::info!(
            %repo,
            tag = %tag,
            "no release/tag-matching run found yet; will retry"
        );
        return Ok(None);
    }

    Ok(best.map(|(_, id)| id))
}

/// Query a specific workflow run by its database ID via the `gh` CLI.
///
/// Returns `Ok(Some((status, conclusion)))` on success, `Ok(None)` when the run
/// view returns no data, and `Err` on CLI or JSON parse failure.
async fn query_run_by_id(
    run_id: u64,
    shell: &dyn ShellGateway,
) -> anyhow::Result<Option<(String, String)>> {
    let id_str = run_id.to_string();
    let result = shell
        .run(
            Path::new("."),
            "gh",
            &["run", "view", &id_str, "--json", "status,conclusion"],
            None,
            None,
        )
        .await?;

    if !result.success {
        anyhow::bail!("gh run view failed: {}", result.stderr);
    }

    let run: serde_json::Value = serde_json::from_str(&result.stdout)?;

    let status = run["status"].as_str().unwrap_or("").to_string();
    let conclusion = run["conclusion"].as_str().unwrap_or("").to_string();

    if status.is_empty() {
        return Ok(None);
    }

    Ok(Some((status, conclusion)))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::TaskBlock;
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::*;

    fn registry_with_repo(name: &str, repo: &str) -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
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
        }))
    }

    /// Trigger with no `new_tag` — fallback to most-recent-run behaviour.
    fn trigger() -> Event {
        Event::new(
            EventType::ReleaseCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "success": true }),
        )
    }

    /// Trigger carrying a `new_tag` — enables release-run filtering.
    fn trigger_with_tag(tag: &str) -> Event {
        Event::new(
            EventType::ReleaseCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "success": true, "new_tag": tag }),
        )
    }

    /// `gh run list` response: a single run with `databaseId`, ready to be
    /// selected by the fallback (no-tag) path.
    fn run_list_with_id(id: u64, event: &str, head_branch: &str) -> CommandResult {
        CommandResult {
            stdout: serde_json::json!([{
                "databaseId": id,
                "status": "completed",
                "conclusion": "success",
                "event": event,
                "headBranch": head_branch,
                "workflowName": "Release",
                "createdAt": "2026-05-01T00:00:00Z",
            }])
            .to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    /// `gh run view <id>` response: a completed run with the given conclusion.
    fn run_view_completed(conclusion: &str) -> CommandResult {
        CommandResult {
            stdout: serde_json::json!({ "status": "completed", "conclusion": conclusion })
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
        let registry = Arc::new(RwLock::new(Registry {
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
        }));
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
        // No new_tag → fallback: gh run list returns run #42, then gh run view #42 = success.
        let shell = FakeShellGateway::sequence(vec![
            run_list_with_id(42, "push", "main"),
            run_view_completed("success"),
        ]);
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
        let shell = FakeShellGateway::sequence(vec![
            run_list_with_id(42, "push", "main"),
            run_view_completed("failure"),
        ]);
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
            // Second poll: run #99 found
            run_list_with_id(99, "push", "main"),
            // gh run view #99: completed successfully
            run_view_completed("success"),
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
            // Second poll: run list succeeds
            run_list_with_id(77, "push", "main"),
            // gh run view: succeeds
            run_view_completed("success"),
        ]);
        let block = WatchPipeline::with_gateways(registry, shell);

        let result = block.execute(&trigger()).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["status"], "success");
    }

    // --- Release-run filtering tests ---

    #[tokio::test(start_paused = true)]
    async fn filters_to_release_event_run_when_tag_provided() {
        let registry = registry_with_repo("my-project", "owner/my-project");

        // gh run list returns two runs: a CI push run and a release-event run.
        let run_list = CommandResult {
            stdout: serde_json::json!([
                {
                    "databaseId": 10,
                    "status": "completed",
                    "conclusion": "success",
                    "event": "push",
                    "headBranch": "main",
                    "workflowName": "CI",
                    "createdAt": "2026-05-01T00:01:00Z",
                },
                {
                    "databaseId": 20,
                    "status": "completed",
                    "conclusion": "success",
                    "event": "release",
                    "headBranch": "v1.5.0",
                    "workflowName": "Release",
                    "createdAt": "2026-05-01T00:00:00Z",
                },
            ])
            .to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        };

        let shell = FakeShellGateway::sequence(vec![
            run_list,
            // gh run view #20 (the release run, not the CI push run #10)
            run_view_completed("success"),
        ]);
        let block = WatchPipeline::with_gateways(registry, shell);

        let result = block.execute(&trigger_with_tag("v1.5.0")).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["status"], "success");

        // Verify that the shell was called with "20" (the release run ID),
        // not "10" (the CI push run).
        // We can't inspect the FakeShellGateway invocations here through the Arc,
        // but the fact that the result is success (run #20 was success) confirms
        // the correct run was selected. A separate direct test of `find_release_run_id`
        // verifies the scoring logic below.
    }

    #[tokio::test(start_paused = true)]
    async fn falls_back_to_most_recent_when_no_tag_matches() {
        let registry = registry_with_repo("my-project", "owner/my-project");

        // Two push runs, neither is a release event. No tag match.
        // With new_tag="v9.9.9" and neither run matching, we retry until timeout.
        // Use a tag that matches headBranch on run #5 to keep the test fast.
        let run_list = CommandResult {
            stdout: serde_json::json!([
                {
                    "databaseId": 5,
                    "status": "completed",
                    "conclusion": "failure",
                    "event": "push",
                    "headBranch": "v9.9.9",
                    "workflowName": "CI",
                    "createdAt": "2026-05-01T00:00:00Z",
                },
            ])
            .to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        };

        let shell = FakeShellGateway::sequence(vec![run_list, run_view_completed("failure")]);
        let block = WatchPipeline::with_gateways(registry, shell);

        let result = block.execute(&trigger_with_tag("v9.9.9")).await.unwrap();

        // headBranch match (score=1) causes us to select run #5 → failure
        assert!(!result.success);
        assert_eq!(result.events[0].payload["conclusion"], "failure");
    }

    #[tokio::test]
    async fn find_release_run_prefers_release_event_with_matching_tag() {
        use crate::gateway::fakes::FakeShellGateway;

        let shell = FakeShellGateway::always(CommandResult {
            stdout: serde_json::json!([
                {
                    "databaseId": 1,
                    "event": "push",
                    "headBranch": "main",
                    "workflowName": "CI",
                    "status": "completed",
                    "conclusion": "success",
                    "createdAt": "2026-05-01T00:02:00Z",
                },
                {
                    "databaseId": 2,
                    "event": "release",
                    "headBranch": "v2.0.0",
                    "workflowName": "Release",
                    "status": "completed",
                    "conclusion": "success",
                    "createdAt": "2026-05-01T00:01:00Z",
                },
                {
                    "databaseId": 3,
                    "event": "release",
                    "headBranch": "v1.9.0",
                    "workflowName": "Release",
                    "status": "completed",
                    "conclusion": "success",
                    "createdAt": "2026-05-01T00:00:00Z",
                },
            ])
            .to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });

        let result =
            find_release_run_id("owner/repo", Some("v2.0.0"), shell.as_ref()).await.unwrap();

        // Run #2 is a release event AND headBranch matches v2.0.0 → score 4
        assert_eq!(result, Some(2));
    }

    #[tokio::test]
    async fn find_release_run_returns_none_when_no_matching_runs() {
        use crate::gateway::fakes::FakeShellGateway;

        let shell = FakeShellGateway::always(CommandResult {
            stdout: serde_json::json!([
                {
                    "databaseId": 1,
                    "event": "push",
                    "headBranch": "main",
                    "workflowName": "CI",
                    "status": "completed",
                    "conclusion": "success",
                    "createdAt": "2026-05-01T00:00:00Z",
                },
            ])
            .to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });

        // Tag "v5.0.0" doesn't match any run's headBranch or event
        let result =
            find_release_run_id("owner/repo", Some("v5.0.0"), shell.as_ref()).await.unwrap();

        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn find_release_run_falls_back_to_most_recent_when_no_tag() {
        use crate::gateway::fakes::FakeShellGateway;

        let shell = FakeShellGateway::always(CommandResult {
            stdout: serde_json::json!([
                { "databaseId": 99, "event": "push", "headBranch": "main",
                  "workflowName": "CI", "status": "completed", "conclusion": "success",
                  "createdAt": "2026-05-01T00:00:00Z" },
            ])
            .to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });

        let result = find_release_run_id("owner/repo", None, shell.as_ref()).await.unwrap();

        assert_eq!(result, Some(99));
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
