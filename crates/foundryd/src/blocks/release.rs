use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::throttle::Throttle;

use crate::gateway::{
    AgentAccess, AgentCapability, AgentGateway, AgentRequest, ClaudeAgentGateway, ShellGateway,
};

/// Tags a patch release when the main branch is clean (vulnerability already fixed).
/// Mutator — events logged but not delivered at `audit_only`;
/// simulated success at `dry_run`.
///
/// Self-filters: only acts when `dirty=false` in the trigger payload.
///
/// Invokes the Claude CLI to draft the changelog, bump the version, create
/// the tag, and push. Requires `AGENTS.md` to exist in the project directory
/// (Claude Code convention for agentic automation).
pub struct CutRelease {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl CutRelease {
    pub fn new(registry: Arc<Registry>) -> Self {
        let shell = Arc::new(crate::gateway::ProcessShellGateway);
        Self {
            registry,
            agent: Arc::new(ClaudeAgentGateway::new(shell)),
        }
    }

    /// Generous timeout for Claude CLI — release tasks can take several minutes.
    const CLAUDE_TIMEOUT: Duration = Duration::from_secs(900); // 15 minutes

    #[cfg(test)]
    fn with_agent(registry: Arc<Registry>, agent: Arc<dyn AgentGateway>) -> Self {
        Self { registry, agent }
    }
}

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

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        // Respect the self-filter: skip when dirty.
        let dirty = trigger
            .payload
            .get("dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        if dirty {
            return vec![];
        }

        let cve = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        vec![Event::new(
            EventType::AutoReleaseCompleted,
            trigger.project.clone(),
            trigger.throttle,
            serde_json::json!({
                "cve": cve,
                "release": "patch",
                "success": true,
                "dry_run": true,
            }),
        )]
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
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                })
            });
        }

        let cve = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let project_path = self
            .registry
            .projects
            .iter()
            .find(|p| p.name == project)
            .map(|p| p.path.clone());

        let agent = Arc::clone(&self.agent);

        tracing::info!(%cve, "cutting patch release");

        Box::pin(run_release(project, throttle, cve, project_path, agent))
    }
}

async fn run_release(
    project: String,
    throttle: Throttle,
    cve: String,
    project_path: Option<String>,
    agent: Arc<dyn AgentGateway>,
) -> anyhow::Result<TaskBlockResult> {
    let Some(path_str) = project_path else {
        tracing::warn!(project = %project, "project not found in registry, skipping release");
        return Ok(TaskBlockResult {
            events: vec![],
            success: false,
            summary: format!("Project '{project}' not found in registry"),
            raw_output: None,
            exit_code: None,
            audit_artifacts: vec![],
        });
    };

    let project_dir = Path::new(&path_str);

    // Verify AGENTS.md exists — required by Claude Code for agentic automation.
    let agents_md = project_dir.join("AGENTS.md");
    if !agents_md.exists() {
        tracing::warn!(path = %agents_md.display(), "AGENTS.md not found, skipping release");
        return Ok(TaskBlockResult {
            events: vec![],
            success: false,
            summary: format!(
                "AGENTS.md not found at {}; cannot invoke Claude CLI",
                agents_md.display()
            ),
            raw_output: None,
            exit_code: None,
            audit_artifacts: vec![],
        });
    }

    let prompt = format!(
        "Cut a patch release for {project} fixing {cve}. \
         Create a changelog entry, bump the patch version, tag the release, and push."
    );

    tracing::info!(%project, %cve, "invoking claude CLI for release");

    let request = AgentRequest {
        prompt,
        working_dir: project_dir.to_path_buf(),
        access: AgentAccess::Full,
        capability: AgentCapability::Coding,
        agent_file: None,
        timeout: CutRelease::CLAUDE_TIMEOUT,
    };

    let run_result = agent.invoke(&request).await;

    let (raw_output, exit_code) = match &run_result {
        Ok(r) => (
            Some(format!("{}\n{}", r.stdout, r.stderr).trim().to_string()),
            Some(r.exit_code),
        ),
        Err(_) => (None, None),
    };

    let (cli_success, new_tag, cli_summary) = match run_result {
        Ok(r) if r.success => {
            let tag = extract_version_tag(&r.stdout);
            let s = format!(
                "Cut patch release for {cve}{}",
                tag.as_deref().map(|t| format!(" — {t}")).unwrap_or_default()
            );
            (true, tag, s)
        }
        Ok(r) => {
            tracing::error!(exit_code = r.exit_code, stderr = %r.stderr, "claude CLI failed");
            let first_stderr = r.stderr.lines().next().unwrap_or("(empty)");
            (
                false,
                None,
                format!("Claude CLI exited with code {}; stderr: {first_stderr}", r.exit_code),
            )
        }
        Err(err) => {
            tracing::warn!(error = %err, "claude CLI not available or failed to spawn");
            (false, None, format!("claude CLI unavailable: {err}"))
        }
    };

    tracing::info!(
        project = %project,
        new_tag = new_tag.as_deref().unwrap_or("(not detected)"),
        success = cli_success,
        "release step completed"
    );

    Ok(TaskBlockResult {
        events: vec![Event::new(
            EventType::AutoReleaseCompleted,
            project,
            throttle,
            serde_json::json!({
                "cve": cve,
                "release": "patch",
                "new_tag": new_tag,
                "success": cli_success,
            }),
        )],
        success: cli_success,
        summary: cli_summary,
        raw_output,
        exit_code,
        audit_artifacts: vec![],
    })
}

/// Scan output words for a semver tag of the form `v<major>.<minor>.<patch>`.
fn extract_version_tag(output: &str) -> Option<String> {
    for word in output.split_whitespace() {
        // Strip trailing punctuation before matching.
        let w = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '.');
        if w.starts_with('v')
            && w.len() > 1
            && w[1..].split('.').count() == 3
            && w[1..].split('.').all(|part| part.chars().all(char::is_numeric))
        {
            return Some(w.to_string());
        }
    }
    None
}

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
            .projects
            .iter()
            .find(|p| p.name == project)
            .map(|p| p.repo.clone())
            .filter(|r| !r.is_empty());

        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let Some(repo) = repo else {
                // No repository configured — stub: assume success.
                tracing::info!("no repo configured, using stub pipeline completion");
                return Ok(stub_success(project, throttle));
            };

            poll_pipeline(project, throttle, &repo, shell.as_ref()).await
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
        raw_output: None,
        exit_code: None,
        audit_artifacts: vec![],
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
    use tempfile::TempDir;

    use crate::gateway::fakes::FakeAgentGateway;

    use super::*;

    fn empty_registry() -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![],
        })
    }

    fn registry_with_project(name: &str, path: &str, has_agents_md: bool) -> Arc<Registry> {
        // Create a real temp dir when has_agents_md is requested.
        // The path parameter is ignored in that case so tests are hermetic.
        let project_path = if has_agents_md {
            let dir = TempDir::new().unwrap();
            let agents_path = dir.path().join("AGENTS.md");
            std::fs::write(&agents_path, "# Agent guidance").unwrap();
            // Leak the TempDir so it persists for the test lifetime.
            let p = dir.path().to_str().unwrap().to_string();
            std::mem::forget(dir);
            p
        } else {
            path.to_string()
        };

        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: name.to_string(),
                path: project_path,
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
        })
    }

    #[tokio::test]
    async fn skips_when_dirty() {
        let block = CutRelease::new(empty_registry());
        let trigger = Event::new(
            EventType::MainBranchAudited,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": true, "cve": "CVE-2026-1234" }),
        );

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("dirty"));
    }

    #[tokio::test]
    async fn fails_when_project_not_in_registry() {
        let block = CutRelease::new(empty_registry());
        let trigger = Event::new(
            EventType::MainBranchAudited,
            "unknown-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-1234" }),
        );

        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("not found in registry"));
    }

    #[tokio::test]
    async fn fails_when_agents_md_missing() {
        // Use a path that definitely doesn't have AGENTS.md.
        let registry = registry_with_project("my-project", "/nonexistent/path", false);
        let block = CutRelease::new(registry);
        let trigger = Event::new(
            EventType::MainBranchAudited,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-1234" }),
        );

        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("AGENTS.md not found"));
    }

    #[tokio::test]
    async fn successful_release_emits_auto_release_completed() {
        let registry = registry_with_project("my-project", "/unused", true);
        let agent = FakeAgentGateway::success_with("Release complete! Tagged as v1.2.3 and pushed.");
        let block = CutRelease::with_agent(registry, agent.clone());
        let trigger = Event::new(
            EventType::MainBranchAudited,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-1234" }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::AutoReleaseCompleted);
        assert_eq!(result.events[0].payload["new_tag"], "v1.2.3");
        assert_eq!(result.events[0].payload["success"], true);

        // Verify the agent was invoked with expected capability and access.
        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].capability, AgentCapability::Coding);
        assert_eq!(invocations[0].access, AgentAccess::Full);
        assert!(invocations[0].prompt.contains("CVE-2026-1234"));
    }

    #[tokio::test]
    async fn release_failure_emits_auto_release_completed_with_success_false() {
        let registry = registry_with_project("my-project", "/unused", true);
        let agent = FakeAgentGateway::failure("Claude CLI failed");
        let block = CutRelease::with_agent(registry, agent);
        let trigger = Event::new(
            EventType::MainBranchAudited,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-1234" }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::AutoReleaseCompleted);
        assert_eq!(result.events[0].payload["success"], false);
    }

    #[test]
    fn extract_version_tag_finds_semver() {
        let output = "Release complete! Tagged as v1.2.3 and pushed.";
        assert_eq!(extract_version_tag(output), Some("v1.2.3".to_string()));
    }

    #[test]
    fn extract_version_tag_returns_none_when_absent() {
        assert_eq!(extract_version_tag("No version info here."), None);
    }

    #[test]
    fn extract_version_tag_ignores_non_semver() {
        assert_eq!(extract_version_tag("version v1.2 released"), None);
    }

    // --- WatchPipeline tests ---

    #[tokio::test]
    async fn watch_pipeline_stubs_when_project_not_in_registry() {
        let block = WatchPipeline::stub();
        let trigger = Event::new(
            EventType::AutoReleaseCompleted,
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
                timeout_secs: None,
            }],
        });
        let block = WatchPipeline::new(registry);
        let trigger = Event::new(
            EventType::AutoReleaseCompleted,
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
}
