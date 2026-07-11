use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_sdk::event::EventType;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::BlockKind;
use foundry_sdk::work_block::{ComposedStep, WorkBlock};

use crate::gateway::{
    AgentAccess, AgentGateway, AgentRequest, ClaudeAgentGateway, ModelTier, ReasoningEffort,
    ShellGateway,
};

mod adapters;
mod mappers;
mod tag_verify;

pub use adapters::{ManualReleaseAdapter, VulnReleaseAdapter};
pub use mappers::{ReleaseOutputMapper, VulnReleaseMapper};

// ---------------------------------------------------------------------------
// AgentRelease WorkBlock — shared release behavior
// ---------------------------------------------------------------------------

/// Typed input for the agent release work block.
pub struct ReleaseInput {
    pub project: String,
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
pub struct AgentRelease {
    agent: Arc<dyn AgentGateway>,
    shell: Arc<dyn ShellGateway>,
}

impl AgentRelease {
    /// Generous timeout for Claude CLI — release tasks can take several minutes.
    const CLAUDE_TIMEOUT: Duration = Duration::from_secs(900); // 15 minutes

    pub fn new(agent: Arc<dyn AgentGateway>) -> Self {
        Self {
            agent,
            shell: Arc::new(crate::gateway::ProcessShellGateway),
        }
    }

    /// Construct with injected gateways (for tests).
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_gateways(agent: Arc<dyn AgentGateway>, shell: Arc<dyn ShellGateway>) -> Self {
        Self { agent, shell }
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
                project: input.project.clone(),
                working_dir: project_dir.clone(),
                access: AgentAccess::Full,
                tier: ModelTier::Balanced,
                effort: ReasoningEffort::Medium,
                agent_file: None,
                // Release runs on the daemon's default provider; the release
                // pipeline carries no per-request provider override today.
                provider: None,
                timeout: Self::CLAUDE_TIMEOUT,
            };

            let run_result = self.agent.invoke(&request).await;
            let (raw_output, exit_code) = agent_run_metadata(&run_result);
            let (cli_success, new_tag, cli_summary) = interpret_agent_result(run_result);

            // If the agent succeeded and extracted a tag, verify the tag points at HEAD.
            let (success, summary) = tag_verify::check_tag_at_head(
                cli_success,
                new_tag.as_deref(),
                cli_summary,
                project_dir,
                self.shell.as_ref(),
            )
            .await;

            tracing::info!(
                new_tag = new_tag.as_deref().unwrap_or("(not detected)"),
                success,
                "release step completed"
            );

            Ok(ReleaseOutput {
                success,
                new_tag,
                summary,
                raw_output,
                exit_code,
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Composed step type aliases
// ---------------------------------------------------------------------------

/// The composed `TaskBlock` type for the vulnerability-driven release path.
pub type CutReleaseStep = ComposedStep<AgentRelease, VulnReleaseAdapter, VulnReleaseMapper>;

/// The composed `TaskBlock` type for the manual release path.
pub type ExecuteReleaseStep = ComposedStep<AgentRelease, ManualReleaseAdapter, ReleaseOutputMapper>;

/// Returns `true` when the trigger's `MainBranchAuditedPayload` has `dirty: true`
/// (or when the payload cannot be parsed — conservative: treat unknown as dirty).
///
/// Single source of truth used by both `VulnReleaseAdapter` (which skips execution)
/// and `VulnReleaseMapper` (which suppresses dry-run events) when the branch is dirty.
pub(super) fn skips_when_dirty(trigger: &foundry_sdk::event::Event) -> bool {
    trigger
        .parse_payload::<foundry_sdk::payload::MainBranchAuditedPayload>()
        .ok()
        .is_none_or(|p| p.dirty)
}

/// Build the composed "Cut Release" step (vulnerability flow).
pub fn cut_release_step(registry: Arc<RwLock<Registry>>) -> CutReleaseStep {
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
pub fn execute_release_step(
    agent: Arc<dyn AgentGateway>,
    registry: Arc<RwLock<Registry>>,
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
#[cfg(any(test, feature = "test-support"))]
pub fn cut_release_step_with_agent(
    agent: Arc<dyn AgentGateway>,
    registry: Arc<RwLock<Registry>>,
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

/// Build an "Execute Release" step with injected agent and shell gateways (for tag verification tests).
#[cfg(any(test, feature = "test-support"))]
pub fn execute_release_step_with_gateways(
    agent: Arc<dyn AgentGateway>,
    shell: Arc<dyn ShellGateway>,
    registry: Arc<RwLock<Registry>>,
) -> ExecuteReleaseStep {
    ComposedStep::new(
        "Execute Release",
        BlockKind::Mutator,
        vec![EventType::ReleaseRequested],
        AgentRelease::with_gateways(agent, shell),
        ManualReleaseAdapter::new(registry),
        ReleaseOutputMapper::new("manual"),
    )
}

// ---------------------------------------------------------------------------
// Agent result helpers
// ---------------------------------------------------------------------------

/// Extract `raw_output` and `exit_code` from a run result reference (before consuming it).
fn agent_run_metadata(
    run_result: &anyhow::Result<crate::gateway::AgentResponse>,
) -> (Option<String>, Option<i32>) {
    match run_result {
        Ok(r) => (
            Some(format!("{}\n{}", r.stdout, r.stderr).trim().to_string()),
            Some(r.exit_code),
        ),
        Err(_) => (None, None),
    }
}

/// Interpret a completed agent run into `(cli_success, new_tag, summary)`.
fn interpret_agent_result(
    run_result: anyhow::Result<crate::gateway::AgentResponse>,
) -> (bool, Option<String>, String) {
    match run_result {
        Ok(r) if r.success => {
            let tag = tag_verify::extract_version_tag(&r.stdout);
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
                format!("Claude CLI exited with code {}; stderr: {first_stderr}", r.exit_code),
            )
        }
        Err(err) => {
            tracing::warn!(error = %err, "claude CLI not available or failed to spawn");
            (false, None, format!("claude CLI unavailable: {err}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::{ActionFlags, Registry};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;

    use super::super::test_helpers;
    use super::*;

    fn empty_registry() -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        }))
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

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].tier, ModelTier::Balanced);
        assert_eq!(invocations[0].effort, ReasoningEffort::Medium);
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

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].tier, ModelTier::Balanced);
        assert_eq!(invocations[0].effort, ReasoningEffort::Medium);
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

    // --- Tag verification tests ---

    /// Build [`FakeShellGateway`] responses for git rev-parse calls.
    fn git_revparse_sequence(
        tag_commit: &str,
        head_commit: &str,
    ) -> Arc<dyn crate::gateway::ShellGateway> {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;

        FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: format!("{tag_commit}\n"),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: format!("{head_commit}\n"),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ])
    }

    #[tokio::test]
    async fn execute_release_fails_when_tag_not_at_head() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agent guidance").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        let mut entry = test_helpers::project_entry("my-project", dir.path().to_str().unwrap());
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);

        let agent = FakeAgentGateway::success_with("Release complete!\nv3.0.0\nAll done.");
        // tag points at a different commit than HEAD
        let shell = git_revparse_sequence("aaaa", "bbbb");
        let block = execute_release_step_with_gateways(agent, shell, registry);

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
        assert!(result.summary.contains("does not point at HEAD"));
    }

    #[tokio::test]
    async fn execute_release_succeeds_when_tag_at_head() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agent guidance").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        let mut entry = test_helpers::project_entry("my-project", dir.path().to_str().unwrap());
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);

        let agent = FakeAgentGateway::success_with("Release complete!\nv3.0.0\nAll done.");
        // tag and HEAD point at the same commit
        let shell = git_revparse_sequence("aaaa", "aaaa");
        let block = execute_release_step_with_gateways(agent, shell, registry);

        let trigger = Event::new(
            EventType::ReleaseRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(result.events[0].payload["success"], true);
        assert_eq!(result.events[0].payload["new_tag"], "v3.0.0");
    }

    #[tokio::test]
    async fn execute_release_fails_when_tag_missing_from_git() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;

        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agent guidance").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        let mut entry = test_helpers::project_entry("my-project", dir.path().to_str().unwrap());
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);

        let agent = FakeAgentGateway::success_with("v3.0.0");
        // First git rev-parse (tag) fails — tag does not exist
        let shell = FakeShellGateway::sequence(vec![CommandResult {
            stdout: String::new(),
            stderr: "fatal: ambiguous argument 'v3.0.0^{commit}'".to_string(),
            exit_code: 128,
            success: false,
        }]);
        let block = execute_release_step_with_gateways(agent, shell, registry);

        let trigger = Event::new(
            EventType::ReleaseRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events[0].payload["success"], false);
    }

    #[tokio::test]
    async fn tag_verification_skipped_when_no_git_dir() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;

        // AGENTS.md present but no .git → not a git repo → skip verification
        let (mut entry, _dir) = test_helpers::project_entry_with_agents_md("my-project", true);
        entry.actions = release_actions();
        let registry = test_helpers::registry_with_entry(entry);

        let agent = FakeAgentGateway::success_with("v4.0.0");
        // Shell gateway that would fail if called — proves verification was skipped
        let shell = FakeShellGateway::always(CommandResult {
            stdout: String::new(),
            stderr: "should not be called".to_string(),
            exit_code: 1,
            success: false,
        });
        let block = execute_release_step_with_gateways(agent, shell, registry);

        let trigger = Event::new(
            EventType::ReleaseRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = block.execute(&trigger).await.unwrap();

        // Should succeed because the git check is skipped (no .git dir)
        assert!(result.success);
        assert_eq!(result.events[0].payload["new_tag"], "v4.0.0");
    }
}
