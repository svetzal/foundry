use std::path::PathBuf;
use std::time::Duration;

use foundry_sdk::task_block::TaskBlockResult;

use crate::gateway::{
    AgentAccess, AgentGateway, AgentOutcome, AgentProvider, AgentRequest, ModelTier,
    ReasoningEffort,
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
    /// Which model tier the block wants.
    pub tier: ModelTier,
    /// How much reasoning effort the block wants.
    pub effort: ReasoningEffort,
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
        tier: spec.tier,
        effort: spec.effort,
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

/// Parameters for a coding-agent invocation.
///
/// Pass to [`invoke_coding_agent`] — the `Full`/`Balanced`/`Medium` profile
/// is applied automatically.
pub(crate) struct CodingAgentSpec {
    pub working_dir: PathBuf,
    pub prompt: String,
    pub agent_file: Option<PathBuf>,
    pub provider: Option<AgentProvider>,
    pub timeout: Duration,
}

/// Invoke an agent with `Full` access at the `Balanced` tier and `Medium`
/// effort — the general-purpose coding profile.
///
/// Convenience wrapper around [`invoke_agent`] for the common pattern used by
/// execution blocks (`ExecutePlan`, `ExecuteMaintain`, `RemediatePipeline`, `RetryExecution`).
pub(crate) async fn invoke_coding_agent(
    agent: &dyn AgentGateway,
    project: &str,
    spec: CodingAgentSpec,
    trace_label: &str,
) -> AgentOutcome {
    invoke_agent(
        agent,
        AgentBlockSpec {
            prompt: spec.prompt,
            working_dir: spec.working_dir,
            access: AgentAccess::Full,
            tier: ModelTier::Balanced,
            effort: ReasoningEffort::Medium,
            agent_file: spec.agent_file,
            provider: spec.provider,
            timeout: spec.timeout,
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
    match serde_json::from_str::<serde_json::Value>(&extract_json(output)) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "agent output was not valid JSON; treating as no structured result");
            None
        }
    }
}

pub(crate) fn extract_json(s: &str) -> String {
    if let Some(start) = s.find('{')
        && let Some(end) = s.rfind('}')
    {
        return s[start..=end].to_string();
    }
    s.to_string()
}

/// Carry type returned to non-success closures in [`fold_agent_outcome`].
///
/// `unavailable` distinguishes a hard infrastructure failure (`Unavailable`)
/// from a soft agent-side failure (`AgentFailed`), letting callers choose
/// different fallbacks for each case when needed.
pub(crate) struct AgentNonSuccess {
    pub error: String,
    pub unavailable: bool,
}

/// Fold an [`AgentOutcome`] into a single value `T`.
///
/// - `Success { stdout }` → `on_success(stdout)`.
/// - `AgentFailed { stderr, .. }` → logs a `tracing::warn!` then calls
///   `on_nonsuccess(AgentNonSuccess { error: stderr, unavailable: false })`.
/// - `Unavailable { error }` → calls
///   `on_nonsuccess(AgentNonSuccess { error, unavailable: true })`.
///
/// This owns the standard warn + dispatch pattern that the eight observer/summary
/// blocks each previously hand-rolled as a three-arm `match`.
pub(crate) fn fold_agent_outcome<T>(
    outcome: AgentOutcome,
    project: &str,
    trace_label: &str,
    on_success: impl FnOnce(String) -> T,
    on_nonsuccess: impl FnOnce(AgentNonSuccess) -> T,
) -> T {
    match outcome {
        AgentOutcome::Success { stdout } => on_success(stdout),
        AgentOutcome::AgentFailed { stderr, .. } => {
            tracing::warn!(project = %project, stderr = %stderr, "{trace_label} failed");
            on_nonsuccess(AgentNonSuccess {
                error: stderr,
                unavailable: false,
            })
        }
        AgentOutcome::Unavailable { error } => on_nonsuccess(AgentNonSuccess {
            error,
            unavailable: true,
        }),
    }
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
        AgentOutcome::AgentFailed { stderr, failure } => {
            tracing::warn!(project = %project, stderr = %stderr, "{trace_label} failed");
            let message = failure
                .as_ref()
                .filter(|failure| failure.is_terminal_provider_failure())
                .map_or_else(
                    || format!("{trace_label} failed: {stderr}"),
                    foundry_sdk::gateway::AgentFailureMetadata::execution_summary,
                );
            Ok((message, false))
        }
        AgentOutcome::Unavailable { error } => {
            Err(TaskBlockResult::failure(format!("agent unavailable: {error}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_provider_returns_none_for_none_input() {
        assert_eq!(parse_agent_provider(None), None);
    }

    #[test]
    fn parse_agent_provider_parses_known_providers() {
        assert_eq!(parse_agent_provider(Some("claude")), Some(AgentProvider::Claude));
        assert_eq!(parse_agent_provider(Some("opencode")), Some(AgentProvider::Opencode));
        assert_eq!(parse_agent_provider(Some("codex")), Some(AgentProvider::Codex));
    }

    #[test]
    fn parse_agent_provider_is_case_insensitive() {
        assert_eq!(parse_agent_provider(Some("Claude")), Some(AgentProvider::Claude));
        assert_eq!(parse_agent_provider(Some("OPENCODE")), Some(AgentProvider::Opencode));
    }

    #[test]
    fn parse_agent_provider_returns_none_for_unknown() {
        assert_eq!(parse_agent_provider(Some("gpt")), None);
        assert_eq!(parse_agent_provider(Some("")), None);
    }

    #[test]
    fn chain_agent_provider_extracts_from_payload() {
        let payload = serde_json::json!({ "agent_provider": "claude" });
        assert_eq!(chain_agent_provider(&payload), Some(AgentProvider::Claude));
    }

    #[test]
    fn chain_agent_provider_returns_none_when_key_missing() {
        let payload = serde_json::json!({ "other_key": "value" });
        assert_eq!(chain_agent_provider(&payload), None);
    }

    #[test]
    fn chain_agent_provider_returns_none_when_value_not_a_string() {
        let payload = serde_json::json!({ "agent_provider": 42 });
        assert_eq!(chain_agent_provider(&payload), None);
    }

    #[test]
    fn chain_agent_provider_returns_none_for_unknown_provider() {
        let payload = serde_json::json!({ "agent_provider": "unknown-provider" });
        assert_eq!(chain_agent_provider(&payload), None);
    }

    #[test]
    fn fold_agent_outcome_routes_success_to_on_success() {
        let outcome = AgentOutcome::Success {
            stdout: "ok".to_string(),
        };
        let result: &str = fold_agent_outcome(
            outcome,
            "proj",
            "test",
            |s| if s == "ok" { "yes" } else { "no" },
            |_| "fail",
        );
        assert_eq!(result, "yes");
    }

    #[test]
    fn fold_agent_outcome_routes_agent_failed_with_unavailable_false() {
        let outcome = AgentOutcome::AgentFailed {
            stderr: "boom".to_string(),
            failure: None,
        };
        let result: Option<bool> = fold_agent_outcome(
            outcome,
            "proj",
            "test",
            |_| Some(true),
            |ns| {
                assert!(!ns.unavailable);
                assert_eq!(ns.error, "boom");
                None
            },
        );
        assert!(result.is_none());
    }

    #[test]
    fn fold_agent_outcome_routes_unavailable_with_unavailable_true() {
        let outcome = AgentOutcome::Unavailable {
            error: "no agent".to_string(),
        };
        let result: Option<bool> = fold_agent_outcome(
            outcome,
            "proj",
            "test",
            |_| Some(true),
            |ns| {
                assert!(ns.unavailable);
                assert_eq!(ns.error, "no agent");
                None
            },
        );
        assert!(result.is_none());
    }
}
