use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Greet workflow
// ---------------------------------------------------------------------------

/// Payload for `GreetingRequested`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GreetingRequestedPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Payload for `GreetingComposed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GreetingComposedPayload {
    pub greeting: String,
}

/// Payload for `GreetingDelivered`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GreetingDeliveredPayload {
    pub delivered: bool,
    pub greeting: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
}
