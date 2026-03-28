use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A single quality-gate definition read from `.hone-gates.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateDefinition {
    pub name: String,
    pub command: String,
    pub required: bool,
    /// Optional per-gate timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
}

/// The outcome of running a single gate command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResult {
    pub name: String,
    pub command: String,
    pub passed: bool,
    pub required: bool,
    pub output: String,
    pub exit_code: i32,
}

/// Aggregated result of running all gates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatesRunResult {
    /// True when every gate (required and optional) passed.
    pub all_passed: bool,
    /// True when every *required* gate passed (optional failures tolerated).
    pub required_passed: bool,
    pub results: Vec<GateResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_definition_deserializes_from_json() {
        let json = r#"{"name":"fmt","command":"cargo fmt --check","required":true}"#;
        let gate: GateDefinition = serde_json::from_str(json).unwrap();
        assert_eq!(gate.name, "fmt");
        assert_eq!(gate.command, "cargo fmt --check");
        assert!(gate.required);
        assert!(gate.timeout.is_none());
    }

    #[test]
    fn gate_definition_with_timeout() {
        let json = r#"{"name":"test","command":"cargo test","required":false,"timeout":{"secs":60,"nanos":0}}"#;
        let gate: GateDefinition = serde_json::from_str(json).unwrap();
        assert_eq!(gate.timeout, Some(Duration::from_secs(60)));
    }

    #[test]
    fn gate_result_round_trips() {
        let result = GateResult {
            name: "clippy".to_string(),
            command: "cargo clippy".to_string(),
            passed: true,
            required: true,
            output: "ok".to_string(),
            exit_code: 0,
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: GateResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "clippy");
        assert!(restored.passed);
    }

    #[test]
    fn gates_run_result_round_trips() {
        let run_result = GatesRunResult {
            all_passed: false,
            required_passed: true,
            results: vec![
                GateResult {
                    name: "fmt".to_string(),
                    command: "cargo fmt --check".to_string(),
                    passed: true,
                    required: true,
                    output: String::new(),
                    exit_code: 0,
                },
                GateResult {
                    name: "lint".to_string(),
                    command: "cargo clippy".to_string(),
                    passed: false,
                    required: false,
                    output: "warnings".to_string(),
                    exit_code: 1,
                },
            ],
        };
        let json = serde_json::to_string(&run_result).unwrap();
        let restored: GatesRunResult = serde_json::from_str(&json).unwrap();
        assert!(!restored.all_passed);
        assert!(restored.required_passed);
        assert_eq!(restored.results.len(), 2);
    }
}
