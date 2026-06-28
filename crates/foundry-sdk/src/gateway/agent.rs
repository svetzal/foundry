use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which agentic CLI backend an invocation should run on.
///
/// The provider is *not* selected by the block — a block stays
/// provider-agnostic and merely forwards an optional override pulled from the
/// request chain. The host wires a single [`AgentGateway`] (in production, a
/// router) that maps this value to a concrete CLI implementation, falling back
/// to a process-level default when the request carries no override.
///
/// The wire form is the lowercase variant name (`"claude"`, `"opencode"`,
/// `"codex"`), matching the historical `FOUNDRY_AGENT_PROVIDER` string values.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum AgentProvider {
    /// Anthropic Claude via the `claude` CLI.
    #[default]
    Claude,
    /// `OpenAI` via the `opencode` CLI.
    Opencode,
    /// `OpenAI` via the `codex` CLI.
    Codex,
}

impl AgentProvider {
    /// The lowercase wire/CLI string for this provider.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentProvider::Claude => "claude",
            AgentProvider::Opencode => "opencode",
            AgentProvider::Codex => "codex",
        }
    }
}

impl std::fmt::Display for AgentProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AgentProvider {
    type Err = String;

    /// Parse a provider name case-insensitively. Unknown names are an error so
    /// callers can fall back to a default (rather than silently accepting a
    /// typo as a valid provider).
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Ok(AgentProvider::Claude),
            "opencode" => Ok(AgentProvider::Opencode),
            "codex" => Ok(AgentProvider::Codex),
            other => Err(format!("unknown agent provider: {other}")),
        }
    }
}

/// Classified kind for a terminal provider/account failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentFailureKind {
    /// Provider account quota, spend, or similar hard usage ceiling was reached.
    AccountLimit,
    /// Provider account is disabled for billing or policy reasons.
    AccountDisabled,
    /// Provider authentication was revoked or expired.
    Authentication,
}

impl AgentFailureKind {
    /// Human summary prefix used in workflow result messages.
    pub fn summary_label(self) -> &'static str {
        match self {
            AgentFailureKind::AccountLimit => "agent account limit reached",
            AgentFailureKind::AccountDisabled => "agent account disabled",
            AgentFailureKind::Authentication => "agent authentication revoked",
        }
    }
}

/// Structured failure metadata captured from an agent provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentFailureMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_log_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_error_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<AgentFailureKind>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub terminal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl AgentFailureMetadata {
    #[must_use]
    pub fn new(provider: AgentProvider) -> Self {
        Self {
            provider: Some(provider),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_session(mut self, session_id: impl Into<String>, source_log_path: PathBuf) -> Self {
        self.session_id = Some(session_id.into());
        self.source_log_path = Some(source_log_path);
        self
    }

    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    #[must_use]
    pub fn with_api_error_status(mut self, status: u16) -> Self {
        self.api_error_status = Some(status);
        self
    }

    #[must_use]
    pub fn terminal(mut self, kind: AgentFailureKind) -> Self {
        self.failure_kind = Some(kind);
        self.terminal = true;
        self
    }

    #[must_use]
    pub fn is_terminal_provider_failure(&self) -> bool {
        self.terminal && self.failure_kind.is_some()
    }

    #[must_use]
    pub fn summary_label(&self) -> &'static str {
        self.failure_kind.map_or("agent failed", AgentFailureKind::summary_label)
    }

    #[must_use]
    pub fn execution_summary(&self) -> String {
        match self.message.as_deref() {
            Some(message) if !message.trim().is_empty() => {
                format!("{}: {}", self.summary_label(), message.trim())
            }
            _ => self.summary_label().to_string(),
        }
    }

    #[must_use]
    pub fn stop_summary(&self) -> String {
        format!("{}; workflow stopped", self.summary_label())
    }
}

/// Classify a provider error message into terminal failure metadata when the
/// wording indicates a non-retryable provider/account state.
#[must_use]
pub fn classify_terminal_provider_failure(
    provider: AgentProvider,
    api_error_status: Option<u16>,
    message: &str,
    session_id: Option<&str>,
    source_log_path: Option<&Path>,
) -> Option<AgentFailureMetadata> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized = trimmed.to_ascii_lowercase();

    let failure_kind = if contains_any(
        &normalized,
        &[
            "authentication revoked",
            "authentication expired",
            "auth revoked",
            "auth expired",
            "access token expired",
            "access token revoked",
            "credentials revoked",
            "credentials expired",
        ],
    ) {
        Some(AgentFailureKind::Authentication)
    } else if contains_any(
        &normalized,
        &[
            "billing disabled",
            "account disabled",
            "billing account disabled",
            "account has been disabled",
            "subscription disabled",
        ],
    ) {
        Some(AgentFailureKind::AccountDisabled)
    } else if contains_any(
        &normalized,
        &[
            "monthly spend limit",
            "spend limit",
            "quota exhausted",
            "account limit",
            "usage limit reached",
            "credit balance is too low",
            "credits exhausted",
            "no credits remaining",
        ],
    ) {
        Some(AgentFailureKind::AccountLimit)
    } else {
        None
    }?;

    // A bare 429 / rate-limit wording is not terminal. Require explicit
    // account/quota/auth/billing phrasing.
    if failure_kind == AgentFailureKind::AccountLimit
        && api_error_status == Some(429)
        && contains_any(&normalized, &["rate limit exceeded", "retry later", "too many requests"])
        && !contains_any(
            &normalized,
            &[
                "monthly spend limit",
                "spend limit",
                "quota exhausted",
                "account limit",
                "credits exhausted",
                "no credits remaining",
            ],
        )
    {
        return None;
    }

    let mut failure = AgentFailureMetadata::new(provider)
        .terminal(failure_kind)
        .with_message(trimmed.to_string());
    if let Some(status) = api_error_status {
        failure = failure.with_api_error_status(status);
    }
    if let (Some(session_id), Some(source_log_path)) = (session_id, source_log_path) {
        failure = failure.with_session(session_id.to_string(), source_log_path.to_path_buf());
    }
    Some(failure)
}

/// Parse a Claude JSON result record and classify terminal provider failures.
#[must_use]
pub fn classify_claude_result_record(
    raw_record: &str,
    session_id: Option<&str>,
    source_log_path: Option<&Path>,
) -> Option<AgentFailureMetadata> {
    let value = serde_json::from_str::<Value>(raw_record).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("result")
        || !value.get("is_error").and_then(Value::as_bool).unwrap_or(false)
    {
        return None;
    }

    let message = value.get("result").and_then(Value::as_str)?;
    let api_error_status = value
        .get("api_error_status")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok());

    classify_terminal_provider_failure(
        AgentProvider::Claude,
        api_error_status,
        message,
        session_id,
        source_log_path,
    )
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

/// Access level for an agent invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAccess {
    /// Read-only tools (Read, Glob, Grep, `WebFetch`, `WebSearch`).
    ReadOnly,
    /// Full tool access — no restrictions.
    Full,
}

impl AgentAccess {
    /// The wire-format label for this access level, used in `AgentSessionStarted` payloads.
    pub fn label(self) -> &'static str {
        match self {
            AgentAccess::ReadOnly => "read_only",
            AgentAccess::Full => "full",
        }
    }
}

/// Abstract model tier — *which* model a block wants, independent of provider.
///
/// Each provider maps a tier to a concrete model id (configurable via the agent
/// config; see [`crate::agent_config`]). Mirrors hopper's `deep/balanced/fast`
/// vocabulary.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    /// Most capable model — deep reasoning, planning, assessment.
    Deep,
    /// General-purpose model — coding and edits. The sensible default.
    #[default]
    Balanced,
    /// Fast, lightweight model — one-shot helpers (naming, triage, summaries).
    Fast,
}

/// Abstract reasoning effort — *how hard* the model should think, independent of
/// provider. Each provider maps a level to its CLI token (configurable; see
/// [`crate::agent_config`]), clamping levels its CLI does not support.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Minimal thinking — fastest, cheapest.
    Minimal,
    /// Low effort.
    Low,
    /// Medium effort. The sensible default.
    #[default]
    Medium,
    /// High effort — for hard reasoning.
    High,
    /// Maximum effort — the most thorough the provider offers.
    Max,
}

impl ModelTier {
    /// The lowercase wire string for this tier.
    pub fn as_str(self) -> &'static str {
        match self {
            ModelTier::Deep => "deep",
            ModelTier::Balanced => "balanced",
            ModelTier::Fast => "fast",
        }
    }
    /// All tiers, in order, for iteration (config seeding, completeness checks).
    pub const ALL: [ModelTier; 3] = [ModelTier::Deep, ModelTier::Balanced, ModelTier::Fast];
}

impl std::fmt::Display for ModelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ReasoningEffort {
    /// The lowercase wire string for this effort level.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
        }
    }
    /// All effort levels, low→high, for iteration (config seeding, completeness).
    pub const ALL: [ReasoningEffort; 5] = [
        ReasoningEffort::Minimal,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::Max,
    ];
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A request to invoke an AI agent.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    /// The work to perform.
    pub prompt: String,
    /// Project name (registry key) this invocation belongs to.
    pub project: String,
    /// Project working directory.
    pub working_dir: PathBuf,
    /// Tool access level.
    pub access: AgentAccess,
    /// Which model tier the block wants. The gateway resolves this to a concrete
    /// model id for its provider.
    pub tier: ModelTier,
    /// How much reasoning effort the block wants. The gateway maps this to its
    /// provider's CLI token.
    pub effort: ReasoningEffort,
    /// Optional path to an agent definition file.
    pub agent_file: Option<PathBuf>,
    /// Optional per-request provider override. `None` means "use the host's
    /// default provider". The production [`AgentGateway`] is a router that reads
    /// this field and dispatches to the matching CLI backend.
    pub provider: Option<AgentProvider>,
    /// Maximum duration for the invocation.
    pub timeout: std::time::Duration,
}

/// Response from an agent invocation.
#[derive(Debug, Clone)]
pub struct AgentResponse {
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Process exit code.
    pub exit_code: i32,
    /// Whether the invocation succeeded (`exit_code` == 0).
    pub success: bool,
    /// Structured provider/account failure metadata when available.
    pub failure: Option<AgentFailureMetadata>,
}

impl AgentResponse {
    #[must_use]
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
            failure: None,
        }
    }

    #[must_use]
    pub fn failure(stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code: 1,
            success: false,
            failure: None,
        }
    }

    #[must_use]
    pub fn failure_with_metadata(
        stderr: impl Into<String>,
        exit_code: i32,
        failure: AgentFailureMetadata,
    ) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code,
            success: false,
            failure: Some(failure),
        }
    }

    #[must_use]
    pub fn terminal_failure(
        provider: AgentProvider,
        kind: AgentFailureKind,
        message: impl Into<String>,
    ) -> Self {
        let message = message.into();
        Self::failure_with_metadata(
            message.clone(),
            1,
            AgentFailureMetadata::new(provider).terminal(kind).with_message(message),
        )
    }
}

/// The outcome of an agent invocation, abstracting over `Result<AgentResponse>`.
/// Separates three distinct cases: successful run, agent ran but failed (non-zero exit),
/// and the agent could not be invoked at all.
pub enum AgentOutcome {
    /// Agent ran and succeeded (`success=true`).
    Success { stdout: String },
    /// Agent ran but exited with failure (`success=false`).
    AgentFailed {
        stderr: String,
        failure: Option<AgentFailureMetadata>,
    },
    /// Agent could not be invoked (spawn failure, timeout, etc.).
    Unavailable { error: String },
}

impl AgentOutcome {
    pub fn from_response(response: anyhow::Result<AgentResponse>) -> Self {
        match response {
            Ok(r) if r.success => AgentOutcome::Success { stdout: r.stdout },
            Ok(r) => AgentOutcome::AgentFailed {
                stderr: r.stderr,
                failure: r.failure,
            },
            Err(err) => AgentOutcome::Unavailable {
                error: err.to_string(),
            },
        }
    }
}

/// Abstracts over AI agent runtimes so that blocks can be tested without
/// invoking real agent processes.
pub trait AgentGateway: Send + Sync {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>>;
}

#[cfg(test)]
mod access_tests {
    use super::AgentAccess;

    #[test]
    fn label_returns_wire_strings() {
        assert_eq!(AgentAccess::ReadOnly.label(), "read_only");
        assert_eq!(AgentAccess::Full.label(), "full");
    }
}

#[cfg(test)]
mod provider_tests {
    use super::AgentProvider;

    #[test]
    fn parses_known_providers_case_insensitively() {
        assert_eq!("claude".parse::<AgentProvider>().unwrap(), AgentProvider::Claude);
        assert_eq!("opencode".parse::<AgentProvider>().unwrap(), AgentProvider::Opencode);
        assert_eq!("codex".parse::<AgentProvider>().unwrap(), AgentProvider::Codex);
        assert_eq!("  CODEX ".parse::<AgentProvider>().unwrap(), AgentProvider::Codex);
    }

    #[test]
    fn rejects_unknown_provider() {
        assert!("gemini".parse::<AgentProvider>().is_err());
        assert!("".parse::<AgentProvider>().is_err());
    }

    #[test]
    fn default_is_claude() {
        assert_eq!(AgentProvider::default(), AgentProvider::Claude);
    }

    #[test]
    fn display_and_as_str_are_lowercase_wire_form() {
        assert_eq!(AgentProvider::Claude.as_str(), "claude");
        assert_eq!(AgentProvider::Opencode.to_string(), "opencode");
        assert_eq!(AgentProvider::Codex.to_string(), "codex");
    }

    #[test]
    fn serde_round_trips_lowercase() {
        let json = serde_json::to_string(&AgentProvider::Codex).unwrap();
        assert_eq!(json, "\"codex\"");
        let back: AgentProvider = serde_json::from_str("\"opencode\"").unwrap();
        assert_eq!(back, AgentProvider::Opencode);
    }
}

#[cfg(test)]
mod agent_failure_classifier_tests {
    use std::path::Path;

    use super::{
        AgentFailureKind, AgentProvider, classify_claude_result_record,
        classify_terminal_provider_failure,
    };

    #[test]
    fn agent_failure_classifier_detects_claude_monthly_spend_limit() {
        let fixture = r#"{"type":"result","is_error":true,"api_error_status":429,"result":"You've hit your monthly spend limit - raise it at claude.ai/settings/usage"}"#;
        let failure = classify_claude_result_record(
            fixture,
            Some("session-123"),
            Some(Path::new("/tmp/session.jsonl")),
        )
        .expect("fixture should classify");

        assert_eq!(failure.provider, Some(AgentProvider::Claude));
        assert_eq!(failure.api_error_status, Some(429));
        assert_eq!(failure.failure_kind, Some(AgentFailureKind::AccountLimit));
        assert!(failure.terminal);
        assert_eq!(
            failure.message.as_deref(),
            Some("You've hit your monthly spend limit - raise it at claude.ai/settings/usage")
        );
    }

    #[test]
    fn agent_failure_classifier_does_not_mark_generic_rate_limit_terminal() {
        let failure = classify_terminal_provider_failure(
            AgentProvider::Claude,
            Some(429),
            "Rate limit exceeded; retry later",
            Some("session-123"),
            Some(Path::new("/tmp/session.jsonl")),
        );

        assert!(failure.is_none());
    }
}
