use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// A single quality-gate definition read from `.hone-gates.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateDefinition {
    pub name: String,
    pub command: String,
    pub required: bool,
    /// Optional per-gate timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    /// Optional command that attempts to auto-fix this gate's failure in place.
    ///
    /// When present and the gate `command` fails, the runner runs `fix_command`
    /// and then re-runs `command`; a passing re-check resolves the failure. The
    /// in-place changes left in the working tree are picked up by the downstream
    /// commit step. This is what lets mechanically-fixable gates (formatters,
    /// lint autofix) self-heal instead of deadlocking the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_command: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fix_command: Option<String>,
}

/// Read gate definitions from `.hone-gates.json` in `project_dir`.
///
/// Returns an empty vec if the file does not exist.
/// Returns an error if the file exists but contains malformed JSON.
///
/// # Errors
///
/// Returns [`StoreError::Io`] if the file exists but cannot be read, and
/// [`StoreError::Parse`] if the JSON is malformed. A missing file is not an
/// error — it returns an empty vec.
pub fn read_gates_file(project_dir: &Path) -> Result<Vec<GateDefinition>, StoreError> {
    let path = project_dir.join(".hone-gates.json");

    if !path.exists() {
        return Ok(vec![]);
    }

    let contents = std::fs::read_to_string(&path).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?;

    let file: GateFile = serde_json::from_str(&contents).map_err(|source| StoreError::Parse {
        path: path.clone(),
        source,
    })?;

    Ok(file
        .gates
        .into_iter()
        .map(|raw| GateDefinition {
            name: raw.name,
            command: raw.command,
            required: raw.required,
            timeout: raw.timeout.map(Duration::from_secs),
            fix_command: raw.fix_command,
        })
        .collect())
}

/// Write gate definitions to `.hone-gates.json` in `project_dir`.
///
/// # Errors
///
/// Returns [`StoreError::Parse`] if serialization fails (extremely rare) and
/// [`StoreError::Io`] if the write fails.
pub fn write_gates_file(project_dir: &Path, gates: &[GateDefinition]) -> Result<(), StoreError> {
    let path = project_dir.join(".hone-gates.json");

    let file = GateFile {
        gates: gates
            .iter()
            .map(|g| RawGate {
                name: g.name.clone(),
                command: g.command.clone(),
                required: g.required,
                timeout: g.timeout.map(|d| d.as_secs()),
                fix_command: g.fix_command.clone(),
            })
            .collect(),
    };

    let json = serde_json::to_string_pretty(&file).map_err(|source| StoreError::Parse {
        path: path.clone(),
        source,
    })?;
    std::fs::write(&path, format!("{json}\n")).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?;

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
    /// Wall-clock time the gate command took, in milliseconds.
    /// Absent for results loaded from older persisted events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// True when this gate initially failed but its `fix_command` repaired the
    /// working tree and the re-check then passed (a self-healed gate). Lets the
    /// triage layer and audit trail distinguish "passed clean" from "passed
    /// after autofix". Defaults to false for older persisted events.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fix_applied: bool,
}

/// serde `skip_serializing_if` helper — omit `fix_applied` when false so older
/// event consumers and clean-pass results stay byte-identical to before.
/// Signature must take `&bool`: serde passes the field by reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
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
            duration_ms: None,
            fix_applied: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: GateResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "clippy");
        assert!(restored.passed);
        assert!(restored.duration_ms.is_none());
        assert!(!restored.fix_applied);
        // duration_ms is omitted from JSON when None
        assert!(!json.contains("duration_ms"));
        // fix_applied is omitted from JSON when false
        assert!(!json.contains("fix_applied"));
    }

    #[test]
    fn gate_result_round_trips_with_duration() {
        let result = GateResult {
            name: "test".to_string(),
            command: "cargo test".to_string(),
            passed: true,
            required: true,
            output: String::new(),
            exit_code: 0,
            duration_ms: Some(1234),
            fix_applied: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"duration_ms\":1234"), "duration_ms must appear in JSON");
        let restored: GateResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.duration_ms, Some(1234));
        assert_eq!(restored.name, "test");
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
                    duration_ms: None,
                    fix_applied: false,
                },
                GateResult {
                    name: "lint".to_string(),
                    command: "cargo clippy".to_string(),
                    passed: false,
                    required: false,
                    output: "warnings".to_string(),
                    exit_code: 1,
                    duration_ms: None,
                    fix_applied: false,
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
                fix_command: Some("cargo fmt".to_string()),
            },
            GateDefinition {
                name: "test".to_string(),
                command: "cargo test".to_string(),
                required: false,
                timeout: Some(Duration::from_secs(300)),
                fix_command: None,
            },
        ];

        write_gates_file(dir.path(), &gates).unwrap();

        let loaded = read_gates_file(dir.path()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "fmt");
        assert_eq!(loaded[0].command, "cargo fmt --check");
        assert!(loaded[0].required);
        assert!(loaded[0].timeout.is_none());
        assert_eq!(loaded[0].fix_command.as_deref(), Some("cargo fmt"));
        assert_eq!(loaded[1].name, "test");
        assert_eq!(loaded[1].timeout, Some(Duration::from_secs(300)));
        assert!(loaded[1].fix_command.is_none());
    }

    #[test]
    fn read_gates_file_with_fix_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"format","command":"cmake --build build --target format-check","required":true,"fix_command":"cmake --build build --target format"}]}"#,
        )
        .unwrap();

        let gates = read_gates_file(dir.path()).unwrap();
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0].fix_command.as_deref(), Some("cmake --build build --target format"));
    }

    #[test]
    fn fix_command_omitted_from_json_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let gates = vec![GateDefinition {
            name: "test".to_string(),
            command: "cargo test".to_string(),
            required: true,
            timeout: None,
            fix_command: None,
        }];
        write_gates_file(dir.path(), &gates).unwrap();
        let contents = std::fs::read_to_string(dir.path().join(".hone-gates.json")).unwrap();
        assert!(!contents.contains("fix_command"));
    }

    #[test]
    fn write_gates_file_empty_gates() {
        let dir = tempfile::tempdir().unwrap();
        write_gates_file(dir.path(), &[]).unwrap();
        let loaded = read_gates_file(dir.path()).unwrap();
        assert!(loaded.is_empty());
    }
}
