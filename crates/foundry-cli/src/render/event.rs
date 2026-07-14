//! Pure rendering for event traces, history tables, and live watch/status output.

use std::collections::HashMap;
use std::fmt::Write as _;

use comfy_table::{ContentArrangement, Table};
use foundry_sdk::event::PayloadExt;
use foundry_sdk::trace::TraceIndex;

use crate::proto::{TraceBlockExecution, TraceEvent, TraceResponse, WatchResponse, WorkflowStatus};

/// Render a flat (legacy / `--flat`) trace as a multi-line string.
pub fn flat_trace(response: &TraceResponse, verbose: bool) -> String {
    let mut out = String::new();

    // Build a lookup: event_id -> event
    let events: HashMap<&str, _> =
        response.events.iter().map(|e| (e.event_id.as_str(), e)).collect();

    // Build a lookup: trigger_event_id -> vec of block executions
    let mut blocks_by_trigger: HashMap<&str, Vec<_>> = HashMap::new();
    for block in &response.block_executions {
        blocks_by_trigger
            .entry(block.trigger_event_id.as_str())
            .or_default()
            .push(block);
    }

    // Build a lookup: emitted_event_id -> payload_json
    let mut event_payloads: HashMap<&str, &str> = HashMap::new();
    for block in &response.block_executions {
        for (i, event_id) in block.emitted_event_ids.iter().enumerate() {
            if let Some(payload) = block.emitted_payload_jsons.get(i) {
                event_payloads.insert(event_id.as_str(), payload.as_str());
            }
        }
    }

    if let Some(root) = response.events.first() {
        if !root.trace_id.is_empty() {
            let _ = writeln!(out, "trace: {}", root.trace_id);
        }
        event_tree(root, &events, &blocks_by_trigger, &event_payloads, 0, verbose, &mut out);
    }

    out
}

fn event_tree(
    event: &TraceEvent,
    events: &HashMap<&str, &TraceEvent>,
    blocks_by_trigger: &HashMap<&str, Vec<&TraceBlockExecution>>,
    event_payloads: &HashMap<&str, &str>,
    depth: usize,
    verbose: bool,
    out: &mut String,
) {
    let indent = "  ".repeat(depth);

    // Special rendering for skill-install results: compact inline format.
    if event.event_type == "local_skill_install_completed" {
        let payload = event_payloads.get(event.event_id.as_str()).copied().unwrap_or("{}");
        let _ = write!(out, "{}", skill_install_line(payload, &indent));
        return;
    }

    let _ = writeln!(
        out,
        "{}{} ({}) project={}",
        indent, event.event_type, event.event_id, event.project
    );

    if let Some(blocks) = blocks_by_trigger.get(event.event_id.as_str()) {
        for block in blocks {
            let status = if block.success { "ok" } else { "FAILED" };
            let _ = writeln!(
                out,
                "{}  \u{2192} {} ({}ms): {} \u{2014} {}",
                indent, block.block_name, block.duration_ms, status, block.summary
            );

            if verbose {
                if !block.trigger_payload_json.is_empty() && block.trigger_payload_json != "{}" {
                    let _ = writeln!(out, "{indent}    trigger: {}", block.trigger_payload_json);
                }
                for (i, payload) in block.emitted_payload_jsons.iter().enumerate() {
                    let _ = writeln!(out, "{indent}    emitted[{i}]: {payload}");
                }
                if !block.raw_output.is_empty() {
                    let _ = writeln!(out, "{indent}    output:");
                    for line in block.raw_output.lines() {
                        let _ = writeln!(out, "{indent}      {line}");
                    }
                }
                if !block.audit_artifacts.is_empty() {
                    let _ = writeln!(out, "{indent}    artifacts:");
                    for path in &block.audit_artifacts {
                        let _ = writeln!(out, "{indent}      {path}");
                    }
                }
            }

            // Recurse into emitted events
            for emitted_id in &block.emitted_event_ids {
                if let Some(emitted_event) = events.get(emitted_id.as_str()) {
                    event_tree(
                        emitted_event,
                        events,
                        blocks_by_trigger,
                        event_payloads,
                        depth + 2,
                        verbose,
                        out,
                    );
                }
            }
        }
    }
}

/// Render a `local_skill_install_completed` event in compact inline format.
fn skill_install_line(payload_json: &str, indent: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload_json) {
        let success = v.bool_or("success", false);
        let command = v.str_or("command", "(unknown command)");
        if success {
            format!("{indent}  \u{2192} Install Skill: ok \u{2014} {command}\n")
        } else {
            let stderr = v.str_or("stderr_tail", "(no output)");
            let detail = if stderr.is_empty() {
                "(no output)"
            } else {
                stderr
            };
            format!("{indent}  \u{2192} Install Skill: warn \u{2014} command failed: {detail}\n")
        }
    } else {
        format!("{indent}  \u{2192} Install Skill: (unparseable payload)\n")
    }
}

/// Render a date header and trace-index table as a multi-line string.
pub fn trace_table(date: &str, indices: &[TraceIndex]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{date}");
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["Event ID", "Trace", "Status", "Duration", "Type", "Project"]);

    for idx in indices {
        let status = if idx.success { "ok" } else { "FAILED" };
        let trace = idx.trace_id.as_deref().unwrap_or("-");
        table.add_row(vec![
            &idx.event_id,
            trace,
            status,
            &format!("{}ms", idx.total_duration_ms),
            &idx.event_type,
            &idx.project,
        ]);
    }

    let _ = writeln!(out, "{table}");
    out
}

/// Render a single live watch event as a display line (ends with `\n`).
pub fn watch_line(event: &WatchResponse) -> String {
    let mut out = String::new();
    let trace_suffix = if event.trace_id.is_empty() {
        String::new()
    } else {
        format!(" trace={}", event.trace_id)
    };
    let _ = writeln!(
        out,
        "{} {} project={}{}",
        event.event_id, event.event_type, event.project, trace_suffix
    );
    if !event.payload_json.is_empty() && event.payload_json != "{}" {
        let _ = writeln!(out, "  payload: {}", event.payload_json);
    }
    out
}

/// Render a list of active workflows as a multi-line string.
pub fn workflow_status_block(workflows: &[WorkflowStatus]) -> String {
    let mut out = String::new();
    for wf in workflows {
        let _ = writeln!(
            out,
            "{} [{}] {} \u{2014} {}",
            wf.workflow_id, wf.workflow_type, wf.project, wf.state
        );
        for tb in &wf.task_blocks {
            let throttled = if tb.throttled { " (throttled)" } else { "" };
            let _ = writeln!(out, "  {} \u{2014} {}{}", tb.name, tb.state, throttled);
        }
    }
    out
}

/// Return the appropriate "no workflows" message depending on whether a trace filter is active.
pub fn no_workflows_message(target_trace_id: Option<&str>) -> &'static str {
    if target_trace_id.is_some() {
        "No active workflows in span's trace."
    } else {
        "No active workflows."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{
        TaskBlockStatus, TraceBlockExecution, TraceEvent, TraceResponse, WatchResponse,
        WorkflowStatus,
    };

    fn make_event(event_id: &str, event_type: &str, project: &str, trace_id: &str) -> TraceEvent {
        TraceEvent {
            event_id: event_id.to_string(),
            event_type: event_type.to_string(),
            project: project.to_string(),
            occurred_at: String::new(),
            throttle: 0,
            trace_id: trace_id.to_string(),
            span_id: String::new(),
            parent_span_id: String::new(),
        }
    }

    fn make_block(
        block_name: &str,
        trigger_event_id: &str,
        success: bool,
        summary: &str,
        duration_ms: u64,
    ) -> TraceBlockExecution {
        TraceBlockExecution {
            block_name: block_name.to_string(),
            trigger_event_id: trigger_event_id.to_string(),
            success,
            summary: summary.to_string(),
            emitted_event_ids: vec![],
            duration_ms,
            raw_output: String::new(),
            exit_code: 0,
            trigger_payload_json: String::new(),
            emitted_payload_jsons: vec![],
            audit_artifacts: vec![],
            span_id: String::new(),
            parent_span_id: String::new(),
        }
    }

    fn make_watch_response(
        event_id: &str,
        event_type: &str,
        project: &str,
        trace_id: &str,
        payload_json: &str,
    ) -> WatchResponse {
        WatchResponse {
            event_id: event_id.to_string(),
            event_type: event_type.to_string(),
            project: project.to_string(),
            payload_json: payload_json.to_string(),
            trace_id: trace_id.to_string(),
            span_id: String::new(),
            parent_span_id: String::new(),
        }
    }

    fn make_workflow(
        workflow_id: &str,
        workflow_type: &str,
        project: &str,
        state: &str,
    ) -> WorkflowStatus {
        WorkflowStatus {
            workflow_id: workflow_id.to_string(),
            workflow_type: workflow_type.to_string(),
            project: project.to_string(),
            state: state.to_string(),
            started_at: String::new(),
            completed_at: String::new(),
            task_blocks: vec![],
            trace_id: String::new(),
        }
    }

    // -- flat_trace tests --

    #[test]
    fn flat_trace_empty_response_returns_empty_string() {
        let resp = TraceResponse {
            found: true,
            events: vec![],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        assert_eq!(flat_trace(&resp, false), "");
    }

    #[test]
    fn flat_trace_includes_trace_id_when_present() {
        let resp = TraceResponse {
            found: true,
            events: vec![make_event("evt_1", "cycle_started", "p", "trace_abc")],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        let out = flat_trace(&resp, false);
        assert!(out.contains("trace: trace_abc"), "got: {out}");
    }

    #[test]
    fn flat_trace_no_trace_line_when_trace_id_empty() {
        let resp = TraceResponse {
            found: true,
            events: vec![make_event("evt_1", "cycle_started", "p", "")],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        let out = flat_trace(&resp, false);
        assert!(!out.contains("trace:"), "got: {out}");
    }

    #[test]
    fn flat_trace_includes_event_and_block() {
        let resp = TraceResponse {
            found: true,
            events: vec![make_event("evt_1", "cycle_started", "proj", "")],
            block_executions: vec![make_block("FanOut", "evt_1", true, "ok summary", 100)],
            total_duration_ms: 100,
        };
        let out = flat_trace(&resp, false);
        assert!(out.contains("cycle_started"), "got: {out}");
        assert!(out.contains("FanOut"), "got: {out}");
        assert!(out.contains("ok summary"), "got: {out}");
    }

    #[test]
    fn flat_trace_verbose_shows_trigger_payload() {
        let mut block = make_block("B", "evt_1", true, "s", 0);
        block.trigger_payload_json = r#"{"key":"val"}"#.to_string();
        let resp = TraceResponse {
            found: true,
            events: vec![make_event("evt_1", "ev", "p", "")],
            block_executions: vec![block],
            total_duration_ms: 0,
        };
        let out = flat_trace(&resp, true);
        assert!(out.contains("trigger:"), "got: {out}");
        assert!(out.contains(r#"{"key":"val"}"#), "got: {out}");
    }

    // -- skill_install_line tests --

    #[test]
    fn skill_install_line_success() {
        let payload = r#"{"success":true,"command":"gilt init --global"}"#;
        let line = skill_install_line(payload, "");
        assert!(line.contains("Install Skill: ok"), "got: {line}");
        assert!(line.contains("gilt init --global"), "got: {line}");
    }

    #[test]
    fn skill_install_line_failure_with_stderr() {
        let payload = r#"{"success":false,"command":"gilt init","stderr_tail":"error: not found"}"#;
        let line = skill_install_line(payload, "");
        assert!(line.contains("warn"), "got: {line}");
        assert!(line.contains("error: not found"), "got: {line}");
    }

    #[test]
    fn skill_install_line_failure_empty_stderr() {
        let payload = r#"{"success":false,"command":"gilt init","stderr_tail":""}"#;
        let line = skill_install_line(payload, "");
        assert!(line.contains("(no output)"), "got: {line}");
    }

    #[test]
    fn skill_install_line_unparseable_payload() {
        let line = skill_install_line("not-json", "");
        assert!(line.contains("(unparseable payload)"), "got: {line}");
    }

    // -- trace_table tests --

    #[test]
    fn trace_table_shows_failed_status() {
        let indices = vec![TraceIndex {
            event_id: "evt_1".to_string(),
            event_type: "cycle_started".to_string(),
            project: "proj".to_string(),
            success: false,
            total_duration_ms: 500,
            trace_id: None,
        }];
        let out = trace_table("2026-01-01", &indices);
        assert!(out.contains("FAILED"), "got: {out}");
    }

    #[test]
    fn trace_table_shows_dash_for_none_trace_id() {
        let indices = vec![TraceIndex {
            event_id: "evt_1".to_string(),
            event_type: "x".to_string(),
            project: "p".to_string(),
            success: true,
            total_duration_ms: 0,
            trace_id: None,
        }];
        let out = trace_table("2026-01-01", &indices);
        assert!(out.contains('-'), "got: {out}");
    }

    #[test]
    fn trace_table_formats_duration_as_ms() {
        let indices = vec![TraceIndex {
            event_id: "evt_1".to_string(),
            event_type: "x".to_string(),
            project: "p".to_string(),
            success: true,
            total_duration_ms: 1234,
            trace_id: None,
        }];
        let out = trace_table("2026-01-01", &indices);
        assert!(out.contains("1234ms"), "got: {out}");
    }

    // -- watch_line tests --

    #[test]
    fn watch_line_includes_trace_when_present() {
        let e = make_watch_response("evt_1", "ev", "p", "trace_abc", "{}");
        let line = watch_line(&e);
        assert!(line.contains("trace=trace_abc"), "got: {line}");
    }

    #[test]
    fn watch_line_no_trace_suffix_when_empty() {
        let e = make_watch_response("evt_1", "ev", "p", "", "{}");
        let line = watch_line(&e);
        assert!(!line.contains("trace="), "got: {line}");
    }

    #[test]
    fn watch_line_includes_payload_when_present() {
        let e = make_watch_response("evt_1", "ev", "p", "", r#"{"k":"v"}"#);
        let line = watch_line(&e);
        assert!(line.contains("payload:"), "got: {line}");
    }

    #[test]
    fn watch_line_no_payload_line_for_empty_payload() {
        let e = make_watch_response("evt_1", "ev", "p", "", "{}");
        let line = watch_line(&e);
        assert!(!line.contains("payload:"), "got: {line}");
    }

    #[test]
    fn watch_line_no_payload_line_for_truly_empty_string() {
        let e = make_watch_response("evt_1", "ev", "p", "", "");
        let line = watch_line(&e);
        assert!(!line.contains("payload:"), "got: {line}");
    }

    // -- workflow_status_block tests --

    #[test]
    fn workflow_status_block_empty_returns_empty_string() {
        assert_eq!(workflow_status_block(&[]), "");
    }

    #[test]
    fn workflow_status_block_formats_workflow_and_task_blocks() {
        let mut wf = make_workflow("wf_1", "iterate", "myproj", "running");
        wf.task_blocks = vec![TaskBlockStatus {
            name: "Preflight".to_string(),
            state: "completed".to_string(),
            started_at: String::new(),
            completed_at: String::new(),
            throttled: false,
        }];
        let out = workflow_status_block(&[wf]);
        assert!(out.contains("wf_1"), "got: {out}");
        assert!(out.contains("myproj"), "got: {out}");
        assert!(out.contains("Preflight"), "got: {out}");
    }

    #[test]
    fn workflow_status_block_marks_throttled_task_blocks() {
        let mut wf = make_workflow("wf_1", "iterate", "p", "running");
        wf.task_blocks = vec![TaskBlockStatus {
            name: "B".to_string(),
            state: "pending".to_string(),
            started_at: String::new(),
            completed_at: String::new(),
            throttled: true,
        }];
        let out = workflow_status_block(&[wf]);
        assert!(out.contains("(throttled)"), "got: {out}");
    }

    // -- no_workflows_message tests --

    #[test]
    fn no_workflows_message_with_trace_mentions_span() {
        let msg = no_workflows_message(Some("trace_abc"));
        assert!(msg.contains("span's trace"), "got: {msg}");
    }

    #[test]
    fn no_workflows_message_without_trace_is_generic() {
        let msg = no_workflows_message(None);
        assert_eq!(msg, "No active workflows.");
    }
}
