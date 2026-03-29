use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
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

/// On-disk representation of `.hone-gates.json`.
#[derive(Serialize, Deserialize)]
struct GateFile {
    gates: Vec<RawGate>,
}

/// A single gate entry as it appears in JSON (timeout is seconds, not Duration).
#[derive(Serialize, Deserialize)]
struct RawGate {
    name: String,
    command: String,
    required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
}

/// Read gate definitions from `.hone-gates.json` in `project_dir`.
///
/// Returns an empty vec if the file does not exist.
/// Returns an error if the file exists but contains malformed JSON.
pub fn read_gates_file(project_dir: &Path) -> Result<Vec<GateDefinition>> {
    let path = project_dir.join(".hone-gates.json");

    if !path.exists() {
        return Ok(vec![]);
    }

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let file: GateFile = serde_json::from_str(&contents)
        .with_context(|| format!("malformed JSON in {}", path.display()))?;

    Ok(file
        .gates
        .into_iter()
        .map(|raw| GateDefinition {
            name: raw.name,
            command: raw.command,
            required: raw.required,
            timeout: raw.timeout.map(Duration::from_secs),
        })
        .collect())
}

/// Write gate definitions to `.hone-gates.json` in `project_dir`.
pub fn write_gates_file(project_dir: &Path, gates: &[GateDefinition]) -> Result<()> {
    let path = project_dir.join(".hone-gates.json");

    let file = GateFile {
        gates: gates
            .iter()
            .map(|g| RawGate {
                name: g.name.clone(),
                command: g.command.clone(),
                required: g.required,
                timeout: g.timeout.map(|d| d.as_secs()),
            })
            .collect(),
    };

    let json = serde_json::to_string_pretty(&file)?;
    std::fs::write(&path, format!("{json}\n"))
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(())
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

    // -- read_gates_file / write_gates_file tests --

    #[test]
    fn read_gates_file_valid() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true},{"name":"test","command":"cargo test","required":false,"timeout":120}]}"#,
        )
        .unwrap();

        let gates = read_gates_file(dir.path()).unwrap();
        assert_eq!(gates.len(), 2);
        assert_eq!(gates[0].name, "fmt");
        assert!(gates[0].required);
        assert!(gates[0].timeout.is_none());
        assert_eq!(gates[1].name, "test");
        assert!(!gates[1].required);
        assert_eq!(gates[1].timeout, Some(Duration::from_secs(120)));
    }

    #[test]
    fn read_gates_file_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let gates = read_gates_file(dir.path()).unwrap();
        assert!(gates.is_empty());
    }

    #[test]
    fn read_gates_file_malformed_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hone-gates.json"), "not json").unwrap();
        let err = read_gates_file(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("malformed JSON"));
    }

    #[test]
    fn write_and_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let gates = vec![
            GateDefinition {
                name: "fmt".to_string(),
                command: "cargo fmt --check".to_string(),
                required: true,
                timeout: None,
            },
            GateDefinition {
                name: "test".to_string(),
                command: "cargo test".to_string(),
                required: false,
                timeout: Some(Duration::from_secs(300)),
            },
        ];

        write_gates_file(dir.path(), &gates).unwrap();

        let loaded = read_gates_file(dir.path()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "fmt");
        assert_eq!(loaded[0].command, "cargo fmt --check");
        assert!(loaded[0].required);
        assert!(loaded[0].timeout.is_none());
        assert_eq!(loaded[1].name, "test");
        assert_eq!(loaded[1].timeout, Some(Duration::from_secs(300)));
    }

    #[test]
    fn write_gates_file_empty_gates() {
        let dir = tempfile::tempdir().unwrap();
        write_gates_file(dir.path(), &[]).unwrap();
        let loaded = read_gates_file(dir.path()).unwrap();
        assert!(loaded.is_empty());
    }
}
