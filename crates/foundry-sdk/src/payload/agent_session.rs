use serde::{Deserialize, Serialize};

use crate::gateway::AgentFailureMetadata;
use crate::token_rates::CostEstimate;
use crate::token_usage::SessionUsage;

// ---------------------------------------------------------------------------
// Agent session lifecycle payloads
// ---------------------------------------------------------------------------

/// Emitted when a Foundry-launched agent session begins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionStartedPayload {
    pub session_id: String,
    pub agent_type: String,
    pub project: String,
    pub working_dir: std::path::PathBuf,
    pub source_log_path: std::path::PathBuf,
    pub tier: String,
    /// The reasoning effort the block requested.
    pub effort: String,
    /// The reasoning effort the session actually runs at, after the provider's
    /// per-tier `effort_caps` are applied. Equal to `effort` when no cap
    /// applies. Empty on events recorded before caps existed.
    #[serde(default)]
    pub effective_effort: String,
    pub access: String,
    pub started_at: String,
    pub trace_id: String,
}

/// Emitted when a Foundry-launched agent session ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionEndedPayload {
    pub session_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub ended_at: String,
    pub bytes_written: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, flatten)]
    pub failure: AgentFailureMetadata,
    /// Tokens the session spent, recovered from its transcript.
    ///
    /// `None` means the session ended without writing a terminal usage record —
    /// unmeasured spend, not zero spend. Consumers must not treat an absent
    /// `usage` as free work; `cost.basis` says `unmeasured` in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SessionUsage>,
    /// What those tokens are worth, and how much that figure can be trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostEstimate>,
}

impl AgentSessionEndedPayload {
    /// The `status` of a session whose process died with the daemon that
    /// launched it, closed on the next daemon start rather than by the session
    /// itself.
    pub const STATUS_INTERRUPTED: &'static str = "interrupted";

    /// The end of a session that never recorded one because the daemon running
    /// it stopped.
    ///
    /// Nothing measured the session's last moments, so `exit_code`, `usage`
    /// and `cost` are absent and `bytes_written` is zero: this records that the
    /// session is over, not what it did.
    #[must_use]
    pub fn interrupted(session_id: &str, ended_at: &str, reason: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            status: Self::STATUS_INTERRUPTED.to_string(),
            exit_code: None,
            ended_at: ended_at.to_string(),
            bytes_written: 0,
            error: Some(reason.to_string()),
            failure: AgentFailureMetadata::default(),
            usage: None,
            cost: None,
        }
    }
}
