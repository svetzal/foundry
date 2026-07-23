use std::path::PathBuf;
use std::time::Duration;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::throttle::Throttle;

use crate::gateway::{
    AgentAccess, AgentGateway, AgentRequest, ModelTier, ReasoningEffort, ShellGateway,
};

mod cut_release;
mod execute_release;
mod tag_verify;

pub use cut_release::CutRelease;
pub use execute_release::ExecuteRelease;

// Re-export for the require_project! macro (which expands to super::read_registry/require_project)
// and SimulatedSuccess trait. Submodules (cut_release, execute_release) access these via super::.
#[allow(unused_imports)]
use super::SimulatedSuccess;
#[allow(unused_imports)]
use super::read_registry;
#[allow(unused_imports)]
use super::require_project;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// Typed input for the release agent invocation.
pub(super) struct ReleaseInput {
    pub project: String,
    pub project_path: PathBuf,
    pub prompt: String,
}

/// Typed output from the release agent invocation.
pub(super) struct ReleaseOutput {
    pub success: bool,
    pub new_tag: Option<String>,
    pub summary: String,
    pub raw_output: Option<String>,
    pub exit_code: Option<i32>,
}

// ---------------------------------------------------------------------------
// Event helper — single source of truth for ReleaseCompleted payload shape
// ---------------------------------------------------------------------------

/// Build a `ReleaseCompleted` event.
///
/// `cve` is omitted from the payload entirely when `None` (manual releases have no CVE key).
/// `dry_run` is omitted when `None` (real executions, not dry runs).
pub(super) fn release_completed_event(
    project: &str,
    throttle: Throttle,
    release_type: &str,
    new_tag: Option<&str>,
    success: bool,
    cve: Option<&str>,
    dry_run: Option<bool>,
) -> Event {
    let mut payload = serde_json::json!({
        "release": release_type,
        "new_tag": new_tag,
        "success": success,
    });
    #[allow(
        clippy::expect_used,
        reason = "payload was just constructed via json!({...}) above and is always an Object"
    )]
    let obj = payload.as_object_mut().expect("release_completed payload is always an object");
    if let Some(dr) = dry_run {
        obj.insert("dry_run".to_string(), serde_json::json!(dr));
    }
    if let Some(c) = cve {
        obj.insert("cve".to_string(), serde_json::json!(c));
    }
    Event::new(EventType::ReleaseCompleted, project.to_string(), throttle, payload)
}

// ---------------------------------------------------------------------------
// Shared release runner
// ---------------------------------------------------------------------------

/// Generous timeout for Claude CLI — release tasks can take several minutes.
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(900);

/// Invoke the Claude agent for a release and return structured output.
///
/// Verifies `AGENTS.md` exists, builds the agent request, invokes the agent,
/// and verifies the tag points at HEAD.
///
/// Returns `Err` if `AGENTS.md` is missing — callers convert this to a failure result.
pub(super) async fn run_release(
    agent: &dyn AgentGateway,
    shell: &dyn ShellGateway,
    input: ReleaseInput,
) -> anyhow::Result<ReleaseOutput> {
    let project_dir = &input.project_path;

    let agents_md = project_dir.join("AGENTS.md");
    if !agents_md.exists() {
        tracing::warn!(path = %agents_md.display(), "AGENTS.md not found, skipping release");
        anyhow::bail!("AGENTS.md not found at {}; cannot invoke Claude CLI", agents_md.display());
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
        provider: None,
        env: Vec::new(),
        timeout: CLAUDE_TIMEOUT,
    };

    let run_result = agent.invoke(&request).await;
    let (raw_output, exit_code) = agent_run_metadata(&run_result);
    let (cli_success, new_tag, cli_summary) = interpret_agent_result(run_result);

    let (success, summary) = tag_verify::check_tag_at_head(
        cli_success,
        new_tag.as_deref(),
        cli_summary,
        project_dir,
        shell,
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
}

// ---------------------------------------------------------------------------
// Agent result helpers
// ---------------------------------------------------------------------------

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
