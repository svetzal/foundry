use std::path::PathBuf;
use std::time::Duration;

use foundry_core::task_block::TaskBlockResult;

use crate::gateway::{
    AgentAccess, AgentCapability, AgentGateway, AgentOutcome, AgentProvider, AgentRequest,
};

/// Parse a per-request agent provider override from its string form. Returns
/// `None` when absent or unparseable, so the routing gateway falls back to the
/// daemon default. Use this with a typed payload's `chain.agent_provider`.
pub(crate) fn parse_agent_provider(s: Option<&str>) -> Option<AgentProvider> {
    s.and_then(|s| s.parse().ok())
}

/// Extract a per-request agent provider override from a raw trigger payload's
/// chain context (the `agent_provider` field forwarded through the
/// iterate/maintain chain). Use this when the block holds the raw
/// `serde_json::Value` payload rather than a typed struct.
pub(crate) fn chain_agent_provider(payload: &serde_json::Value) -> Option<AgentProvider> {
    parse_agent_provider(payload.get("agent_provider").and_then(serde_json::Value::as_str))
}

/// Configuration for an agent invocation within a task block.
///
/// Pass to [`invoke_agent`] to handle request construction, invocation,
/// outcome conversion, and tracing.
pub(crate) struct AgentBlockSpec {
    pub prompt: String,
    pub working_dir: PathBuf,
    pub access: AgentAccess,
    pub capability: AgentCapability,
    pub agent_file: Option<PathBuf>,
    /// Per-request provider override (from the run's chain context). `None`
    /// means the routing gateway uses the daemon default.
    pub provider: Option<AgentProvider>,
    pub timeout: Duration,
}

/// Invoke an agent with the given spec, returning the outcome.
///
/// Encapsulates `AgentRequest` construction, `AgentGateway::invoke`,
/// `AgentOutcome::from_response`, and tracing.
pub(crate) async fn invoke_agent(
    agent: &dyn AgentGateway,
    spec: AgentBlockSpec,
    trace_label: &str,
    project: &str,
) -> AgentOutcome {
    let request = AgentRequest {
        prompt: spec.prompt,
        project: project.to_string(),
        working_dir: spec.working_dir,
        access: spec.access,
        capability: spec.capability,
        agent_file: spec.agent_file,
        provider: spec.provider,
        timeout: spec.timeout,
    };
    tracing::info!(project = %project, "{trace_label}: invoking agent");
    let response = agent.invoke(&request).await;
    let outcome = AgentOutcome::from_response(response);
    if let AgentOutcome::Unavailable { ref error } = outcome {
        tracing::warn!(project = %project, error = %error, "{trace_label}: agent unavailable");
    }
    outcome
}

/// Invoke an agent with `Full` access and `Coding` capability.
///
/// Convenience wrapper around [`invoke_agent`] for the common pattern used by
/// execution blocks (`ExecutePlan`, `ExecuteMaintain`, `RemediatePipeline`, `RetryExecution`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn invoke_coding_agent(
    agent: &dyn AgentGateway,
    project: &str,
    working_dir: std::path::PathBuf,
    prompt: String,
    agent_file: Option<std::path::PathBuf>,
    provider: Option<AgentProvider>,
    timeout: std::time::Duration,
    trace_label: &str,
) -> AgentOutcome {
    invoke_agent(
        agent,
        AgentBlockSpec {
            prompt,
            working_dir,
            access: AgentAccess::Full,
            capability: AgentCapability::Coding,
            agent_file,
            provider,
            timeout,
        },
        trace_label,
        project,
    )
    .await
}

/// Extract and parse the first JSON object from agent output.
///
/// Returns `None` if no valid JSON object is found. Handles agent responses
/// that include surrounding prose by locating the first `{` and last `}`.
pub(crate) fn parse_agent_json(output: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(&extract_json(output)).ok()
}

fn extract_json(s: &str) -> String {
    if let Some(start) = s.find('{') {
        if let Some(end) = s.rfind('}') {
            return s[start..=end].to_string();
        }
    }
    s.to_string()
}

/// Match an agent outcome into text output plus a success flag, or return a failure result.
///
/// On `Unavailable`, returns `Err(TaskBlockResult::failure(...))`.
/// On `AgentFailed`, logs a warning and returns `Ok((fallback_text, false))`.
/// On `Success`, returns `Ok((trimmed_stdout, true))`.
pub(crate) fn match_agent_text_outcome(
    outcome: AgentOutcome,
    project: &str,
    trace_label: &str,
) -> Result<(String, bool), TaskBlockResult> {
    match outcome {
        AgentOutcome::Success { stdout } => Ok((stdout.trim().to_string(), true)),
        AgentOutcome::AgentFailed { stderr } => {
            tracing::warn!(project = %project, stderr = %stderr, "{trace_label} failed");
            Ok((format!("{trace_label} failed: {stderr}"), false))
        }
        AgentOutcome::Unavailable { error } => {
            Err(TaskBlockResult::failure(format!("agent unavailable: {error}")))
        }
    }
}
