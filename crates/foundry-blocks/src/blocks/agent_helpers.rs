use std::path::PathBuf;
use std::time::Duration;

use foundry_sdk::task_block::TaskBlockResult;

use crate::gateway::{
    AgentAccess, AgentGateway, AgentOutcome, AgentProvider, AgentRequest, ModelTier,
    ReasoningEffort,
};

/// Resolve the agent file path from the registry agent name.
///
/// Convention: `~/.claude/agents/{agent}.md`. Returns `None` when the name is
/// empty or the file does not exist — callers fall back to the default agent.
pub(crate) fn resolve_agent_file(agent_name: &str) -> Option<PathBuf> {
    if agent_name.is_empty() {
        return None;
    }
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home)
        .join(".claude")
        .join("agents")
        .join(format!("{agent_name}.md"));
    if path.exists() { Some(path) } else { None }
}

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

/// Extract and parse a JSON object from agent output.
///
/// Returns `None` if no valid JSON object is found. Prefers the final fenced
/// JSON object when present, then falls back to the last structurally valid
/// bare JSON object embedded in surrounding prose.
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
    extract_last_fenced_json_object(s)
        .or_else(|| extract_last_structural_json_object(s))
        .unwrap_or_else(|| s.to_string())
}

fn extract_last_fenced_json_object(s: &str) -> Option<String> {
    let mut candidates = Vec::new();
    let mut remaining = s;

    while let Some(open) = remaining.find("```") {
        let after_open = &remaining[open + 3..];
        let content_start = after_open.find('\n').map_or(0, |newline| newline + 1);
        let content = &after_open[content_start..];
        let Some(close) = content.find("```") else {
            break;
        };
        candidates.push(content[..close].trim().to_string());
        remaining = &content[close + 3..];
    }

    candidates.into_iter().rev().find(|candidate| {
        serde_json::from_str::<serde_json::Value>(candidate)
            .ok()
            .is_some_and(|value| value.is_object())
    })
}

fn extract_last_structural_json_object(s: &str) -> Option<String> {
    let mut last_valid = None;
    let mut object_start = None;
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, ch) in s.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    object_start = Some(idx);
                }
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0
                    && let Some(start) = object_start.take()
                {
                    let candidate = &s[start..=idx];
                    if serde_json::from_str::<serde_json::Value>(candidate)
                        .ok()
                        .is_some_and(|value| value.is_object())
                    {
                        last_valid = Some(candidate.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    last_valid
}

/// Parameters for a read-only observer agent invocation.
///
/// Pass to [`invoke_reasoning_agent`] or [`invoke_summary_agent`] — the
/// access/tier/effort profile is applied automatically by the wrapper.
pub(crate) struct ReadOnlyAgentSpec {
    pub working_dir: PathBuf,
    pub prompt: String,
    pub agent_file: Option<PathBuf>,
    pub provider: Option<AgentProvider>,
    pub timeout: Duration,
}

/// Invoke an agent with `ReadOnly` access at the `Deep` tier and `High`
/// effort — the reasoning/assessment observer profile.
///
/// Convenience wrapper around [`invoke_agent`] for observer blocks that need
/// deep analysis without writing files (`ScoutDrift`, `AssessProject`,
/// `StrategicAssessor`, `SummarizeCommits`, `SummarizeEvents`).
pub(crate) async fn invoke_reasoning_agent(
    agent: &dyn AgentGateway,
    project: &str,
    spec: ReadOnlyAgentSpec,
    trace_label: &str,
) -> AgentOutcome {
    invoke_agent(
        agent,
        AgentBlockSpec {
            prompt: spec.prompt,
            working_dir: spec.working_dir,
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Deep,
            effort: ReasoningEffort::High,
            agent_file: spec.agent_file,
            provider: spec.provider,
            timeout: spec.timeout,
        },
        trace_label,
        project,
    )
    .await
}

/// Invoke an agent with `ReadOnly` access at the `Fast` tier and `Low`
/// effort — the lightweight summary/triage observer profile.
///
/// Convenience wrapper around [`invoke_agent`] for observer blocks that need
/// a quick, cheap read-only call (`AssessProject` naming, `SummarizeResult`,
/// `TriageAssessment`).
pub(crate) async fn invoke_summary_agent(
    agent: &dyn AgentGateway,
    project: &str,
    spec: ReadOnlyAgentSpec,
    trace_label: &str,
) -> AgentOutcome {
    invoke_agent(
        agent,
        AgentBlockSpec {
            prompt: spec.prompt,
            working_dir: spec.working_dir,
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Fast,
            effort: ReasoningEffort::Low,
            agent_file: spec.agent_file,
            provider: spec.provider,
            timeout: spec.timeout,
        },
        trace_label,
        project,
    )
    .await
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

/// Build a prompt that ends with the standard JSON-output envelope.
///
/// Appends `"\n\nOutput ONLY valid JSON in this exact format, nothing else:\n{schema_block}"`
/// to `preamble`, giving assessment blocks a single place to spell the sentinel.
pub(crate) fn json_output_prompt(preamble: &str, schema_block: &str) -> String {
    format!(
        "{preamble}\n\nOutput ONLY valid JSON in this exact format, nothing else:\n{schema_block}"
    )
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
    fn extract_json_returns_input_when_close_brace_precedes_open_brace() {
        // Input where `}` comes before `{`: must return verbatim without panic.
        let input = "} oops {";
        let result = extract_json(input);
        assert_eq!(result, input, "inverted braces should return input verbatim");
    }

    #[test]
    fn extract_json_extracts_object_from_surrounding_prose() {
        let input = "some text {\"key\": \"value\"} trailing";
        let result = extract_json(input);
        assert_eq!(result, "{\"key\": \"value\"}");
    }

    #[test]
    fn extract_json_prefers_terminal_fenced_object_over_earlier_brace_prose() {
        let input = "Gate schema uses gate{command,required} notation.\n```json\n{\"verdict\":\"complete\"}\n```";
        let result = extract_json(input);
        assert_eq!(result, "{\"verdict\":\"complete\"}");
    }

    #[test]
    fn parse_agent_json_prefers_terminal_fenced_object_over_invalid_brace_example() {
        let input = "Discuss gate{command,required} before the actual answer.\n```json\n{\"accepted\":true,\"reason\":\"ok\"}\n```";
        let result = parse_agent_json(input).expect("fenced JSON object should parse");
        assert_eq!(result["accepted"], true);
        assert_eq!(result["reason"], "ok");
    }

    #[test]
    fn parse_agent_json_returns_none_for_inverted_braces() {
        // Inverted brace order must return verbatim from extract_json and then
        // fail JSON parsing gracefully — no panic, returns None.
        let result = parse_agent_json("} then {");
        assert!(result.is_none(), "inverted braces should yield None");
    }

    #[test]
    fn parse_agent_json_returns_none_for_non_json_prose() {
        let result = parse_agent_json("no json here at all");
        assert!(result.is_none());
    }

    #[test]
    fn json_output_prompt_appends_envelope() {
        let result = json_output_prompt("My preamble.", "{\"key\": \"value\"}");
        assert!(result.starts_with("My preamble."));
        let envelope_count = result
            .matches("Output ONLY valid JSON in this exact format, nothing else:")
            .count();
        assert_eq!(envelope_count, 1);
        assert!(result.contains("{\"key\": \"value\"}"));
    }

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

    #[test]
    fn resolve_agent_file_returns_none_for_empty_name() {
        assert!(resolve_agent_file("").is_none());
    }

    #[test]
    fn resolve_agent_file_returns_none_when_file_does_not_exist() {
        assert!(resolve_agent_file("nonexistent-agent-xyz-foundry-test").is_none());
    }

    #[tokio::test]
    async fn invoke_reasoning_agent_forwards_readonly_deep_high() {
        use crate::gateway::fakes::FakeAgentGateway;

        let agent = FakeAgentGateway::success_with("analysis complete");
        let spec = ReadOnlyAgentSpec {
            working_dir: std::path::PathBuf::from("/tmp"),
            prompt: "analyze this".to_string(),
            agent_file: None,
            provider: None,
            timeout: Duration::from_secs(60),
        };
        invoke_reasoning_agent(&*agent, "proj", spec, "test").await;
        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].access, AgentAccess::ReadOnly);
        assert_eq!(invocations[0].tier, ModelTier::Deep);
        assert_eq!(invocations[0].effort, ReasoningEffort::High);
    }

    #[tokio::test]
    async fn invoke_summary_agent_forwards_readonly_fast_low() {
        use crate::gateway::fakes::FakeAgentGateway;

        let agent = FakeAgentGateway::success_with("summary done");
        let spec = ReadOnlyAgentSpec {
            working_dir: std::path::PathBuf::from("/tmp"),
            prompt: "summarize this".to_string(),
            agent_file: None,
            provider: None,
            timeout: Duration::from_secs(60),
        };
        invoke_summary_agent(&*agent, "proj", spec, "test").await;
        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].access, AgentAccess::ReadOnly);
        assert_eq!(invocations[0].tier, ModelTier::Fast);
        assert_eq!(invocations[0].effort, ReasoningEffort::Low);
    }
}
