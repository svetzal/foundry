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
    /// Whether bounded invocations enforce the declared writable repository.
    ///
    /// Claude uses a scoped edit allowance, denied permission prompts and an
    /// OS sandbox. Codex uses its workspace sandbox. Opencode has no proven
    /// enforcement and must not run a campaign's bounded invocation.
    pub const fn enforces_writable_root(self) -> bool {
        matches!(self, Self::Claude | Self::Codex)
    }

    /// The lowercase wire/CLI string for this provider.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentProvider::Claude => "claude",
            AgentProvider::Opencode => "opencode",
            AgentProvider::Codex => "codex",
        }
    }
}

/// The agent's role in a campaign cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStage {
    Formation,
    Execution,
    Review,
}

impl AgentStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Formation => "formation",
            Self::Execution => "execution",
            Self::Review => "review",
        }
    }
}

/// Optional provider overrides for each stage. Omitted stages inherit the
/// campaign's legacy provider or the daemon's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageProviders {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formation: Option<AgentProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<AgentProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<AgentProvider>,
}

impl StageProviders {
    /// Resolve one stage, preserving an absent daemon-default override.
    pub fn resolve(
        self,
        stage: AgentStage,
        fallback: Option<AgentProvider>,
    ) -> Option<AgentProvider> {
        match stage {
            AgentStage::Formation => self.formation,
            AgentStage::Execution => self.execution,
            AgentStage::Review => self.review,
        }
        .or(fallback)
    }

    /// Inherit only the stages the member did not set.
    #[must_use]
    pub fn inherit(self, initiative: &Self) -> Self {
        Self {
            formation: self.formation.or(initiative.formation),
            execution: self.execution.or(initiative.execution),
            review: self.review.or(initiative.review),
        }
    }

    /// Whether serializing this map would carry no provider overrides.
    pub fn is_empty(&self) -> bool {
        self.formation.is_none() && self.execution.is_none() && self.review.is_none()
    }
}

#[cfg(test)]
mod stage_provider_tests {
    use super::*;

    #[test]
    fn stages_choose_explicit_provider_then_legacy_default() {
        let providers = StageProviders {
            formation: Some(AgentProvider::Claude),
            execution: Some(AgentProvider::Codex),
            review: None,
        };
        assert_eq!(
            providers.resolve(AgentStage::Formation, Some(AgentProvider::Codex)),
            Some(AgentProvider::Claude)
        );
        assert_eq!(
            providers.resolve(AgentStage::Execution, Some(AgentProvider::Claude)),
            Some(AgentProvider::Codex)
        );
        assert_eq!(
            providers.resolve(AgentStage::Review, Some(AgentProvider::Codex)),
            Some(AgentProvider::Codex)
        );
        assert_eq!(providers.resolve(AgentStage::Review, None), None);
    }

    #[test]
    fn members_inherit_only_omitted_initiative_stages() {
        let member = StageProviders {
            execution: Some(AgentProvider::Codex),
            ..StageProviders::default()
        };
        let initiative = StageProviders {
            formation: Some(AgentProvider::Claude),
            execution: Some(AgentProvider::Claude),
            review: Some(AgentProvider::Claude),
        };
        assert_eq!(
            member.inherit(&initiative),
            StageProviders {
                formation: Some(AgentProvider::Claude),
                execution: Some(AgentProvider::Codex),
                review: Some(AgentProvider::Claude)
            }
        );
    }

    #[test]
    fn only_providers_with_enforced_boundaries_declare_writable_root_support() {
        assert!(AgentProvider::Claude.enforces_writable_root());
        assert!(AgentProvider::Codex.enforces_writable_root());
        assert!(!AgentProvider::Opencode.enforces_writable_root());
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

/// Classified kind of a provider failure.
///
/// Three kinds are terminal account states: another attempt returns the same
/// refusal until a person changes the account. `ProviderUnavailable` is
/// transient: the provider could not serve the session right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentFailureKind {
    /// Provider account quota, spend, or similar hard usage ceiling was reached.
    AccountLimit,
    /// Provider account is disabled for billing or policy reasons.
    AccountDisabled,
    /// Provider authentication was revoked or expired.
    Authentication,
    /// The provider refused the session for now: the model is at capacity,
    /// the API is overloaded or rate limited, or it answered HTTP 429, 503 or
    /// 529. Worth another attempt after a pause, and never a reason to stop a
    /// workflow.
    ProviderUnavailable,
}

impl AgentFailureKind {
    /// Human summary prefix used in workflow result messages.
    pub fn summary_label(self) -> &'static str {
        match self {
            AgentFailureKind::AccountLimit => "agent account limit reached",
            AgentFailureKind::AccountDisabled => "agent account disabled",
            AgentFailureKind::Authentication => "agent authentication revoked",
            AgentFailureKind::ProviderUnavailable => "provider unavailable",
        }
    }

    /// Whether another attempt returns the same refusal until a person acts.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, AgentFailureKind::ProviderUnavailable)
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
    /// Why the run must stop and wait for a person, set by Foundry's own
    /// checks after the agent session rather than by the provider (for
    /// example: the agent pushed its commits itself, before Foundry's checks
    /// ran). A run with this set is not retried: another attempt cannot undo
    /// what needs reviewing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_review: Option<String>,
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

    /// Record `kind`, terminal only when the kind is.
    #[must_use]
    pub fn classified(mut self, kind: AgentFailureKind) -> Self {
        self.failure_kind = Some(kind);
        self.terminal = kind.is_terminal();
        self
    }

    /// Mark the failure as needing review, with the reason a person reads.
    #[must_use]
    pub fn with_needs_review(mut self, reason: impl Into<String>) -> Self {
        self.needs_review = Some(reason.into());
        self
    }

    #[must_use]
    pub fn is_terminal_provider_failure(&self) -> bool {
        self.terminal && self.failure_kind.is_some()
    }

    /// Whether the provider refused the session as unavailable for now.
    #[must_use]
    pub fn is_provider_unavailable(&self) -> bool {
        self.failure_kind == Some(AgentFailureKind::ProviderUnavailable)
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

/// Wording that names a provider refusing a session for now.
const PROVIDER_UNAVAILABLE_WORDING: &[&str] = &[
    "at capacity",
    "overloaded",
    "rate limited",
    "rate-limited",
    "rate limit exceeded",
    "too many requests",
    "temporarily unavailable",
    "service unavailable",
];

/// HTTP statuses a provider returns when it cannot serve a session for now:
/// 429 (rate limited), 503 (unavailable) and 529 (Anthropic's overloaded).
const PROVIDER_UNAVAILABLE_STATUSES: &[u16] = &[429, 503, 529];

/// Classify a provider error message into failure metadata.
///
/// Terminal account wording (revoked authentication, a disabled account, an
/// exhausted spend or quota) wins and yields a terminal kind. Otherwise
/// capacity, overload and rate-limit wording, or an HTTP 429, 503 or 529,
/// yields [`AgentFailureKind::ProviderUnavailable`], which is not terminal.
/// `None` when the message names neither. When `api_error_status` is absent,
/// a status the message itself names (`unexpected status 503`, `last status:
/// 429`) is used and recorded.
#[must_use]
pub fn classify_provider_failure(
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
    let status = api_error_status.or_else(|| status_in_message(&normalized));
    let failure_kind = terminal_failure_kind(&normalized, status)
        .or_else(|| transient_failure_kind(&normalized, status))?;

    let mut failure = AgentFailureMetadata::new(provider)
        .classified(failure_kind)
        .with_message(trimmed.to_string());
    if let Some(status) = status {
        failure = failure.with_api_error_status(status);
    }
    if let (Some(session_id), Some(source_log_path)) = (session_id, source_log_path) {
        failure = failure.with_session(session_id.to_string(), source_log_path.to_path_buf());
    }
    Some(failure)
}

/// The terminal account kind the wording names, if any.
fn terminal_failure_kind(normalized: &str, status: Option<u16>) -> Option<AgentFailureKind> {
    let failure_kind = if contains_any(
        normalized,
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
        AgentFailureKind::Authentication
    } else if contains_any(
        normalized,
        &[
            "billing disabled",
            "account disabled",
            "billing account disabled",
            "account has been disabled",
            "subscription disabled",
        ],
    ) {
        AgentFailureKind::AccountDisabled
    } else if contains_any(
        normalized,
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
        AgentFailureKind::AccountLimit
    } else {
        return None;
    };

    // A bare 429 / rate-limit wording is not terminal. Require explicit
    // account/quota/auth/billing phrasing; the transient check sees the rest.
    if failure_kind == AgentFailureKind::AccountLimit
        && status == Some(429)
        && contains_any(normalized, &["rate limit exceeded", "retry later", "too many requests"])
        && !contains_any(
            normalized,
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
    Some(failure_kind)
}

/// `ProviderUnavailable` when the wording or the status says the provider
/// could not serve the session for now.
fn transient_failure_kind(normalized: &str, status: Option<u16>) -> Option<AgentFailureKind> {
    (contains_any(normalized, PROVIDER_UNAVAILABLE_WORDING)
        || status.is_some_and(|status| PROVIDER_UNAVAILABLE_STATUSES.contains(&status)))
    .then_some(AgentFailureKind::ProviderUnavailable)
}

/// The three-digit HTTP status a message names after the word `status`, as
/// codex reports it (`unexpected status 503 Service Unavailable`, `last
/// status: 429`).
fn status_in_message(normalized: &str) -> Option<u16> {
    normalized.match_indices("status").find_map(|(index, word)| {
        let rest = normalized[index + word.len()..].trim_start_matches([':', '=', ' ']);
        let rest = rest
            .strip_prefix("code")
            .map_or(rest, |rest| rest.trim_start_matches([':', '=', ' ']));
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 3 {
            rest[..3].parse().ok()
        } else {
            None
        }
    })
}

/// Parse a Claude JSON result record and classify its provider failure,
/// terminal and transient alike (see [`classify_provider_failure`]).
#[must_use]
pub fn classify_claude_result_record(
    raw_record: &str,
    session_id: Option<&str>,
    source_log_path: Option<&Path>,
) -> Option<AgentFailureMetadata> {
    let value = match serde_json::from_str::<Value>(raw_record) {
        Ok(v) => v,
        Err(e) => {
            // Best-effort: a line that isn't valid JSON just isn't a Claude result
            // record — the caller scans a JSONL stream that may contain other line
            // shapes, so skip it rather than fail classification; log for
            // investigability.
            tracing::debug!(error = %e, "line is not valid JSON while classifying Claude result record");
            return None;
        }
    };
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

    classify_provider_failure(
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
    /// Environment entries applied only to this agent process and its children.
    /// Workflow blocks use this for execution-scoped safety controls that must
    /// not affect the daemon or the registered checkout.
    pub env: Vec<(String, String)>,
    /// Maximum duration for the invocation.
    /// Maximum agent runtime. Zero means no clock deadline.
    pub timeout: std::time::Duration,
    /// The trace this invocation belongs to, taken from the block's triggering
    /// event.
    ///
    /// Agent sessions are where every token is spent, so without this the
    /// estate's cost cannot be attributed to the workflow that incurred it —
    /// only to a project and a timestamp, which is ambiguous whenever runs
    /// overlap. `None` means the caller had no trace context (a direct CLI
    /// invocation), not that attribution was skipped.
    pub trace_id: Option<String>,
    /// Whether the caller parses this invocation's answer as a typed JSON
    /// object.
    ///
    /// A print-mode session gets exactly one turn: an agent that ends its turn
    /// waiting on something (a background command, a notification that never
    /// arrives) produces prose where the block needs a verdict, and the block
    /// fails on a non-result. When this is set, a gateway that can resume a
    /// session is licensed to do so once to recover the missing answer.
    pub requires_json: bool,
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

    /// A session the provider refused as unavailable for now (exit 1).
    #[must_use]
    pub fn provider_unavailable(provider: AgentProvider, message: impl Into<String>) -> Self {
        let message = message.into();
        Self::failure_with_metadata(
            message.clone(),
            1,
            AgentFailureMetadata::new(provider)
                .classified(AgentFailureKind::ProviderUnavailable)
                .with_message(message),
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
mod needs_review_tests {
    use super::AgentFailureMetadata;

    #[test]
    fn needs_review_is_not_a_provider_failure_and_round_trips() {
        let failure = AgentFailureMetadata::default().with_needs_review("agent pushed directly");
        assert!(
            !failure.is_terminal_provider_failure(),
            "a review stop must not open a provider circuit breaker"
        );
        let json = serde_json::to_value(&failure).unwrap();
        assert_eq!(json["needs_review"], "agent pushed directly");
        let back: AgentFailureMetadata = serde_json::from_value(json).unwrap();
        assert_eq!(back.needs_review.as_deref(), Some("agent pushed directly"));
    }

    #[test]
    fn needs_review_is_omitted_when_absent() {
        let json = serde_json::to_value(AgentFailureMetadata::default()).unwrap();
        assert!(json.get("needs_review").is_none());
    }
}

#[cfg(test)]
mod agent_failure_classifier_tests {
    use std::path::Path;

    use super::{
        AgentFailureKind, AgentProvider, classify_claude_result_record, classify_provider_failure,
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

    /// A generic 429 rate limit is not an account state: it is the provider
    /// refusing the session for now, and must not stop a workflow.
    #[test]
    fn agent_failure_classifier_marks_generic_rate_limit_unavailable_not_terminal() {
        let failure = classify_provider_failure(
            AgentProvider::Claude,
            Some(429),
            "Rate limit exceeded; retry later",
            Some("session-123"),
            Some(Path::new("/tmp/session.jsonl")),
        )
        .expect("a 429 rate limit classifies");

        assert_eq!(failure.failure_kind, Some(AgentFailureKind::ProviderUnavailable));
        assert!(!failure.terminal);
        assert!(!failure.is_terminal_provider_failure());
        assert!(failure.is_provider_unavailable());
    }

    /// The 2026-10-19 production failure: codex exits 1 with zero tokens and
    /// this message.
    #[test]
    fn codex_model_at_capacity_is_provider_unavailable() {
        let failure = classify_provider_failure(
            AgentProvider::Codex,
            None,
            "Selected model is at capacity. Please try a different model.",
            None,
            None,
        )
        .expect("capacity wording classifies");

        assert_eq!(failure.provider, Some(AgentProvider::Codex));
        assert_eq!(failure.failure_kind, Some(AgentFailureKind::ProviderUnavailable));
        assert!(!failure.terminal);
        assert_eq!(
            failure.execution_summary(),
            "provider unavailable: Selected model is at capacity. Please try a different model."
        );
    }

    #[test]
    fn transient_wording_classifies_as_provider_unavailable_for_both_providers() {
        for provider in [AgentProvider::Codex, AgentProvider::Claude] {
            for message in [
                "Selected model is at capacity. Please try a different model.",
                "Overloaded",
                "You are being rate limited",
                "Rate limit exceeded",
                "429 Too Many Requests",
                "The service is temporarily unavailable",
                "503 Service Unavailable",
            ] {
                let failure = classify_provider_failure(provider, None, message, None, None)
                    .unwrap_or_else(|| panic!("{provider} {message:?} must classify"));
                assert_eq!(
                    failure.failure_kind,
                    Some(AgentFailureKind::ProviderUnavailable),
                    "{provider} {message:?}"
                );
                assert!(!failure.terminal, "{provider} {message:?}");
            }
        }
    }

    #[test]
    fn statuses_429_503_and_529_alone_classify_as_provider_unavailable() {
        for status in [429, 503, 529] {
            let failure = classify_provider_failure(
                AgentProvider::Claude,
                Some(status),
                "API Error",
                None,
                None,
            )
            .unwrap_or_else(|| panic!("status {status} must classify"));
            assert_eq!(failure.failure_kind, Some(AgentFailureKind::ProviderUnavailable));
            assert_eq!(failure.api_error_status, Some(status));
            assert!(!failure.terminal);
        }
        assert!(
            classify_provider_failure(AgentProvider::Claude, Some(500), "API Error", None, None)
                .is_none(),
            "an unrelated status names no provider state"
        );
    }

    /// Codex names the status in its message rather than a field.
    #[test]
    fn codex_status_in_the_message_classifies_and_is_recorded() {
        for (message, status) in [
            ("unexpected status 503: upstream connect error", 503),
            ("exceeded retry limit, last status: 429", 429),
            ("stream error: status code 529", 529),
        ] {
            let failure =
                classify_provider_failure(AgentProvider::Codex, None, message, None, None)
                    .unwrap_or_else(|| panic!("{message:?} must classify"));
            assert_eq!(failure.failure_kind, Some(AgentFailureKind::ProviderUnavailable));
            assert_eq!(failure.api_error_status, Some(status), "{message:?}");
        }
    }

    #[test]
    fn claude_overloaded_error_record_is_provider_unavailable() {
        let fixture = r#"{"type":"result","is_error":true,"api_error_status":529,"result":"API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}"}"#;
        let failure = classify_claude_result_record(
            fixture,
            Some("session-529"),
            Some(Path::new("/tmp/session.jsonl")),
        )
        .expect("an overloaded result record classifies");

        assert_eq!(failure.failure_kind, Some(AgentFailureKind::ProviderUnavailable));
        assert_eq!(failure.api_error_status, Some(529));
        assert!(!failure.terminal);
        assert_eq!(failure.session_id.as_deref(), Some("session-529"));
    }

    /// Terminal account wording wins over a transient status or wording.
    #[test]
    fn terminal_wording_still_wins_over_transient_signals() {
        let cases = [
            (
                Some(429),
                "Too many requests: you've hit your monthly spend limit",
                AgentFailureKind::AccountLimit,
            ),
            (
                Some(503),
                "Service unavailable: billing account disabled",
                AgentFailureKind::AccountDisabled,
            ),
            (Some(529), "Overloaded; access token expired", AgentFailureKind::Authentication),
            (
                None,
                "Model at capacity, and your quota exhausted",
                AgentFailureKind::AccountLimit,
            ),
        ];
        for (status, message, kind) in cases {
            let failure =
                classify_provider_failure(AgentProvider::Codex, status, message, None, None)
                    .unwrap_or_else(|| panic!("{message:?} must classify"));
            assert_eq!(failure.failure_kind, Some(kind), "{message:?}");
            assert!(failure.terminal, "{message:?}");
        }
    }

    #[test]
    fn unrelated_failure_wording_is_unclassified() {
        assert!(
            classify_provider_failure(
                AgentProvider::Codex,
                None,
                "The 'x' model is not supported when using Codex with a ChatGPT account.",
                None,
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn provider_unavailable_round_trips_as_snake_case() {
        let json = serde_json::to_value(AgentFailureKind::ProviderUnavailable).unwrap();
        assert_eq!(json, "provider_unavailable");
        assert!(!AgentFailureKind::ProviderUnavailable.is_terminal());
        assert!(AgentFailureKind::AccountLimit.is_terminal());
    }
}
