use std::path::Path;

use anyhow::Result;

use foundry_sdk::event::PayloadExt;

use crate::commands::parse_throttle;
use crate::proto::{EmitRequest, WatchRequest, WatchResponse, foundry_client::FoundryClient};

/// Connect, emit, and stream watch events until `is_terminal` returns true.
struct WorkflowRunner {
    addr: String,
    project: String,
}

impl WorkflowRunner {
    fn new(addr: &str, project: &str) -> Self {
        Self {
            addr: addr.to_string(),
            project: project.to_string(),
        }
    }

    /// Subscribe to the watch stream, emit `event_type` with `payload`, then
    /// stream events until `is_terminal` returns `true`.
    async fn run_workflow(
        &self,
        event_type: &str,
        payload: serde_json::Value,
        is_terminal: impl Fn(&str, &str) -> bool,
    ) -> Result<(String, Vec<WatchResponse>)> {
        // Subscribe before emitting so we don't miss events.
        let mut watch_client = FoundryClient::connect(self.addr.clone()).await?;
        let mut stream = watch_client
            .watch(WatchRequest {
                project: self.project.clone(),
            })
            .await?
            .into_inner();

        let mut emit_client = FoundryClient::connect(self.addr.clone()).await?;
        let payload_json = if payload.is_null() {
            String::new()
        } else {
            payload.to_string()
        };
        let response = emit_client
            .emit(EmitRequest {
                event_type: event_type.to_string(),
                project: self.project.clone(),
                throttle: 0, // Full
                payload_json,
                trace_id: String::new(),
                span_id: String::new(),
                parent_span_id: String::new(),
            })
            .await?
            .into_inner();
        println!("Event: {}", response.event_id);
        println!();

        let mut events = Vec::new();
        while let Some(event) = stream.message().await? {
            let done = is_terminal(&event.event_type, &event.payload_json);
            print_watch_event(&event);
            events.push(event);
            if done {
                break;
            }
        }

        Ok((response.event_id, events))
    }

    /// Fetch and render the trace for `event_id` after a 1-second delay.
    async fn show_trace(&self, event_id: &str) -> Result<()> {
        let mut trace_client = FoundryClient::connect(self.addr.clone()).await?;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let trace_resp = trace_client
            .trace(crate::proto::TraceRequest {
                event_id: event_id.to_string(),
            })
            .await?
            .into_inner();
        if trace_resp.found {
            crate::event_commands::render_trace(&trace_resp, false);
            println!("---");
        }
        Ok(())
    }
}

fn print_watch_event(event: &WatchResponse) {
    match event.event_type.as_str() {
        "block_started" => print_block_started(event),
        "block_completed" => print_block_completed(event),
        _ => {
            let status = extract_status(&event.payload_json);
            println!("[{}] {} {}", event.project, event.event_type, status);
        }
    }
}

fn print_block_started(event: &WatchResponse) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&event.payload_json) else {
        println!("[{}] block started", event.project);
        return;
    };
    let block = v.str_or("block", "unknown block");
    let trigger = v.str_or("trigger_event_type", "unknown trigger");
    println!("[{}] running block {block} (from {trigger})", event.project);
}

fn print_block_completed(event: &WatchResponse) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&event.payload_json) else {
        println!("[{}] block completed", event.project);
        return;
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
    println!("[{}] finished block {block} {suffix}", event.project);
}

fn format_duration(duration_ms: u64) -> String {
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

pub async fn run(addr: &str, project: Option<String>, throttle: &str) -> Result<()> {
    let project_name = project.unwrap_or_else(|| "system".to_string());
    let is_system_run = project_name == "system";

    // Subscribe to the watch stream before emitting so we don't miss events.
    let mut watch_client = FoundryClient::connect(addr.to_string()).await?;
    let watch_request = WatchRequest {
        project: if is_system_run {
            String::new()
        } else {
            project_name.clone()
        },
    };
    let mut stream = watch_client.watch(watch_request).await?.into_inner();

    // Now emit the maintenance run event using a separate connection.
    let mut emit_client = FoundryClient::connect(addr.to_string()).await?;
    let opener_event_type = if is_system_run {
        "maintenance_cycle_started"
    } else {
        "project_run_started"
    };
    let request = EmitRequest {
        event_type: opener_event_type.to_string(),
        project: project_name.clone(),
        throttle: parse_throttle(throttle),
        payload_json: String::new(),
        trace_id: String::new(),
        span_id: String::new(),
        parent_span_id: String::new(),
    };

    let response = emit_client.emit(request).await?.into_inner();
    println!("Triggered maintenance run for {project_name}");
    println!("Event: {}", response.event_id);
    println!();

    // Stream progress events until the maintenance run completes.
    while let Some(event) = stream.message().await? {
        print_watch_event(&event);

        if is_run_complete(&event.event_type, &event.payload_json, is_system_run) {
            break;
        }
    }

    Ok(())
}

/// Determine whether a watch stream event signals that the run is complete.
fn is_run_complete(event_type: &str, payload_json: &str, is_system_run: bool) -> bool {
    let expected = if is_system_run {
        "maintenance_summary_requested"
    } else {
        "project_run_completed"
    };
    if event_type != expected {
        return false;
    }
    if !is_system_run {
        return true;
    }
    // System run: only exit on the service-level completion (has root_event_id).
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload_json) {
        v.get("root_event_id").is_some()
    } else {
        false
    }
}

/// Validate an optional `--agent` provider override and return its canonical wire form.
fn resolve_agent_override(agent: Option<&str>) -> Result<Option<String>> {
    match agent {
        None => Ok(None),
        Some(s) => s
            .parse::<foundry_sdk::gateway::AgentProvider>()
            .map(|p| Some(p.to_string()))
            .map_err(|e| anyhow::anyhow!("{e} (valid: claude, opencode, codex)")),
    }
}

pub async fn iterate(addr: &str, project: &str, agent: Option<&str>) -> Result<()> {
    let agent_provider = resolve_agent_override(agent)?;
    let mut payload = serde_json::json!({
        "project": project,
        "actions": { "maintain": false },
    });
    if let Some(p) = &agent_provider {
        payload["agent_provider"] = serde_json::json!(p);
    }
    let runner = WorkflowRunner::new(addr, project);
    println!("Iterating {project}...");
    let (event_id, _events) = runner
        .run_workflow("project_iteration_requested", payload, |t, _| {
            t == "project_iteration_completed"
        })
        .await?;

    runner.show_trace(&event_id).await?;
    Ok(())
}

pub async fn task(addr: &str, project: &str, description: &str, agent: Option<&str>) -> Result<()> {
    let agent_provider = resolve_agent_override(agent)?;
    let mut payload = serde_json::json!({
        "project": project,
        "workflow": "task",
        "prompt": description,
    });
    if let Some(p) = &agent_provider {
        payload["agent_provider"] = serde_json::json!(p);
    }
    let runner = WorkflowRunner::new(addr, project);
    println!("Running task for {project}...");
    let (event_id, _events) = runner
        .run_workflow("execution_requested", payload, |t, p| {
            t == "project_iteration_completed"
                && serde_json::from_str::<serde_json::Value>(p)
                    .is_ok_and(|v| v.str_or("workflow", "") == "task")
        })
        .await?;

    runner.show_trace(&event_id).await?;
    Ok(())
}

pub async fn release(addr: &str, project: &str, bump: Option<String>) -> Result<()> {
    let runner = WorkflowRunner::new(addr, project);
    let payload = match &bump {
        Some(b) => serde_json::json!({ "bump": b }),
        None => serde_json::json!({}),
    };
    println!("Releasing {project}...");
    let (event_id, _events) = runner
        .run_workflow("release_requested", payload, |t, p| {
            t == "local_install_completed"
                || (t == "release_completed"
                    && serde_json::from_str::<serde_json::Value>(p)
                        .is_ok_and(|v| !v.bool_or("success", true)))
        })
        .await?;

    runner.show_trace(&event_id).await?;
    Ok(())
}

pub async fn scout(addr: &str, project: &str, agent: Option<&str>) -> Result<()> {
    let agent_provider = resolve_agent_override(agent)?;
    let payload = match &agent_provider {
        Some(p) => serde_json::json!({ "agent_provider": p }),
        None => serde_json::Value::Null,
    };
    let runner = WorkflowRunner::new(addr, project);
    println!("Scouting {project} for intent drift...");
    let (event_id, events) = runner
        .run_workflow("drift_assessment_requested", payload, |t, _| {
            t == "drift_assessment_completed"
        })
        .await?;

    if let Some(terminal) = events.iter().find(|e| e.event_type == "drift_assessment_completed") {
        print_scout_result(project, &terminal.payload_json);
    }

    runner.show_trace(&event_id).await?;
    Ok(())
}

pub async fn pipeline(addr: &str, project: &str, agent: Option<&str>) -> Result<()> {
    let agent_provider = resolve_agent_override(agent)?;
    let payload = match &agent_provider {
        Some(p) => serde_json::json!({ "agent_provider": p }),
        None => serde_json::Value::Null,
    };
    let runner = WorkflowRunner::new(addr, project);
    println!("Checking pipeline for {project}...");
    let (event_id, _events) = runner
        .run_workflow("pipeline_check_requested", payload, |t, p| {
            (t == "pipeline_checked"
                && serde_json::from_str::<serde_json::Value>(p)
                    .is_ok_and(|v| v.bool_or("passing", false)))
                || t == "remediation_completed"
        })
        .await?;

    runner.show_trace(&event_id).await?;
    Ok(())
}

/// Print drift scout results in a human-readable format.
fn print_scout_result(project: &str, payload_json: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload_json) else {
        println!("  {project}: could not parse result");
        return;
    };

    let candidate_count = v.u64_or("candidate_count", 0);
    let high_value_count = v.u64_or("high_value_count", 0);

    println!("{project}: {candidate_count} candidates found, {high_value_count} high-value");
    println!();

    if let Some(err) = v.get("parse_error").and_then(serde_json::Value::as_str) {
        println!("  Parse error: {err}");
        return;
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
            println!("  #{rank} [{divergence}] {summary}{marker}");

            if let Some(impact) = candidate.get("impact") {
                let severity = impact.str_or("severity", "?");
                let frequency = impact.str_or("frequency", "?");
                let risk_type = impact.str_or("risk_type", "?");
                println!("     severity={severity} frequency={frequency} risk={risk_type}");
            }

            println!("     confidence={confidence} next={next_step}");

            if let Some(explanation) =
                candidate.get("explanation").and_then(serde_json::Value::as_str)
            {
                println!("     {explanation}");
            }

            println!();
        }
    }
}

pub async fn validate(
    addr: &str,
    projects: Vec<String>,
    all: bool,
    registry_path: &Path,
) -> Result<()> {
    let project_names = if all {
        let registry = foundry_sdk::registry::Registry::load(registry_path)?;
        registry.active_projects().iter().map(|p| p.name.clone()).collect::<Vec<_>>()
    } else if projects.is_empty() {
        anyhow::bail!("specify one or more project names, or use --all");
    } else {
        projects
    };

    if project_names.is_empty() {
        println!("No active projects in registry.");
        return Ok(());
    }

    let mut any_failed = false;

    for project_name in &project_names {
        println!("Validating {project_name}...");
        let runner = WorkflowRunner::new(addr, project_name);
        let (event_id, events) = runner
            .run_workflow("validation_requested", serde_json::Value::Null, |t, _| {
                t == "validation_completed"
            })
            .await?;

        if let Some(terminal) = events.iter().find(|e| e.event_type == "validation_completed") {
            print_validation_result(project_name, &terminal.payload_json);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&terminal.payload_json)
                && !v.bool_or("success", false)
            {
                any_failed = true;
            }
        }

        runner.show_trace(&event_id).await?;
        println!();
    }

    if any_failed {
        std::process::exit(1);
    }

    Ok(())
}

/// Print per-gate pass/fail results for a validation.
fn print_validation_result(project: &str, payload_json: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload_json) else {
        println!("  {project}: could not parse result");
        return;
    };

    let success = v.bool_or("success", false);
    let status = if success { "PASS" } else { "FAIL" };
    println!("  {project}: {status}");

    if let Some(results) = v.get("results").and_then(serde_json::Value::as_array) {
        for gate in results {
            let name = gate.str_or("name", "unknown");
            let passed = gate.bool_or("passed", false);
            let required = gate.bool_or("required", true);
            let marker = if passed { "ok" } else { "FAILED" };
            let req = if required { "required" } else { "optional" };
            print!("    {name}: {marker} ({req})");
            if !passed
                && let Some(output) = gate.get("output").and_then(serde_json::Value::as_str)
                && !output.is_empty()
            {
                let snippet: String = output.chars().take(200).collect();
                print!(" — {snippet}");
            }
            println!();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- is_run_complete tests --

    #[test]
    fn non_completion_event_is_not_terminal() {
        assert!(!is_run_complete("project_validation_completed", "{}", false));
        assert!(!is_run_complete("project_validation_completed", "{}", true));
        assert!(!is_run_complete("project_run_started", "{}", false));
        assert!(!is_run_complete("maintenance_cycle_started", "{}", true));
    }

    #[test]
    fn single_project_run_does_not_exit_on_cycle_completion() {
        let service_payload = r#"{"success":true,"root_event_id":"evt_abc123"}"#;
        assert!(!is_run_complete("maintenance_summary_requested", service_payload, false));
    }

    #[test]
    fn single_project_run_exits_on_project_run_completion() {
        let service_payload = r#"{"success":true,"root_event_id":"evt_abc123"}"#;
        assert!(is_run_complete("project_run_completed", service_payload, false));

        // Empty payload — still terminal for single-project
        assert!(is_run_complete("project_run_completed", "{}", false));
    }

    #[test]
    fn system_run_ignores_gather_cycle_completion() {
        let gather_payload = r#"{"gather_id":"gth_x","expected":3,"arrived":3}"#;
        assert!(!is_run_complete("maintenance_cycle_completed", gather_payload, true));
    }

    #[test]
    fn system_run_ignores_project_run_completion() {
        let service_payload = r#"{"success":true,"root_event_id":"evt_abc123"}"#;
        assert!(!is_run_complete("project_run_completed", service_payload, true));
    }

    #[test]
    fn system_run_exits_on_summary_request() {
        let service_payload = r#"{"root_event_id":"evt_abc123","total_duration_ms":1000}"#;
        assert!(is_run_complete("maintenance_summary_requested", service_payload, true));
    }

    #[test]
    fn system_run_does_not_exit_on_empty_payload() {
        assert!(!is_run_complete("maintenance_summary_requested", "{}", true));
        assert!(!is_run_complete("maintenance_summary_requested", "", true));
    }

    // -- extract_status tests --

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

    #[test]
    fn format_duration_uses_ms_below_one_second_and_tenths_after() {
        assert_eq!(format_duration(999), "999ms");
        assert_eq!(format_duration(1_000), "1.0s");
        assert_eq!(format_duration(143_967), "143.9s");
    }
}
