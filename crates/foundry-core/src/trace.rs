use serde::{Deserialize, Serialize};

use crate::event::Event;

/// Lightweight summary of a stored trace, suitable for listing without loading
/// the full `raw_output` content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceIndex {
    pub event_id: String,
    pub event_type: String,
    pub project: String,
    pub success: bool,
    pub total_duration_ms: u64,
}

/// Record of a single block execution within a processing chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockExecution {
    /// Name of the block that ran.
    pub block_name: String,
    /// The `event_id` that triggered this block.
    pub trigger_event_id: String,
    /// Whether the block succeeded.
    pub success: bool,
    /// Human-readable summary from the block.
    pub summary: String,
    /// Event IDs emitted by this block (empty if suppressed or failed).
    pub emitted_event_ids: Vec<String>,
    /// Wall-clock milliseconds spent executing this block (including retries).
    pub duration_ms: u64,
    /// Combined stdout+stderr from any shell command run by this block.
    pub raw_output: Option<String>,
    /// Exit code from any shell command run by this block.
    pub exit_code: Option<i32>,
    /// The payload of the event that triggered this block.
    pub trigger_payload: serde_json::Value,
    /// The payloads of events emitted by this block.
    pub emitted_payloads: Vec<serde_json::Value>,
    /// Paths to audit artifacts produced by this block.
    #[serde(default)]
    pub audit_artifacts: Vec<String>,
}

/// The full result of processing an event chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessResult {
    /// All events produced during the chain (including the root).
    pub events: Vec<Event>,
    /// Record of each block execution in order.
    pub block_executions: Vec<BlockExecution>,
    /// Wall-clock milliseconds for the entire `process()` call.
    pub total_duration_ms: u64,
}
