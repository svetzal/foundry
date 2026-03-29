use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::Result;
use comfy_table::{ContentArrangement, Table};

use foundry_core::trace::{ProcessResult, TraceIndex};

use crate::proto::{
    EmitRequest, StatusRequest, TraceRequest, TraceResponse, WatchRequest,
    foundry_client::FoundryClient,
};

fn parse_throttle(s: &str) -> i32 {
    match s {
        "audit_only" => 1,
        "dry_run" => 2,
        _ => 0,
    }
}

pub async fn emit(
    addr: &str,
    event_type: &str,
    project: &str,
    throttle: &str,
    payload: Option<String>,
    wait: bool,
) -> Result<()> {
    let mut client = FoundryClient::connect(addr.to_string()).await?;

    let request = EmitRequest {
        event_type: event_type.to_string(),
        project: project.to_string(),
        throttle: parse_throttle(throttle),
        payload_json: payload.unwrap_or_default(),
    };

    let response = client.emit(request).await?.into_inner();

    println!("Event emitted: {}", response.event_id);

    if wait {
        println!("Waiting for processing to complete...");
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let trace_req = TraceRequest {
                event_id: response.event_id.clone(),
            };
            let trace_resp = client.trace(trace_req).await?.into_inner();
            if trace_resp.found {
                render_trace(&trace_resp, false);
                let block_sum: u64 =
                    trace_resp.block_executions.iter().map(|b| b.duration_ms).sum();
                println!("---");
                println!("Total: {}ms (blocks: {}ms)", trace_resp.total_duration_ms, block_sum);
                break;
            }
        }
    }

    Ok(())
}

pub async fn status(addr: &str, workflow_id: Option<String>) -> Result<()> {
    let mut client = FoundryClient::connect(addr.to_string()).await?;

    let request = StatusRequest {
        workflow_id: workflow_id.unwrap_or_default(),
    };

    let response = client.status(request).await?.into_inner();

    if response.workflows.is_empty() {
        println!("No active workflows.");
    } else {
        for wf in &response.workflows {
            println!("{} [{}] {} — {}", wf.workflow_id, wf.workflow_type, wf.project, wf.state);
            for tb in &wf.task_blocks {
                let throttled = if tb.throttled { " (throttled)" } else { "" };
                println!("  {} — {}{}", tb.name, tb.state, throttled);
            }
        }
    }

    Ok(())
}

pub async fn watch(addr: &str, project: Option<String>) -> Result<()> {
    let mut client = FoundryClient::connect(addr.to_string()).await?;

    let request = WatchRequest {
        project: project.unwrap_or_default(),
    };

    let mut stream = client.watch(request).await?.into_inner();

    while let Some(event) = stream.message().await? {
        println!("{} {} project={}", event.event_id, event.event_type, event.project);
        if !event.payload_json.is_empty() && event.payload_json != "{}" {
            println!("  payload: {}", event.payload_json);
        }
    }

    Ok(())
}

pub async fn trace(addr: &str, event_id: &str, verbose: bool) -> Result<()> {
    let mut client = FoundryClient::connect(addr.to_string()).await?;

    let request = TraceRequest {
        event_id: event_id.to_string(),
    };

    let response = client.trace(request).await?.into_inner();

    if !response.found {
        println!("No trace found for {event_id} (expired or unknown).");
        return Ok(());
    }

    render_trace(&response, verbose);
    let block_sum: u64 = response.block_executions.iter().map(|b| b.duration_ms).sum();
    println!("---");
    println!("Total: {}ms (blocks: {}ms)", response.total_duration_ms, block_sum);

    Ok(())
}

fn render_trace(response: &TraceResponse, verbose: bool) {
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

    // Start with the root event (first in the list)
    if let Some(root) = response.events.first() {
        print_event_tree(root, &events, &blocks_by_trigger, 0, verbose);
    }
}

fn print_event_tree(
    event: &crate::proto::TraceEvent,
    events: &HashMap<&str, &crate::proto::TraceEvent>,
    blocks_by_trigger: &HashMap<&str, Vec<&crate::proto::TraceBlockExecution>>,
    depth: usize,
    verbose: bool,
) {
    let indent = "  ".repeat(depth);
    println!("{}{} ({}) project={}", indent, event.event_type, event.event_id, event.project);

    if let Some(blocks) = blocks_by_trigger.get(event.event_id.as_str()) {
        for block in blocks {
            let status = if block.success { "ok" } else { "FAILED" };
            println!(
                "{}  \u{2192} {} ({}ms): {} \u{2014} {}",
                indent, block.block_name, block.duration_ms, status, block.summary
            );

            if verbose {
                // Show trigger payload
                if !block.trigger_payload_json.is_empty() && block.trigger_payload_json != "{}" {
                    println!("{indent}    trigger: {}", block.trigger_payload_json);
                }
                // Show emitted payloads
                for (i, payload) in block.emitted_payload_jsons.iter().enumerate() {
                    println!("{indent}    emitted[{i}]: {payload}");
                }
                // Show raw output if non-empty
                if !block.raw_output.is_empty() {
                    println!("{indent}    output:");
                    for line in block.raw_output.lines() {
                        println!("{indent}      {line}");
                    }
                }
                // Show audit artifacts
                if !block.audit_artifacts.is_empty() {
                    println!("{indent}    artifacts:");
                    for path in &block.audit_artifacts {
                        println!("{indent}      {path}");
                    }
                }
            }

            // Recurse into emitted events
            for emitted_id in &block.emitted_event_ids {
                if let Some(emitted_event) = events.get(emitted_id.as_str()) {
                    print_event_tree(emitted_event, events, blocks_by_trigger, depth + 2, verbose);
                }
            }
        }
    }
}

pub async fn run(addr: &str, project: Option<String>, throttle: &str) -> Result<()> {
    let project_name = project.unwrap_or_else(|| "system".to_string());
    let is_system_run = project_name == "system";

    // Subscribe to the watch stream before emitting so we don't miss events.
    // For system-level runs, watch all projects so per-project progress events
    // are visible. For single-project runs, filter to that project only.
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
    let request = EmitRequest {
        event_type: "maintenance_run_started".to_string(),
        project: project_name.clone(),
        throttle: parse_throttle(throttle),
        payload_json: String::new(),
    };

    let response = emit_client.emit(request).await?.into_inner();
    println!("Triggered maintenance run for {project_name}");
    println!("Event: {}", response.event_id);
    println!();

    // Stream progress events until the maintenance run completes.
    while let Some(event) = stream.message().await? {
        let status = extract_status(&event.payload_json);
        println!("[{}] {} {}", event.project, event.event_type, status);

        if is_run_complete(&event.event_type, &event.payload_json, is_system_run) {
            break;
        }
    }

    Ok(())
}

/// Determine whether a watch stream event signals that the run is complete.
///
/// For single-project runs, any `maintenance_run_completed` event is terminal.
///
/// For system-level runs, the fan-out orchestrator emits an early
/// `maintenance_run_completed` (with `project_count` in payload) before
/// per-project chains execute. The true terminal signal is the service-level
/// broadcast (with `root_event_id` in payload) emitted after all processing
/// finishes.
fn is_run_complete(event_type: &str, payload_json: &str, is_system_run: bool) -> bool {
    if event_type != "maintenance_run_completed" {
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

pub async fn iterate(addr: &str, project: &str) -> Result<()> {
    // Subscribe to the watch stream before emitting so we don't miss events.
    let mut watch_client = FoundryClient::connect(addr.to_string()).await?;
    let watch_request = WatchRequest {
        project: project.to_string(),
    };
    let mut stream = watch_client.watch(watch_request).await?.into_inner();

    // Emit IterationRequested
    let mut emit_client = FoundryClient::connect(addr.to_string()).await?;
    let request = EmitRequest {
        event_type: "iteration_requested".to_string(),
        project: project.to_string(),
        throttle: 0, // Full
        payload_json: serde_json::json!({
            "project": project,
            "actions": { "maintain": false },
        })
        .to_string(),
    };

    let response = emit_client.emit(request).await?.into_inner();
    println!("Iterating {project}...");
    println!("Event: {}", response.event_id);
    println!();

    // Stream progress events until the iteration completes
    while let Some(event) = stream.message().await? {
        let status = extract_status(&event.payload_json);
        println!("[{}] {} {}", event.project, event.event_type, status);

        if event.event_type == "project_iteration_completed" {
            break;
        }
    }

    // Show the trace for full visibility
    let mut trace_client = FoundryClient::connect(addr.to_string()).await?;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let trace_resp = trace_client
        .trace(TraceRequest {
            event_id: response.event_id.clone(),
        })
        .await?
        .into_inner();
    if trace_resp.found {
        render_trace(&trace_resp, false);
        println!("---");
    }

    Ok(())
}

pub async fn scout(addr: &str, project: &str) -> Result<()> {
    // Subscribe to the watch stream before emitting so we don't miss events.
    let mut watch_client = FoundryClient::connect(addr.to_string()).await?;
    let watch_request = WatchRequest {
        project: project.to_string(),
    };
    let mut stream = watch_client.watch(watch_request).await?.into_inner();

    // Emit DriftAssessmentRequested
    let mut emit_client = FoundryClient::connect(addr.to_string()).await?;
    let request = EmitRequest {
        event_type: "drift_assessment_requested".to_string(),
        project: project.to_string(),
        throttle: 0, // Full
        payload_json: String::new(),
    };

    let response = emit_client.emit(request).await?.into_inner();
    println!("Scouting {project} for intent drift...");
    println!("Event: {}", response.event_id);
    println!();

    // Stream events until we see DriftAssessmentCompleted
    while let Some(event) = stream.message().await? {
        if event.event_type == "drift_assessment_completed" {
            print_scout_result(project, &event.payload_json);
            break;
        }
    }

    // Show the trace for full visibility
    let mut trace_client = FoundryClient::connect(addr.to_string()).await?;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let trace_resp = trace_client
        .trace(TraceRequest {
            event_id: response.event_id.clone(),
        })
        .await?
        .into_inner();
    if trace_resp.found {
        render_trace(&trace_resp, false);
        println!("---");
    }

    Ok(())
}

/// Print drift scout results in a human-readable format.
fn print_scout_result(project: &str, payload_json: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload_json) else {
        println!("  {project}: could not parse result");
        return;
    };

    let candidate_count = v.get("candidate_count").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let high_value_count =
        v.get("high_value_count").and_then(serde_json::Value::as_u64).unwrap_or(0);

    println!("{project}: {candidate_count} candidates found, {high_value_count} high-value");
    println!();

    if let Some(err) = v.get("parse_error").and_then(serde_json::Value::as_str) {
        println!("  Parse error: {err}");
        return;
    }

    if let Some(candidates) = v.get("candidates").and_then(serde_json::Value::as_array) {
        for candidate in candidates {
            let rank = candidate.get("rank").and_then(serde_json::Value::as_u64).unwrap_or(0);
            let summary = candidate
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("(no summary)");
            let divergence = candidate
                .get("divergence_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let high_value = candidate
                .get("high_value")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let confidence = candidate
                .get("confidence")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let next_step = candidate
                .get("suggested_next_step")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");

            let marker = if high_value { " ***" } else { "" };
            println!("  #{rank} [{divergence}] {summary}{marker}");

            // Impact line
            if let Some(impact) = candidate.get("impact") {
                let severity =
                    impact.get("severity").and_then(serde_json::Value::as_str).unwrap_or("?");
                let frequency =
                    impact.get("frequency").and_then(serde_json::Value::as_str).unwrap_or("?");
                let risk_type =
                    impact.get("risk_type").and_then(serde_json::Value::as_str).unwrap_or("?");
                println!("     severity={severity} frequency={frequency} risk={risk_type}");
            }

            println!("     confidence={confidence} next={next_step}");

            // Explanation
            if let Some(explanation) =
                candidate.get("explanation").and_then(serde_json::Value::as_str)
            {
                println!("     {explanation}");
            }

            println!();
        }
    }
}

/// Resolve the traces directory from env or default.
fn traces_dir() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_TRACES_DIR") {
        PathBuf::from(p)
    } else {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(format!("{home}/.foundry/traces"))
    }
}

/// Read all trace index entries from a single date directory.
fn read_index_from_dir(dir: &Path, project_filter: Option<&str>) -> Vec<TraceIndex> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut indices = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(result) = serde_json::from_str::<ProcessResult>(&content) else {
            continue;
        };
        let event_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        let (event_type, project) = result
            .events
            .first()
            .map(|e| (e.event_type.to_string(), e.project.clone()))
            .unwrap_or_default();
        if let Some(filter) = project_filter {
            if project != filter {
                continue;
            }
        }
        let success = result.block_executions.iter().all(|b| b.success);
        indices.push(TraceIndex {
            event_id,
            event_type,
            project,
            success,
            total_duration_ms: result.total_duration_ms,
        });
    }
    indices
}

fn print_trace_table(date: &str, indices: &[TraceIndex]) {
    println!("{date}");
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["Event ID", "Status", "Duration", "Type", "Project"]);

    for idx in indices {
        let status = if idx.success { "ok" } else { "FAILED" };
        table.add_row(vec![
            &idx.event_id,
            status,
            &format!("{}ms", idx.total_duration_ms),
            &idx.event_type,
            &idx.project,
        ]);
    }

    println!("{table}");
}

// The Result return type is consistent with the other command functions even
// though this function's current body never fails.
#[allow(clippy::unnecessary_wraps)]
pub fn history(date: Option<&str>, project: Option<&str>) -> Result<()> {
    let base_dir = traces_dir();

    if let Some(date_str) = date {
        let dir = base_dir.join(date_str);
        let indices = read_index_from_dir(&dir, project);
        if indices.is_empty() {
            println!("No traces found for {date_str}.");
        } else {
            print_trace_table(date_str, &indices);
        }
    } else {
        // List recent 7 days
        let today = chrono::Utc::now().date_naive();
        let mut found_any = false;
        for offset in 0..7_i64 {
            let day = today - chrono::Duration::days(offset);
            let date_str = day.format("%Y-%m-%d").to_string();
            let dir = base_dir.join(&date_str);
            let indices = read_index_from_dir(&dir, project);
            if !indices.is_empty() {
                print_trace_table(&date_str, &indices);
                found_any = true;
            }
        }
        if !found_any {
            println!("No traces found in the last 7 days.");
        }
    }

    Ok(())
}

pub async fn validate(
    addr: &str,
    projects: Vec<String>,
    all: bool,
    registry_path: &Path,
) -> Result<()> {
    // Resolve which projects to validate
    let project_names = if all {
        let registry = foundry_core::registry::Registry::load(registry_path)?;
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
        // Subscribe to the watch stream before emitting so we don't miss events.
        let mut watch_client = FoundryClient::connect(addr.to_string()).await?;
        let watch_request = WatchRequest {
            project: project_name.clone(),
        };
        let mut stream = watch_client.watch(watch_request).await?.into_inner();

        // Emit ValidationRequested
        let mut emit_client = FoundryClient::connect(addr.to_string()).await?;
        let request = EmitRequest {
            event_type: "validation_requested".to_string(),
            project: project_name.clone(),
            throttle: 0, // Full — the workflow is read-only by design
            payload_json: String::new(),
        };

        let response = emit_client.emit(request).await?.into_inner();
        println!("Validating {project_name}...");

        // Stream events until we see ValidationCompleted for this project
        while let Some(event) = stream.message().await? {
            if event.event_type == "validation_completed" {
                print_validation_result(project_name, &event.payload_json);
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&event.payload_json) {
                    if !v.get("success").and_then(serde_json::Value::as_bool).unwrap_or(false) {
                        any_failed = true;
                    }
                }
                break;
            }
        }

        // Also show the trace for full visibility
        let mut trace_client = FoundryClient::connect(addr.to_string()).await?;
        // Small delay to let trace finalize
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let trace_resp = trace_client
            .trace(TraceRequest {
                event_id: response.event_id.clone(),
            })
            .await?
            .into_inner();
        if trace_resp.found {
            render_trace(&trace_resp, false);
            println!("---");
        }
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

    let success = v.get("success").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let status = if success { "PASS" } else { "FAIL" };
    println!("  {project}: {status}");

    if let Some(results) = v.get("results").and_then(serde_json::Value::as_array) {
        for gate in results {
            let name = gate.get("name").and_then(serde_json::Value::as_str).unwrap_or("unknown");
            let passed = gate.get("passed").and_then(serde_json::Value::as_bool).unwrap_or(false);
            let required =
                gate.get("required").and_then(serde_json::Value::as_bool).unwrap_or(true);
            let marker = if passed { "ok" } else { "FAILED" };
            let req = if required { "required" } else { "optional" };
            print!("    {name}: {marker} ({req})");
            if !passed {
                if let Some(output) = gate.get("output").and_then(serde_json::Value::as_str) {
                    if !output.is_empty() {
                        let snippet: String = output.chars().take(200).collect();
                        print!(" — {snippet}");
                    }
                }
            }
            println!();
        }
    }
}

/// Extract a compact status hint from the event payload JSON.
fn extract_status(payload_json: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    // -- is_run_complete tests --

    #[test]
    fn non_completion_event_is_not_terminal() {
        assert!(!is_run_complete("project_validation_completed", "{}", false));
        assert!(!is_run_complete("project_validation_completed", "{}", true));
        assert!(!is_run_complete("maintenance_run_started", "{}", false));
        assert!(!is_run_complete("maintenance_run_started", "{}", true));
    }

    #[test]
    fn single_project_run_exits_on_any_completion() {
        // Service-level completion (has root_event_id)
        let service_payload = r#"{"success":true,"root_event_id":"evt_abc123"}"#;
        assert!(is_run_complete("maintenance_run_completed", service_payload, false));

        // Fan-out completion (has project_count) — still terminal for single-project
        let fanout_payload = r#"{"project_count":3,"skipped_count":0,"projects":["a","b","c"]}"#;
        assert!(is_run_complete("maintenance_run_completed", fanout_payload, false));

        // Empty payload — still terminal for single-project
        assert!(is_run_complete("maintenance_run_completed", "{}", false));
    }

    #[test]
    fn system_run_ignores_fanout_completion() {
        let fanout_payload = r#"{"project_count":3,"skipped_count":0,"projects":["a","b","c"]}"#;
        assert!(!is_run_complete("maintenance_run_completed", fanout_payload, true));
    }

    #[test]
    fn system_run_exits_on_service_completion() {
        let service_payload = r#"{"success":true,"root_event_id":"evt_abc123"}"#;
        assert!(is_run_complete("maintenance_run_completed", service_payload, true));
    }

    #[test]
    fn system_run_does_not_exit_on_empty_payload() {
        assert!(!is_run_complete("maintenance_run_completed", "{}", true));
        assert!(!is_run_complete("maintenance_run_completed", "", true));
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
}
