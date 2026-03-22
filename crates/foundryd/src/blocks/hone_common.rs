/// Extract a human-readable summary from hone's JSON output.
/// Falls back gracefully when the output is not valid JSON or lacks the expected field.
pub(crate) fn parse_hone_summary(stdout: &str, success: bool) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) {
        if let Some(summary) =
            value.get("summary").or_else(|| value.get("message")).and_then(|v| v.as_str())
        {
            return summary.to_string();
        }
    }

    // Fall back to first non-empty line of raw output.
    stdout.lines().find(|l| !l.trim().is_empty()).map_or_else(
        || {
            if success {
                "completed".to_string()
            } else {
                "failed (no output)".to_string()
            }
        },
        str::to_string,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hone_summary_extracts_json_summary_field() {
        let json = r#"{"summary":"fixed 2 vulnerabilities","changed":true}"#;
        assert_eq!(parse_hone_summary(json, true), "fixed 2 vulnerabilities");
    }

    #[test]
    fn parse_hone_summary_falls_back_to_first_line() {
        let raw = "Patching dependency tree\nDone.";
        assert_eq!(parse_hone_summary(raw, true), "Patching dependency tree");
    }

    #[test]
    fn parse_hone_summary_empty_output_failure() {
        assert_eq!(parse_hone_summary("", false), "failed (no output)");
    }

    #[test]
    fn parse_hone_summary_empty_output_success() {
        assert_eq!(parse_hone_summary("", true), "completed");
    }
}
