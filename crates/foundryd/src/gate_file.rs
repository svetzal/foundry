use std::path::Path;

use anyhow::Result;
use foundry_core::gates::GateDefinition;

/// Read gate definitions from `.hone-gates.json` in `project_dir`.
///
/// Returns an empty vec if the file does not exist.
/// Returns an error if the file exists but contains malformed JSON.
pub fn read_gates(project_dir: &Path) -> Result<Vec<GateDefinition>> {
    foundry_core::gates::read_gates_file(project_dir)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn reads_valid_gates_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true},{"name":"test","command":"cargo test","required":false,"timeout":120}]}"#,
        )
        .unwrap();

        let gates = read_gates(dir.path()).unwrap();

        assert_eq!(gates.len(), 2);
        assert_eq!(gates[0].name, "fmt");
        assert_eq!(gates[0].command, "cargo fmt --check");
        assert!(gates[0].required);
        assert!(gates[0].timeout.is_none());
        assert_eq!(gates[1].name, "test");
        assert!(!gates[1].required);
        assert_eq!(gates[1].timeout, Some(Duration::from_secs(120)));
    }

    #[test]
    fn missing_file_returns_empty_vec() {
        let dir = tempfile::tempdir().unwrap();
        let gates = read_gates(dir.path()).unwrap();
        assert!(gates.is_empty());
    }

    #[test]
    fn malformed_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hone-gates.json"), "not json at all").unwrap();

        let err = read_gates(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("malformed JSON"), "unexpected error: {err:#}");
    }

    #[test]
    fn empty_gates_array() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hone-gates.json"), r#"{"gates":[]}"#).unwrap();

        let gates = read_gates(dir.path()).unwrap();
        assert!(gates.is_empty());
    }
}
