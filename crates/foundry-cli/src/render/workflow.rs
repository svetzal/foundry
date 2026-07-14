//! Pure rendering for workflow watch events, scout results, and validation results.

use std::fmt::Write as _;

use foundry_sdk::event::PayloadExt;

use crate::proto::WatchResponse;

/// Format a single watch event as a display line (ends with `\n`).
pub fn watch_event_line(event: &WatchResponse) -> String {
    match event.event_type.as_str() {
        "block_started" => block_started_line(event),
        "block_completed" => block_completed_line(event),
        _ => {
            let status = extract_status(&event.payload_json);
            format!("[{}] {} {}\n", event.project, event.event_type, status)
        }
    }
}

fn block_started_line(event: &WatchResponse) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&event.payload_json) else {
        return format!("[{}] block started\n", event.project);
    };
    let block = v.str_or("block", "unknown block");
    let trigger = v.str_or("trigger_event_type", "unknown trigger");
    format!("[{}] running block {block} (from {trigger})\n", event.project)
}

fn block_completed_line(event: &WatchResponse) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&event.payload_json) else {
        return format!("[{}] block completed\n", event.project);
    };
    let block = v.str_or("block", "unknown block");
    let status = v.str_or("status", "done");
    let duration = v
        .get("duration_ms")
        .and_then(serde_json::Value::as_u64)
        .map(format_duration)
        .unwrap_or_default();
    let suffix = if duration.is_empty() {
        format!("({status})")
    } else {
        format!("({status}, {duration})")
    };
    format!("[{}] finished block {block} {suffix}\n", event.project)
}

pub(crate) fn format_duration(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        format!("{duration_ms}ms")
    } else {
        let seconds = duration_ms / 1_000;
        let tenths = (duration_ms % 1_000) / 100;
        format!("{seconds}.{tenths}s")
    }
}

/// Extract a compact status hint from the event payload JSON.
pub(crate) fn extract_status(payload_json: &str) -> String {
    if payload_json.is_empty() || payload_json == "{}" {
        return String::new();
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload_json) {
        if let Some(success) = v.get("success").and_then(serde_json::Value::as_bool) {
            return if success {
                "(ok)".to_string()
            } else {
                "(FAILED)".to_string()
            };
        }
        if let Some(status) = v.get("status").and_then(serde_json::Value::as_str) {
            return format!("({status})");
        }
    }
    String::new()
}

/// Format a scout (drift assessment) result as a multi-line string.
pub fn scout_result(project: &str, payload_json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload_json) else {
        return format!("  {project}: could not parse result\n");
    };

    let candidate_count = v.u64_or("candidate_count", 0);
    let high_value_count = v.u64_or("high_value_count", 0);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{project}: {candidate_count} candidates found, {high_value_count} high-value"
    );
    let _ = writeln!(out);

    if let Some(err) = v.get("parse_error").and_then(serde_json::Value::as_str) {
        let _ = writeln!(out, "  Parse error: {err}");
        return out;
    }

    if let Some(candidates) = v.get("candidates").and_then(serde_json::Value::as_array) {
        for candidate in candidates {
            let rank = candidate.u64_or("rank", 0);
            let summary = candidate.str_or("summary", "(no summary)");
            let divergence = candidate.str_or("divergence_type", "unknown");
            let high_value = candidate.bool_or("high_value", false);
            let confidence = candidate.str_or("confidence", "unknown");
            let next_step = candidate.str_or("suggested_next_step", "unknown");

            let marker = if high_value { " ***" } else { "" };
            let _ = writeln!(out, "  #{rank} [{divergence}] {summary}{marker}");

            if let Some(impact) = candidate.get("impact") {
                let severity = impact.str_or("severity", "?");
                let frequency = impact.str_or("frequency", "?");
                let risk_type = impact.str_or("risk_type", "?");
                let _ = writeln!(
                    out,
                    "     severity={severity} frequency={frequency} risk={risk_type}"
                );
            }

            let _ = writeln!(out, "     confidence={confidence} next={next_step}");

            if let Some(explanation) =
                candidate.get("explanation").and_then(serde_json::Value::as_str)
            {
                let _ = writeln!(out, "     {explanation}");
            }

            let _ = writeln!(out);
        }
    }

    out
}

/// Format per-gate pass/fail results for a validation as a multi-line string.
pub fn validation_result(project: &str, payload_json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload_json) else {
        return format!("  {project}: could not parse result\n");
    };

    let success = v.bool_or("success", false);
    let status = if success { "PASS" } else { "FAIL" };

    let mut out = String::new();
    let _ = writeln!(out, "  {project}: {status}");

    if let Some(results) = v.get("results").and_then(serde_json::Value::as_array) {
        for gate in results {
            let name = gate.str_or("name", "unknown");
            let passed = gate.bool_or("passed", false);
            let required = gate.bool_or("required", true);
            let marker = if passed { "ok" } else { "FAILED" };
            let req = if required { "required" } else { "optional" };
            let _ = write!(out, "    {name}: {marker} ({req})");
            if !passed
                && let Some(output) = gate.get("output").and_then(serde_json::Value::as_str)
                && !output.is_empty()
            {
                let snippet: String = output.chars().take(200).collect();
                let _ = write!(out, " \u{2014} {snippet}");
            }
            let _ = writeln!(out);
        }
    }

    out
}

/// Returns `true` when the validation payload indicates failure (or is unparseable).
pub fn validation_failed(payload_json: &str) -> bool {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload_json) {
        !v.bool_or("success", false)
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::WatchResponse;

    fn watch_response(event_type: &str, project: &str, payload_json: &str) -> WatchResponse {
        WatchResponse {
            event_id: "evt_1".to_string(),
            event_type: event_type.to_string(),
            project: project.to_string(),
            payload_json: payload_json.to_string(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
        }
    }

    // -- format_duration tests (moved from workflow_commands.rs) --

    #[test]
    fn format_duration_uses_ms_below_one_second_and_tenths_after() {
        assert_eq!(format_duration(999), "999ms");
        assert_eq!(format_duration(1_000), "1.0s");
        assert_eq!(format_duration(143_967), "143.9s");
    }

    // -- extract_status tests (moved from workflow_commands.rs) --

    #[test]
    fn extract_status_success() {
        assert_eq!(extract_status(r#"{"success":true}"#), "(ok)");
    }

    #[test]
    fn extract_status_failure() {
        assert_eq!(extract_status(r#"{"success":false}"#), "(FAILED)");
    }

    #[test]
    fn extract_status_string() {
        assert_eq!(extract_status(r#"{"status":"skipped"}"#), "(skipped)");
    }

    #[test]
    fn extract_status_empty() {
        assert_eq!(extract_status("{}"), String::new());
        assert_eq!(extract_status(""), String::new());
    }

    // -- block_started_line tests --

    #[test]
    fn block_started_line_formats_known_block_and_trigger() {
        let e = watch_response(
            "block_started",
            "myproj",
            r#"{"block":"MyBlock","trigger_event_type":"SomethingRequested"}"#,
        );
        assert_eq!(
            block_started_line(&e),
            "[myproj] running block MyBlock (from SomethingRequested)\n"
        );
    }

    #[test]
    fn block_started_line_falls_back_on_unparseable_payload() {
        let e = watch_response("block_started", "myproj", "not-json");
        assert_eq!(block_started_line(&e), "[myproj] block started\n");
    }

    // -- block_completed_line tests --

    #[test]
    fn block_completed_line_includes_duration_when_present() {
        let e = watch_response(
            "block_completed",
            "myproj",
            r#"{"block":"MyBlock","status":"ok","duration_ms":1500}"#,
        );
        assert_eq!(block_completed_line(&e), "[myproj] finished block MyBlock (ok, 1.5s)\n");
    }

    #[test]
    fn block_completed_line_omits_duration_when_absent() {
        let e = watch_response(
            "block_completed",
            "myproj",
            r#"{"block":"MyBlock","status":"skipped"}"#,
        );
        assert_eq!(block_completed_line(&e), "[myproj] finished block MyBlock (skipped)\n");
    }

    #[test]
    fn block_completed_line_falls_back_on_unparseable_payload() {
        let e = watch_response("block_completed", "myproj", "not-json");
        assert_eq!(block_completed_line(&e), "[myproj] block completed\n");
    }

    // -- watch_event_line tests --

    #[test]
    fn watch_event_line_dispatches_block_started() {
        let e = watch_response("block_started", "p", r#"{"block":"B","trigger_event_type":"T"}"#);
        let line = watch_event_line(&e);
        assert!(line.contains("running block B"), "got: {line}");
    }

    #[test]
    fn watch_event_line_dispatches_block_completed() {
        let e = watch_response("block_completed", "p", r#"{"block":"B","status":"ok"}"#);
        let line = watch_event_line(&e);
        assert!(line.contains("finished block B"), "got: {line}");
    }

    #[test]
    fn watch_event_line_formats_other_events_with_status() {
        let e = watch_response("project_run_started", "p", r#"{"status":"running"}"#);
        assert_eq!(watch_event_line(&e), "[p] project_run_started (running)\n");
    }

    #[test]
    fn watch_event_line_formats_other_events_no_status() {
        let e = watch_response("some_event", "proj", "{}");
        assert_eq!(watch_event_line(&e), "[proj] some_event \n");
    }

    // -- scout_result tests --

    #[test]
    fn scout_result_unparseable_payload() {
        let r = scout_result("myproj", "not-json");
        assert!(r.contains("could not parse result"), "got: {r}");
    }

    #[test]
    fn scout_result_parse_error_branch() {
        let payload = r#"{"candidate_count":0,"high_value_count":0,"parse_error":"syntax error"}"#;
        let r = scout_result("myproj", payload);
        assert!(r.contains("Parse error: syntax error"), "got: {r}");
    }

    #[test]
    fn scout_result_high_value_marker() {
        let payload = r#"{"candidate_count":1,"high_value_count":1,"candidates":[{"rank":1,"summary":"s","divergence_type":"dt","high_value":true,"confidence":"high","suggested_next_step":"ns"}]}"#;
        let r = scout_result("myproj", payload);
        assert!(r.contains(" ***"), "got: {r}");
    }

    #[test]
    fn scout_result_no_high_value_marker_when_false() {
        let payload = r#"{"candidate_count":1,"high_value_count":0,"candidates":[{"rank":1,"summary":"s","divergence_type":"dt","high_value":false,"confidence":"high","suggested_next_step":"ns"}]}"#;
        let r = scout_result("myproj", payload);
        assert!(!r.contains(" ***"), "got: {r}");
    }

    #[test]
    fn scout_result_impact_rendering() {
        let payload = r#"{"candidate_count":1,"high_value_count":0,"candidates":[{"rank":1,"summary":"s","divergence_type":"dt","high_value":false,"confidence":"high","suggested_next_step":"ns","impact":{"severity":"high","frequency":"often","risk_type":"data"}}]}"#;
        let r = scout_result("myproj", payload);
        assert!(r.contains("severity=high"), "got: {r}");
        assert!(r.contains("frequency=often"), "got: {r}");
        assert!(r.contains("risk=data"), "got: {r}");
    }

    #[test]
    fn scout_result_explanation_present() {
        let payload = r#"{"candidate_count":1,"high_value_count":0,"candidates":[{"rank":1,"summary":"s","divergence_type":"dt","high_value":false,"confidence":"high","suggested_next_step":"ns","explanation":"because"}]}"#;
        let r = scout_result("myproj", payload);
        assert!(r.contains("because"), "got: {r}");
    }

    #[test]
    fn scout_result_explanation_absent() {
        let payload = r#"{"candidate_count":1,"high_value_count":0,"candidates":[{"rank":1,"summary":"s","divergence_type":"dt","high_value":false,"confidence":"high","suggested_next_step":"ns"}]}"#;
        let r = scout_result("myproj", payload);
        assert!(!r.is_empty());
    }

    // -- validation_result tests --

    #[test]
    fn validation_result_shows_pass_header() {
        let payload = r#"{"success":true,"results":[]}"#;
        let r = validation_result("proj", payload);
        assert!(r.contains("PASS"), "got: {r}");
    }

    #[test]
    fn validation_result_shows_fail_header() {
        let payload = r#"{"success":false,"results":[]}"#;
        let r = validation_result("proj", payload);
        assert!(r.contains("FAIL"), "got: {r}");
    }

    #[test]
    fn validation_result_truncates_output_at_200_chars() {
        let long_output = "x".repeat(300);
        let payload = format!(
            r#"{{"success":false,"results":[{{"name":"g","passed":false,"required":true,"output":"{long_output}"}}]}}"#
        );
        let r = validation_result("proj", &payload);
        let expected_snippet: String = "x".chars().take(200).collect();
        assert!(r.contains(&expected_snippet), "got: {r}");
        // Should not contain the 201st 'x' — only the 200-char snippet is included.
        assert_eq!(r.matches('x').count(), 200, "got: {r}");
    }

    #[test]
    fn validation_result_empty_output_no_em_dash() {
        let payload = r#"{"success":true,"results":[{"name":"clippy","passed":true,"required":true,"output":""}]}"#;
        let r = validation_result("proj", payload);
        assert!(!r.contains('\u{2014}'), "got: {r}");
    }

    #[test]
    fn validation_result_optional_gate_label() {
        let payload = r#"{"success":true,"results":[{"name":"optional-check","passed":true,"required":false}]}"#;
        let r = validation_result("proj", payload);
        assert!(r.contains("optional"), "got: {r}");
    }

    // -- validation_failed tests --

    #[test]
    fn validation_failed_when_success_true_returns_false() {
        assert!(!validation_failed(r#"{"success":true}"#));
    }

    #[test]
    fn validation_failed_when_success_false_returns_true() {
        assert!(validation_failed(r#"{"success":false}"#));
    }

    #[test]
    fn validation_failed_when_success_missing_defaults_to_true() {
        assert!(validation_failed(r#"{"other":"field"}"#));
    }

    #[test]
    fn validation_failed_when_unparseable_returns_true() {
        assert!(validation_failed("not-json"));
    }
}
