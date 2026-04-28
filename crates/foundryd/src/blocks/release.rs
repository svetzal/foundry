use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlockResult};
use foundry_core::work_block::{ComposedStep, EventAdapter, OutputMapper, WorkBlock};

use crate::gateway::{
    AgentAccess, AgentCapability, AgentGateway, AgentRequest, ClaudeAgentGateway, ShellGateway,
};

// ---------------------------------------------------------------------------
// AgentRelease WorkBlock — shared release behavior
// ---------------------------------------------------------------------------

/// Typed input for the agent release work block.
pub struct ReleaseInput {
    pub project_path: PathBuf,
    pub prompt: String,
}

/// Typed output from the agent release work block.
pub struct ReleaseOutput {
    pub success: bool,
    pub new_tag: Option<String>,
    pub summary: String,
    pub raw_output: Option<String>,
    pub exit_code: Option<i32>,
}

/// Pure behavior: verify AGENTS.md exists, invoke Claude agent with a prompt,
/// extract version tag from output, return structured result.
///
/// This is the shared logic previously duplicated between `CutRelease` and
/// `ExecuteRelease`.
pub struct AgentRelease {
    agent: Arc<dyn AgentGateway>,
}

impl AgentRelease {
    /// Generous timeout for Claude CLI — release tasks can take several minutes.
    const CLAUDE_TIMEOUT: Duration = Duration::from_secs(900); // 15 minutes

    pub fn new(agent: Arc<dyn AgentGateway>) -> Self {
        Self { agent }
    }
}

impl WorkBlock for AgentRelease {
    type Input = ReleaseInput;
    type Output = ReleaseOutput;

    fn name(&self) -> &'static str {
        "Agent Release"
    }

    fn execute(
        &self,
        input: Self::Input,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<Self::Output>> + Send + '_>> {
        Box::pin(async move {
            let project_dir = &input.project_path;

            // Verify AGENTS.md exists — required by Claude Code for agentic automation.
            let agents_md = project_dir.join("AGENTS.md");
            if !agents_md.exists() {
                tracing::warn!(path = %agents_md.display(), "AGENTS.md not found, skipping release");
                anyhow::bail!(
                    "AGENTS.md not found at {}; cannot invoke Claude CLI",
                    agents_md.display()
                );
            }

            tracing::info!(prompt = %input.prompt, "invoking claude CLI for release");

            let request = AgentRequest {
                prompt: input.prompt,
                working_dir: project_dir.clone(),
                access: AgentAccess::Full,
                capability: AgentCapability::Coding,
                agent_file: None,
                timeout: Self::CLAUDE_TIMEOUT,
            };

            let run_result = self.agent.invoke(&request).await;

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
                        "Release completed{}",
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
                        format!(
                            "Claude CLI exited with code {}; stderr: {first_stderr}",
                            r.exit_code
                        ),
                    )
                }
                Err(err) => {
                    tracing::warn!(error = %err, "claude CLI not available or failed to spawn");
                    (false, None, format!("claude CLI unavailable: {err}"))
                }
            };

            tracing::info!(
                new_tag = new_tag.as_deref().unwrap_or("(not detected)"),
                success = cli_success,
                "release step completed"
            );

            Ok(ReleaseOutput {
                success: cli_success,
                new_tag,
                summary: cli_summary,
                raw_output,
                exit_code,
            })
        })
    }
}

// ---------------------------------------------------------------------------
// VulnReleaseAdapter — CutRelease trigger path (MainBranchAudited, dirty=false)
// ---------------------------------------------------------------------------

/// Adapts a `MainBranchAudited` event into a [`ReleaseInput`] for the
/// vulnerability-driven release path.
///
/// Returns `None` when `dirty=true` (self-filter: only acts on clean branches).
pub struct VulnReleaseAdapter {
    registry: Arc<Registry>,
}

impl VulnReleaseAdapter {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl EventAdapter<ReleaseInput> for VulnReleaseAdapter {
    fn adapt(&self, trigger: &Event) -> Option<ReleaseInput> {
        let dirty = trigger
            .payload
            .get("dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        if dirty {
            tracing::info!("main branch is dirty, skipping release");
            return None;
        }

        let project = &trigger.project;
        let cve = trigger
            .payload
            .get("cve")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let entry = self.registry.find_project(project)?;
        let project_path = PathBuf::from(&entry.path);

        let prompt = format!(
            "Cut a patch release for {project} fixing {cve}. \
             Create a changelog entry, bump the patch version, tag the release, and push."
        );

        tracing::info!(%project, %cve, "cutting patch release");

        Some(ReleaseInput {
            project_path,
            prompt,
        })
    }
}

// ---------------------------------------------------------------------------
// ManualReleaseAdapter — ExecuteRelease trigger path (ReleaseRequested)
// ---------------------------------------------------------------------------

/// Adapts a `ReleaseRequested` event into a [`ReleaseInput`] for the
/// manual release path.
///
/// Returns `None` when `entry.actions.release` is false.
pub struct ManualReleaseAdapter {
    registry: Arc<Registry>,
}

impl ManualReleaseAdapter {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl EventAdapter<ReleaseInput> for ManualReleaseAdapter {
    fn adapt(&self, trigger: &Event) -> Option<ReleaseInput> {
        let project = &trigger.project;

        let Some(entry) = self.registry.find_project(project) else {
            tracing::warn!(project = %project, "project not found in registry");
            return None;
        };

        if !entry.actions.release {
            tracing::info!(%project, "release action disabled, skipping");
            return None;
        }

        let project_path = PathBuf::from(&entry.path);
        let bump = trigger
            .payload
            .get("bump")
            .and_then(serde_json::Value::as_str)
            .map(String::from);

        let bump_instruction = match &bump {
            Some(b) => format!("The version bump type is {b}."),
            None => {
                "Determine the appropriate version bump from the changelog and unreleased changes."
                    .to_string()
            }
        };

        let prompt = format!(
            "Release {project}. Follow the release process documented in AGENTS.md exactly.\n\
             {bump_instruction}\n\
             Complete all steps: run quality gates, update the changelog, bump the version in all \
             locations, commit, tag, and push. Output the new version tag on a line by itself (e.g. v1.2.3)."
        );

        tracing::info!(%project, bump = bump.as_deref().unwrap_or("auto"), "executing release");

        Some(ReleaseInput {
            project_path,
            prompt,
        })
    }
}

// ---------------------------------------------------------------------------
// ReleaseOutputMapper — shared output mapping for both release paths
// ---------------------------------------------------------------------------

/// Maps [`ReleaseOutput`] into a [`TaskBlockResult`] with a `ReleaseCompleted` event.
///
/// Parameterized with `release_type` (e.g. "patch" or "manual") and optional
/// extra payload fields (e.g. CVE for vulnerability releases).
/// Closure type for producing extra payload fields from a trigger event.
type ExtraPayloadFn = Box<dyn Fn(&Event) -> serde_json::Value + Send + Sync>;

pub struct ReleaseOutputMapper {
    release_type: &'static str,
    /// Extra payload fields merged into every `ReleaseCompleted` event.
    extra_payload: Option<ExtraPayloadFn>,
}

impl ReleaseOutputMapper {
    pub fn new(release_type: &'static str) -> Self {
        Self {
            release_type,
            extra_payload: None,
        }
    }

    #[must_use]
    pub fn with_extra_payload(
        mut self,
        f: impl Fn(&Event) -> serde_json::Value + Send + Sync + 'static,
    ) -> Self {
        self.extra_payload = Some(Box::new(f));
        self
    }

    fn build_payload(
        &self,
        trigger: &Event,
        success: bool,
        new_tag: Option<&String>,
    ) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "release": self.release_type,
            "new_tag": new_tag,
            "success": success,
        });

        if let Some(extra) = &self.extra_payload {
            if let (Some(base), Some(extra)) = (payload.as_object_mut(), extra(trigger).as_object())
            {
                for (k, v) in extra {
                    base.insert(k.clone(), v.clone());
                }
            }
        }

        payload
    }
}

impl OutputMapper<ReleaseOutput> for ReleaseOutputMapper {
    fn map(&self, output: ReleaseOutput, trigger: &Event) -> TaskBlockResult {
        let payload = self.build_payload(trigger, output.success, output.new_tag.as_ref());

        TaskBlockResult {
            events: vec![Event::new(
                EventType::ReleaseCompleted,
                trigger.project.clone(),
                trigger.throttle,
                payload,
            )],
            success: output.success,
            summary: output.summary,
            raw_output: output.raw_output,
            exit_code: output.exit_code,
            audit_artifacts: vec![],
        }
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        let mut payload = serde_json::json!({
            "release": self.release_type,
            "success": true,
            "dry_run": true,
        });

        if let Some(extra) = &self.extra_payload {
            if let (Some(base), Some(extra)) = (payload.as_object_mut(), extra(trigger).as_object())
            {
                for (k, v) in extra {
                    base.insert(k.clone(), v.clone());
                }
            }
        }

        vec![Event::new(
            EventType::ReleaseCompleted,
            trigger.project.clone(),
            trigger.throttle,
            payload,
        )]
    }
}

// ---------------------------------------------------------------------------
// VulnReleaseMapper — specialized mapper for vulnerability releases
// ---------------------------------------------------------------------------

/// Dry-run mapper for the vulnerability release path that respects
/// the `dirty` self-filter — emits no events when dirty.
pub struct VulnReleaseMapper {
    inner: ReleaseOutputMapper,
}

impl VulnReleaseMapper {
    pub fn new() -> Self {
        Self {
            inner: ReleaseOutputMapper::new("patch").with_extra_payload(|trigger| {
                let cve = trigger
                    .payload
                    .get("cve")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                serde_json::json!({ "cve": cve })
            }),
        }
    }
}

impl OutputMapper<ReleaseOutput> for VulnReleaseMapper {
    fn map(&self, output: ReleaseOutput, trigger: &Event) -> TaskBlockResult {
        self.inner.map(output, trigger)
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
        self.inner.dry_run_events(trigger)
    }
}

// ---------------------------------------------------------------------------
// Composed step constructors
// ---------------------------------------------------------------------------

/// The composed `TaskBlock` type for the vulnerability-driven release path
/// (replaces `CutRelease`).
pub type CutReleaseStep = ComposedStep<AgentRelease, VulnReleaseAdapter, VulnReleaseMapper>;

/// The composed `TaskBlock` type for the manual release path
/// (replaces `ExecuteRelease`).
pub type ExecuteReleaseStep = ComposedStep<AgentRelease, ManualReleaseAdapter, ReleaseOutputMapper>;

/// Build the composed "Cut Release" step (vulnerability flow).
///
/// Sinks on `MainBranchAudited`, skips when dirty, invokes agent for patch release.
pub fn cut_release_step(registry: Arc<Registry>) -> CutReleaseStep {
    let shell: Arc<dyn ShellGateway> = Arc::new(crate::gateway::ProcessShellGateway);
    let agent: Arc<dyn AgentGateway> = Arc::new(ClaudeAgentGateway::new(shell));

    ComposedStep::new(
        "Cut Release",
        BlockKind::Mutator,
        vec![EventType::MainBranchAudited],
        AgentRelease::new(agent),
        VulnReleaseAdapter::new(registry),
        VulnReleaseMapper::new(),
    )
}

/// Build the composed "Execute Release" step (manual flow).
///
/// Sinks on `ReleaseRequested`, checks action flag, invokes agent following AGENTS.md.
pub fn execute_release_step(
    agent: Arc<dyn AgentGateway>,
    registry: Arc<Registry>,
) -> ExecuteReleaseStep {
    ComposedStep::new(
        "Execute Release",
        BlockKind::Mutator,
        vec![EventType::ReleaseRequested],
        AgentRelease::new(agent),
        ManualReleaseAdapter::new(registry),
        ReleaseOutputMapper::new("manual"),
    )
}

/// Build a "Cut Release" step with a test agent (for unit/integration tests).
#[cfg(test)]
pub fn cut_release_step_with_agent(
    agent: Arc<dyn AgentGateway>,
    registry: Arc<Registry>,
) -> CutReleaseStep {
    ComposedStep::new(
        "Cut Release",
        BlockKind::Mutator,
        vec![EventType::MainBranchAudited],
        AgentRelease::new(agent),
        VulnReleaseAdapter::new(registry),
        VulnReleaseMapper::new(),
    )
}

// ---------------------------------------------------------------------------
// extract_version_tag — shared utility
// ---------------------------------------------------------------------------

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, Registry};
    use foundry_core::task_block::TaskBlock;
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;

    use super::super::test_helpers;
    use super::*;

    fn empty_registry() -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![],
        })
    }

    // --- CutRelease (composed) tests ---

    #[tokio::test]
    async fn skips_when_dirty() {
        let block = cut_release_step(empty_registry());
        let trigger = Event::new(
            EventType::MainBranchAudited,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": true, "cve": "CVE-2026-1234" }),
        );

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("Skipped"));
    }

    #[tokio::test]
    async fn fails_when_project_not_in_registry() {
        let agent = FakeAgentGateway::success();
        let block = cut_release_step_with_agent(agent, empty_registry());
        let trigger = Event::new(
            EventType::MainBranchAudited,
            "unknown-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-1234" }),
        );

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success); // adapter returns None → skip
        assert!(result.events.is_empty());
        assert!(result.summary.contains("Skipped"));
    }

    #[tokio::test]
    async fn fails_when_agents_md_missing() {
        // Use a path that definitely doesn't have AGENTS.md.
        let (entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", false);
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::success();
        let block = cut_release_step_with_agent(agent, registry);
        let trigger = Event::new(
            EventType::MainBranchAudited,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-1234" }),
        );

        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        // AgentRelease returns Err when AGENTS.md missing — default map_error
        // produces a failure with no events, stopping the chain.
        assert!(result.events.is_empty());
        assert!(result.summary.contains("AGENTS.md not found"));
    }

    #[tokio::test]
    async fn successful_release_emits_release_completed() {
        let (entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        let registry = test_helpers::registry_with_entry(entry);
        let agent =
            FakeAgentGateway::success_with("Release complete! Tagged as v1.2.3 and pushed.");
        let block = cut_release_step_with_agent(agent.clone(), registry);
        let trigger = Event::new(
            EventType::MainBranchAudited,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-1234" }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
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
    async fn release_failure_emits_release_completed_with_success_false() {
        let (entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::failure("Claude CLI failed");
        let block = cut_release_step_with_agent(agent, registry);
        let trigger = Event::new(
            EventType::MainBranchAudited,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-1234" }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
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

    // --- ExecuteRelease (composed) tests ---

    fn release_actions() -> ActionFlags {
        ActionFlags {
            release: true,
            ..ActionFlags::default()
        }
    }

    #[tokio::test]
    async fn execute_release_skips_when_action_disabled() {
        let (entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::success();
        let block = execute_release_step(agent, registry);
        let trigger = Event::new(
            EventType::ReleaseRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("Skipped"));
    }

    #[tokio::test]
    async fn execute_release_fails_when_project_not_in_registry() {
        let agent = FakeAgentGateway::success();
        let block = execute_release_step(agent, empty_registry());
        let trigger = Event::new(
            EventType::ReleaseRequested,
            "unknown-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success); // adapter returns None → skip
        assert!(result.events.is_empty());
        assert!(result.summary.contains("Skipped"));
    }

    #[tokio::test]
    async fn execute_release_fails_when_agents_md_missing() {
        let (mut entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", false);
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::success();
        let block = execute_release_step(agent, registry);
        let trigger = Event::new(
            EventType::ReleaseRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        // AgentRelease returns Err when AGENTS.md missing — default map_error
        // produces a failure with no events, stopping the chain.
        assert!(result.events.is_empty());
        assert!(result.summary.contains("AGENTS.md not found"));
    }

    #[tokio::test]
    async fn execute_release_success_emits_release_completed() {
        let (mut entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::success_with("Release complete!\nv2.0.0\nAll steps done.");
        let block = execute_release_step(agent.clone(), registry);
        let trigger = Event::new(
            EventType::ReleaseRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "bump": "minor" }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(result.events[0].payload["new_tag"], "v2.0.0");
        assert_eq!(result.events[0].payload["success"], true);
        assert_eq!(result.events[0].payload["release"], "manual");

        // Verify the agent was invoked with expected capability and access.
        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].capability, AgentCapability::Coding);
        assert_eq!(invocations[0].access, AgentAccess::Full);
        assert!(invocations[0].prompt.contains("minor"));
        assert!(invocations[0].prompt.contains("AGENTS.md"));
    }

    #[tokio::test]
    async fn execute_release_auto_bump_when_no_bump_specified() {
        let (mut entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::success_with("v1.3.0");
        let block = execute_release_step(agent.clone(), registry);
        let trigger = Event::new(
            EventType::ReleaseRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);

        let invocations = agent.invocations();
        assert!(invocations[0].prompt.contains("Determine the appropriate version bump"));
    }

    #[tokio::test]
    async fn execute_release_failure_emits_release_completed_with_success_false() {
        let (mut entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);
        let agent = FakeAgentGateway::failure("release failed");
        let block = execute_release_step(agent, registry);
        let trigger = Event::new(
            EventType::ReleaseRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(result.events[0].payload["success"], false);
    }
}
