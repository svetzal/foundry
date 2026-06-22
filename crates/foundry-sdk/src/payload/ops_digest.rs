use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Ops-digest workflow — periodic summary of MBOS operational events
// ---------------------------------------------------------------------------

/// Payload for `OpsDigestStarted` (cycle-root, emitted by the sentinel).
///
/// Mirrors `CommitDigestStartedPayload` for symmetry. The sentinel emits an
/// empty payload (`{}`); the event count defaults to zero on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpsDigestStartedPayload {
    #[serde(default)]
    pub event_count: u64,
}

/// A lean per-event summary carried in the `OpsObserved` payload.
///
/// Captures only the fields the downstream summariser actually needs.
/// We deliberately avoid carrying full event bodies — the digest is a
/// high-level operational scan, not a raw event dump.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpsEventDigest {
    /// Unique MBOS event ID.
    pub id: String,
    /// MBOS event type string (e.g., `"ci_pipeline_failure"`).
    pub event_type: String,
    /// ISO 8601 timestamp when the event occurred (`occurredAt`).
    pub occurred_at: String,
    /// Classified domain bucket (e.g., `"clients"`, `"infrastructure"`, `"ai"`).
    pub domain: String,
    /// MBOS urgency label (`"P0"`, `"P1"`, `"P2"`). Absent on legacy events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urgency: Option<String>,
    /// Human-readable one-line `summary` from the MBOS event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Client name when the event carries a `client` object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

/// Payload for `OpsObserved` — the pressure-gated evidence the summariser
/// will turn into an ops digest.
///
/// When `proceed` is `false` the downstream blocks self-filter and
/// `OpsDigestCompleted{skipped: true}` is emitted instead.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpsObservedPayload {
    /// `true` when the gate was satisfied (count >= threshold or anomaly present).
    #[serde(default)]
    pub proceed: bool,
    /// Number of new MBOS events since the last watermark.
    #[serde(default)]
    pub new_event_count: u64,
    /// `true` when at least one event in the window is classified as an anomaly.
    #[serde(default)]
    pub anomaly_present: bool,
    /// The watermark that would be written if the chain completes. `None` when
    /// there are no new events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_watermark: Option<String>,
    /// Lean summaries of every event in the window, for the summariser.
    #[serde(default)]
    pub events: Vec<OpsEventDigest>,
}

/// Payload for `OpsSummaryCompleted` — the agent's rendered digest body plus
/// bookkeeping totals.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpsSummaryCompletedPayload {
    pub markdown: String,
    #[serde(default)]
    pub event_count: u64,
    /// The watermark to advance once the digest is written to disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_watermark: Option<String>,
}

/// Payload for `OpsDigestCompleted` — the formation's terminal event.
///
/// `digest_path` is `None` on a dry-run firing (chain ran, file not written),
/// on a skipped firing (`skipped: true`), and on any persistence failure
/// (`success: false`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpsDigestCompletedPayload {
    pub success: bool,
    /// `true` when the pressure gate was not satisfied and the chain was
    /// short-circuited without calling the agent or writing a file.
    #[serde(default)]
    pub skipped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest_path: Option<String>,
    #[serde(default)]
    pub event_count: u64,
}
