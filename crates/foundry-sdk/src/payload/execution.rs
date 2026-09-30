use serde::{Deserialize, Serialize};

use super::context::ChainContext;

// ---------------------------------------------------------------------------
// Prompt execution workflow
// ---------------------------------------------------------------------------

/// Payload for `ExecutionRequested`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRequestedPayload {
    pub project: String,
    pub prompt: String,
    /// Opaque operator context captured by whatever client asked for this run —
    /// the CLI's hostname, and the `--origin` text an operator passed.
    ///
    /// Additive and optional: a payload that omits it records exactly the
    /// origins the ledger recorded before it existed. Nothing in the daemon
    /// parses, validates or routes on it; it is carried through to the ledger
    /// and displayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_origin: Option<String>,
    #[serde(flatten)]
    pub chain: ChainContext,
}
