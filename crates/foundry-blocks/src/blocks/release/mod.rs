use std::path::PathBuf;
use std::time::Duration;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::GitSyncFailure;
use foundry_sdk::throttle::Throttle;

use crate::gateway::{
    AgentAccess, AgentGateway, AgentRequest, ModelTier, ReasoningEffort, ShellGateway,
};

use super::checkout_sync::{BranchCheckOutcome, SyncOutcome, check_git_branch, sync_checkout};

mod checkout_report;
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
// Shared dispatch predicates
// ---------------------------------------------------------------------------

/// Whether `project` has the release action enabled in `registry`.
///
/// `ExecuteRelease` treats a disabled release — or an unknown project — as a
/// domain skip that emits no `ReleaseCompleted`, so this is also the condition
/// under which the work-item ledger may record a release item: recording one
/// for a release that will not run would leave it `running` with no terminal
/// to settle it.
pub(super) fn release_enabled(
    registry: &std::sync::Arc<std::sync::RwLock<foundry_sdk::registry::Registry>>,
    project: &str,
) -> bool {
    // Best-effort: a poisoned registry lock is indistinguishable from
    // "release disabled" to every caller here, but it is a distinct fault and
    // must be visible in the log rather than silently read as policy.
    match super::read_registry(registry) {
        Ok(guard) => guard.find_project(project).is_some_and(|entry| entry.actions.release),
        Err(error) => {
            tracing::warn!(error = %error, "registry lock poisoned while reading the release action");
            false
        }
    }
}

/// The CVE `CutRelease` will cut a release for on `trigger`, or `None` when it
/// will not run at all.
///
/// This is the block's `accepts()` predicate, named so the work-item ledger
/// records a release item exactly when an automatic release runs.
pub(super) fn cut_release_target(trigger: &Event) -> Option<String> {
    cut_release::cut_target(trigger)
}

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// Typed input for the release agent invocation.
pub(super) struct ReleaseInput {
    pub project: String,
    pub project_path: PathBuf,
    /// The registry's branch for the project. The checkout is brought level
    /// with `origin/<branch>` before the agent is asked to bump anything.
    pub branch: String,
    pub prompt: String,
    /// Trace this release belongs to, forwarded into the agent session so its
    /// token spend is attributed to the release workflow.
    pub trace_id: Option<String>,
}

/// Typed output from the release agent invocation.
pub(super) struct ReleaseOutput {
    pub success: bool,
    pub new_tag: Option<String>,
    pub summary: String,
    pub raw_output: Option<String>,
    pub exit_code: Option<i32>,
    /// Paths a failed release left uncommitted that were clean when it
    /// started, relative to the repository root. Empty on success.
    pub left_uncommitted: Vec<String>,
    /// Why the checkout could not be brought level with its remote. When set,
    /// the agent was never invoked and nothing was bumped, committed, tagged
    /// or published.
    pub sync_failure: Option<GitSyncFailure>,
    /// Commits fast-forwarded from `origin/<branch>` before the agent ran.
    /// `None` when the sync was refused or the checkout is not a git work
    /// tree.
    pub fast_forwarded: Option<u32>,
}

impl ReleaseOutput {
    /// A release refused before the agent ran because the checkout could not
    /// be brought level with its remote.
    fn refused(failure: GitSyncFailure, detail: &str) -> Self {
        Self {
            success: false,
            new_tag: None,
            summary: format!(
                "Release refused before any version bump: {failure}: {detail}. \
                 The agent was not invoked; nothing was committed, tagged or published."
            ),
            raw_output: None,
            exit_code: None,
            left_uncommitted: Vec::new(),
            sync_failure: Some(failure),
            fast_forwarded: None,
        }
    }
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

/// Build the `ReleaseCompleted` event for a release that went through
/// [`run_release`], on top of [`release_completed_event`].
///
/// `left_uncommitted` is omitted when empty, and `sync_failure` and
/// `fast_forwarded` when `None`, so a release in a checkout outside git keeps
/// the payload shape it always had.
pub(super) fn release_outcome_event(
    project: &str,
    throttle: Throttle,
    release_type: &str,
    cve: Option<&str>,
    output: &ReleaseOutput,
) -> Event {
    let mut event = release_completed_event(
        project,
        throttle,
        release_type,
        output.new_tag.as_deref(),
        output.success,
        cve,
        None,
    );
    if let Some(obj) = event.payload.as_object_mut() {
        if !output.left_uncommitted.is_empty() {
            obj.insert("left_uncommitted".to_string(), serde_json::json!(output.left_uncommitted));
        }
        if let Some(failure) = output.sync_failure {
            obj.insert("sync_failure".to_string(), serde_json::json!(failure));
        }
        if let Some(count) = output.fast_forwarded {
            obj.insert("fast_forwarded".to_string(), serde_json::json!(count));
        }
    }
    event
}

// ---------------------------------------------------------------------------
// Shared release runner
// ---------------------------------------------------------------------------

/// Generous timeout for Claude CLI — release tasks can take several minutes.
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(900);

/// Release prompt sentence that keeps the agent's work inside its one turn.
///
/// The gateway disables background tasks outright; this sentence is the
/// belt-and-braces half, so the agent plans for it rather than discovering it.
pub(super) const FOREGROUND_INSTRUCTION: &str = "This is a single non-interactive turn: run the \
     quality gates to completion in the foreground, passing a Bash timeout long enough for them, \
     and do not end your turn waiting on anything.";

/// Invoke the Claude agent for a release and return structured output.
///
/// Verifies `AGENTS.md` exists, brings the registered checkout level with
/// `origin/<branch>` (see [`sync_before_release`]), builds the agent request,
/// invokes the agent, and verifies the tag points at HEAD. A checkout that
/// cannot be fast-forwarded refuses the release before the agent runs, with a
/// typed `sync_failure`. When the release fails, the summary also names what
/// it left in the checkout (see `checkout_report`); the checkout itself is
/// never cleaned.
///
/// Returns `Err` if `AGENTS.md` is missing or git cannot say which branch the
/// checkout is on — callers convert this to a failure result.
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
        trace_id: input.trace_id.clone(),
        requires_json: false,
    };

    // The release runs in the registered checkout, so a stale checkout bumps
    // the wrong version and a failure can strand its edits there. Find the
    // work tree once; outside one (a non-git fixture) the sync, the dirt
    // snapshot and the leftover report are all skipped, with the probe's
    // warning.
    let checkout = checkout_report::probe(project_dir, shell).await;

    let fast_forwarded = match &checkout {
        Some(_) => {
            match sync_before_release(shell, project_dir, &input.project, &input.branch).await? {
                SyncOutcome::Synced { fast_forwarded } => {
                    tracing::info!(project = %input.project, fast_forwarded, "release checkout synced with remote");
                    Some(fast_forwarded)
                }
                SyncOutcome::Refused { failure, detail } => {
                    tracing::warn!(project = %input.project, %failure, %detail, "release refused: checkout could not be synced");
                    return Ok(ReleaseOutput::refused(failure, &detail));
                }
            }
        }
        None => None,
    };

    // Record what was already dirty, to tell a failed release's leftovers
    // from dirt that was there before it.
    let before = match checkout {
        Some(checkout) => checkout_report::snapshot(project_dir, shell)
            .await
            .map(|state| (checkout, state)),
        None => None,
    };

    let run_result = agent.invoke(&request).await;
    let (raw_output, exit_code) = agent_run_metadata(&run_result);
    let (cli_success, new_tag, cli_summary) = interpret_agent_result(run_result);

    let (success, mut summary) = tag_verify::check_tag_at_head(
        cli_success,
        new_tag.as_deref(),
        cli_summary,
        project_dir,
        shell,
    )
    .await;

    let mut left_uncommitted = Vec::new();
    if !success
        && let Some((checkout, before)) = before
        && let Some(after) = checkout_report::snapshot(project_dir, shell).await
    {
        let leftovers = checkout_report::leftovers(&before, &after);
        if let Some(sentences) = checkout_report::describe(&checkout, &leftovers) {
            tracing::warn!(checkout = %checkout.display(), %sentences, "failed release left work in the checkout");
            summary = checkout_report::append_to_summary(&summary, &sentences);
        }
        left_uncommitted = leftovers.left;
    }

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
        left_uncommitted,
        sync_failure: None,
        fast_forwarded,
    })
}

/// Bring the registered checkout level with `origin/<branch>` before the agent
/// is asked to bump anything.
///
/// Verifies `branch` is checked out, recovering from a detached `HEAD`, then
/// fetches and fast-forwards by the maintenance chain's own
/// [`sync_checkout`]. Another branch refuses with
/// [`GitSyncFailure::WrongBranch`]; a dirty tree, a diverged branch and an
/// unreachable remote refuse as `sync_checkout` does. Every refusal leaves the
/// checkout exactly as found.
///
/// The sync is never simulated here. A dry-run release is answered by
/// `SimulatedSuccess` before `run_release` is reached, so this only runs under
/// a throttle that permits mutation, and `sync_checkout` is called with
/// `dry_run = false`.
///
/// Returns `Err` when git cannot say which branch is checked out, or the
/// detached-`HEAD` recovery fails: the checkout needs a human before any
/// release can run in it.
async fn sync_before_release(
    shell: &dyn ShellGateway,
    path: &std::path::Path,
    project: &str,
    branch: &str,
) -> anyhow::Result<SyncOutcome> {
    match check_git_branch(project, path, branch, shell).await? {
        BranchCheckOutcome::Ok => {}
        BranchCheckOutcome::WrongBranch(reason) => {
            return Ok(SyncOutcome::Refused {
                failure: GitSyncFailure::WrongBranch,
                detail: reason,
            });
        }
        BranchCheckOutcome::Err(reason) => {
            anyhow::bail!("cannot verify the release checkout is on {branch}: {reason}");
        }
    }
    sync_checkout(shell, path, branch, false).await
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
