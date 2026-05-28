use serde::{Deserialize, Serialize};

/// Identifies the workflow type for a Foundry event chain.
///
/// Used to route and filter events through the correct block handlers.
/// Serializes to `snake_case` strings (`"iterate"`, `"maintain"`, etc.)
/// matching the payload convention used across all event types.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum WorkflowType {
    Iterate,
    Maintain,
    Prompt,
    Validate,
    Scout,
    Pipeline,
    Unknown,
}

impl WorkflowType {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    /// Read the workflow type from an event payload.
    ///
    /// Reads `payload["workflow"]` and parses it as a [`WorkflowType`].
    /// Falls back to [`WorkflowType::Unknown`] if the field is missing or unrecognized.
    pub fn from_payload(payload: &serde_json::Value) -> Self {
        payload
            .get("workflow")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse().ok())
            .unwrap_or(WorkflowType::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_serialize_as_snake_case() {
        let cases = [
            (WorkflowType::Iterate, "iterate"),
            (WorkflowType::Maintain, "maintain"),
            (WorkflowType::Prompt, "prompt"),
            (WorkflowType::Validate, "validate"),
            (WorkflowType::Scout, "scout"),
            (WorkflowType::Pipeline, "pipeline"),
            (WorkflowType::Unknown, "unknown"),
        ];
        for (variant, expected) in &cases {
            let serialized = serde_json::to_value(variant).unwrap();
            assert_eq!(serialized, serde_json::Value::String((*expected).to_string()));
        }
    }

    #[test]
    fn from_payload_reads_workflow_field() {
        let payload = serde_json::json!({"workflow": "iterate"});
        assert_eq!(WorkflowType::from_payload(&payload), WorkflowType::Iterate);
    }

    #[test]
    fn from_payload_falls_back_to_unknown() {
        let payload = serde_json::json!({});
        assert_eq!(WorkflowType::from_payload(&payload), WorkflowType::Unknown);
    }

    #[test]
    fn from_payload_unknown_string_gives_unknown() {
        let payload = serde_json::json!({"workflow": "something_else"});
        assert_eq!(WorkflowType::from_payload(&payload), WorkflowType::Unknown);
    }

    #[test]
    fn round_trip_from_str() {
        use std::str::FromStr;
        assert_eq!(WorkflowType::from_str("iterate").unwrap(), WorkflowType::Iterate);
        assert_eq!(WorkflowType::from_str("maintain").unwrap(), WorkflowType::Maintain);
        assert_eq!(WorkflowType::from_str("prompt").unwrap(), WorkflowType::Prompt);
        assert_eq!(WorkflowType::from_str("validate").unwrap(), WorkflowType::Validate);
        assert_eq!(WorkflowType::from_str("scout").unwrap(), WorkflowType::Scout);
        assert_eq!(WorkflowType::from_str("pipeline").unwrap(), WorkflowType::Pipeline);
        assert_eq!(WorkflowType::from_str("unknown").unwrap(), WorkflowType::Unknown);
    }

    #[test]
    fn as_str_returns_snake_case() {
        assert_eq!(WorkflowType::Iterate.as_str(), "iterate");
        assert_eq!(WorkflowType::Maintain.as_str(), "maintain");
        assert_eq!(WorkflowType::Unknown.as_str(), "unknown");
    }
}
