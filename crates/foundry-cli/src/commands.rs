use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use comfy_table::{ContentArrangement, Table};

use foundry_core::event::PayloadExt;
use foundry_core::trace::{ProcessResult, TraceIndex};

use crate::proto::{
    EmitRequest, SpanRequest, StatusRequest, TraceRequest, TraceResponse, WatchRequest,
    WatchResponse, WorkflowStatus, foundry_client::FoundryClient,
};

fn parse_throttle(s: &str) -> i32 {
    match s {
        "audit_only" => 1,
        "dry_run" => 2,
        _ => 0,
    }
}

/// Parse a W3C traceparent header value into `(trace_id, parent_span_id)`.
///
/// Format: `00-<trace_id 32 hex>-<span_id 16 hex>-<flags 2 hex>`.
/// Returns `(None, None)` for any malformed input (wrong version, wrong number
/// of parts, or wrong field lengths).
fn parse_traceparent(value: &str) -> (Option<String>, Option<String>) {
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 4 || parts[0] != "00" || parts[1].len() != 32 || parts[2].len() != 16 {
        return (None, None);
    }
    (Some(parts[1].to_string()), Some(parts[2].to_string()))
}

/// Read the `TRACEPARENT` environment variable and parse it.
///
/// Thin wrapper around [`parse_traceparent`]. Returns `(None, None)` when the
/// env var is absent or malformed.
fn parse_traceparent_from_env() -> (Option<String>, Option<String>) {
    match std::env::var("TRACEPARENT") {
        Ok(v) => parse_traceparent(&v),
        Err(_) => (None, None),
    }
}

/// Connect, emit, and stream watch events until `is_terminal` returns true.
///
/// Returns the root event ID and all collected [`WatchResponse`] events.
/// Does not print any output — callers are responsible for all display logic.
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
    ///
    /// `payload` is serialised as JSON; pass [`serde_json::Value::Null`] for an
    /// empty payload.
    ///
    /// Returns `(root_event_id, collected_events)`.
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

        let mut events = Vec::new();
        while let Some(event) = stream.message().await? {
            let done = is_terminal(&event.event_type, &event.payload_json);
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
            .trace(TraceRequest {
                event_id: event_id.to_string(),
            })
            .await?
            .into_inner();
        if trace_resp.found {
            render_trace(&trace_resp, false);
            println!("---");
        }
        Ok(())
    }
}

/// Print a slice of watch events in `[project] event_type (status)` format.
fn print_watch_events(events: &[WatchResponse]) {
    for event in events {
        let status = extract_status(&event.payload_json);
        println!("[{}] {} {}", event.project, event.event_type, status);
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

    let (env_trace_id, env_parent_span_id) = parse_traceparent_from_env();
    let request = EmitRequest {
        event_type: event_type.to_string(),
        project: project.to_string(),
        throttle: parse_throttle(throttle),
        payload_json: payload.unwrap_or_default(),
        trace_id: env_trace_id.unwrap_or_default(),
        span_id: String::new(), // daemon mints
        parent_span_id: env_parent_span_id.unwrap_or_default(),
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

pub async fn status(addr: &str, workflow_id: Option<String>, span: Option<String>) -> Result<()> {
    let mut client = FoundryClient::connect(addr.to_string()).await?;

    // If `--span` is set, resolve it to a trace_id first so we can filter the
    // Status response. A span_id is unique within a trace; any event in the
    // span carries the trace_id we want to filter on.
    let target_trace_id: Option<String> = if let Some(span_id) = span.as_ref() {
        let span_response = client
            .span(SpanRequest {
                span_id: span_id.clone(),
            })
            .await?
            .into_inner();
        if !span_response.found {
            println!("No span found with id: {span_id}");
            return Ok(());
        }
        span_response.events.first().map(|e| e.trace_id.clone())
    } else {
        None
    };

    let request = StatusRequest {
        workflow_id: workflow_id.unwrap_or_default(),
    };

    let response = client.status(request).await?.into_inner();

    let workflows: Vec<WorkflowStatus> =
        filter_workflows_by_trace(response.workflows, target_trace_id.as_deref());

    if workflows.is_empty() {
        if target_trace_id.is_some() {
            println!("No active workflows in span's trace.");
        } else {
            println!("No active workflows.");
        }
    } else {
        for wf in &workflows {
            println!("{} [{}] {} — {}", wf.workflow_id, wf.workflow_type, wf.project, wf.state);
            for tb in &wf.task_blocks {
                let throttled = if tb.throttled { " (throttled)" } else { "" };
                println!("  {} — {}{}", tb.name, tb.state, throttled);
            }
        }
    }

    Ok(())
}

/// Filter workflows by `trace_id`, returning all when `target` is `None`.
///
/// Extracted so the (very small) filter logic is independently testable
/// without a live gRPC client.
fn filter_workflows_by_trace(
    workflows: Vec<WorkflowStatus>,
    target: Option<&str>,
) -> Vec<WorkflowStatus> {
    match target {
        Some(trace_id) => workflows.into_iter().filter(|w| w.trace_id == trace_id).collect(),
        None => workflows,
    }
}

pub async fn watch(addr: &str, project: Option<String>) -> Result<()> {
    let mut client = FoundryClient::connect(addr.to_string()).await?;

    let request = WatchRequest {
        project: project.unwrap_or_default(),
    };

    let mut stream = client.watch(request).await?.into_inner();

    while let Some(event) = stream.message().await? {
        let trace_suffix = if event.trace_id.is_empty() {
            String::new()
        } else {
            format!(" trace={}", event.trace_id)
        };
        println!(
            "{} {} project={}{}",
            event.event_id, event.event_type, event.project, trace_suffix
        );
        if !event.payload_json.is_empty() && event.payload_json != "{}" {
            println!("  payload: {}", event.payload_json);
        }
    }

    Ok(())
}

pub async fn trace(addr: &str, event_id: &str, verbose: bool, flat: bool) -> Result<()> {
    let mut client = FoundryClient::connect(addr.to_string()).await?;

    let request = TraceRequest {
        event_id: event_id.to_string(),
    };

    let response = client.trace(request).await?.into_inner();

    if !response.found {
        println!("No trace found for {event_id} (expired or unknown).");
        return Ok(());
    }

    // Legacy fallback: pre-OTel traces have no span_id on any event. Render
    // those flat with a one-line note even when `--flat` is not set, since
    // the tree builder would produce a degenerate (single empty-string-rooted)
    // forest otherwise.
    let legacy = is_legacy_trace_response(&response);

    if flat || legacy {
        if legacy && !flat {
            eprintln!("(legacy trace: rendering flat)");
        }
        render_trace(&response, verbose);
    } else {
        let forest = crate::trace_tree::build_forest(
            response.events.clone(),
            response.block_executions.clone(),
        );
        let mut out = String::new();
        crate::trace_tree::render(&forest, &mut out);
        print!("{out}");
    }

    let block_sum: u64 = response.block_executions.iter().map(|b| b.duration_ms).sum();
    println!("---");
    println!("Total: {}ms (blocks: {}ms)", response.total_duration_ms, block_sum);

    Ok(())
}

/// Returns `true` when no event in the response carries a `span_id`.
///
/// Pre-0.17 daemons did not stamp `span_id` on events, so the tree builder
/// has nothing to root the forest on. Detecting this lets us fall back to
/// the legacy flat renderer gracefully.
fn is_legacy_trace_response(response: &TraceResponse) -> bool {
    !response.events.is_empty() && response.events.iter().all(|e| e.span_id.is_empty())
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

    // Build a lookup: emitted_event_id -> payload_json (index-correlated from block executions).
    let mut event_payloads: HashMap<&str, &str> = HashMap::new();
    for block in &response.block_executions {
        for (i, event_id) in block.emitted_event_ids.iter().enumerate() {
            if let Some(payload) = block.emitted_payload_jsons.get(i) {
                event_payloads.insert(event_id.as_str(), payload.as_str());
            }
        }
    }

    // Start with the root event (first in the list)
    if let Some(root) = response.events.first() {
        if !root.trace_id.is_empty() {
            println!("trace: {}", root.trace_id);
        }
        print_event_tree(root, &events, &blocks_by_trigger, &event_payloads, 0, verbose);
    }
}

fn print_event_tree(
    event: &crate::proto::TraceEvent,
    events: &HashMap<&str, &crate::proto::TraceEvent>,
    blocks_by_trigger: &HashMap<&str, Vec<&crate::proto::TraceBlockExecution>>,
    event_payloads: &HashMap<&str, &str>,
    depth: usize,
    verbose: bool,
) {
    let indent = "  ".repeat(depth);

    // Special rendering for skill-install results: compact inline format.
    if event.event_type == "local_skill_install_completed" {
        let payload = event_payloads.get(event.event_id.as_str()).copied().unwrap_or("{}");
        print_skill_install_event(payload, &indent);
        return;
    }

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
                    print_event_tree(
                        emitted_event,
                        events,
                        blocks_by_trigger,
                        event_payloads,
                        depth + 2,
                        verbose,
                    );
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
    //
    // System-wide runs open a `MaintenanceCycle` (the daemon fans out to each
    // registered project). Single-project runs open a `ProjectRun` directly.
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
/// For single-project runs, any `project_run_completed` event is terminal.
///
/// For system-level runs, the engine's gather synthesizes
/// `maintenance_cycle_completed` once all per-project runs finish, but the
/// summary is rendered in a second phase after that. The true terminal signal
/// is `maintenance_summary_requested` (carrying `root_event_id`), which the
/// service broadcasts only after the summary has been written.
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

pub async fn iterate(addr: &str, project: &str) -> Result<()> {
    let runner = WorkflowRunner::new(addr, project);
    let (event_id, events) = runner
        .run_workflow(
            "project_iteration_requested",
            serde_json::json!({
                "project": project,
                "actions": { "maintain": false },
            }),
            |t, _| t == "project_iteration_completed",
        )
        .await?;

    println!("Iterating {project}...");
    println!("Event: {event_id}");
    println!();
    print_watch_events(&events);

    runner.show_trace(&event_id).await?;
    Ok(())
}

pub async fn release(addr: &str, project: &str, bump: Option<String>) -> Result<()> {
    let runner = WorkflowRunner::new(addr, project);
    let payload = match &bump {
        Some(b) => serde_json::json!({ "bump": b }),
        None => serde_json::json!({}),
    };
    let (event_id, events) = runner
        .run_workflow("release_requested", payload, |t, p| {
            t == "local_install_completed"
                || (t == "release_completed"
                    && serde_json::from_str::<serde_json::Value>(p)
                        .is_ok_and(|v| !v.bool_or("success", true)))
        })
        .await?;

    println!("Releasing {project}...");
    println!("Event: {event_id}");
    println!();
    print_watch_events(&events);

    runner.show_trace(&event_id).await?;
    Ok(())
}

pub async fn scout(addr: &str, project: &str) -> Result<()> {
    let runner = WorkflowRunner::new(addr, project);
    let (event_id, events) = runner
        .run_workflow("drift_assessment_requested", serde_json::Value::Null, |t, _| {
            t == "drift_assessment_completed"
        })
        .await?;

    println!("Scouting {project} for intent drift...");
    println!("Event: {event_id}");
    println!();

    if let Some(terminal) = events.iter().find(|e| e.event_type == "drift_assessment_completed") {
        print_scout_result(project, &terminal.payload_json);
    }

    runner.show_trace(&event_id).await?;
    Ok(())
}

pub async fn pipeline(addr: &str, project: &str) -> Result<()> {
    let runner = WorkflowRunner::new(addr, project);
    let (event_id, events) = runner
        .run_workflow("pipeline_check_requested", serde_json::Value::Null, |t, p| {
            (t == "pipeline_checked"
                && serde_json::from_str::<serde_json::Value>(p)
                    .is_ok_and(|v| v.bool_or("passing", false)))
                || t == "remediation_completed"
        })
        .await?;

    println!("Checking pipeline for {project}...");
    println!("Event: {event_id}");
    println!();
    print_watch_events(&events);

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

            // Impact line
            if let Some(impact) = candidate.get("impact") {
                let severity = impact.str_or("severity", "?");
                let frequency = impact.str_or("frequency", "?");
                let risk_type = impact.str_or("risk_type", "?");
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
        let success = result.is_success();
        let trace_id = result.events.first().and_then(|e| e.trace_id.clone());
        indices.push(TraceIndex {
            event_id,
            event_type,
            project,
            success,
            total_duration_ms: result.total_duration_ms,
            trace_id,
        });
    }
    indices
}

fn print_trace_table(date: &str, indices: &[TraceIndex]) {
    println!("{date}");
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

    println!("{table}");
}

// The Result return type is consistent with the other command functions even
// though this function's current body never fails.
#[allow(clippy::unnecessary_wraps)]
pub fn history(date: Option<&str>, project: Option<&str>) -> Result<()> {
    let base_dir = foundry_core::paths::traces_dir();

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
        println!("Validating {project_name}...");
        let runner = WorkflowRunner::new(addr, project_name);
        let (event_id, events) = runner
            .run_workflow("validation_requested", serde_json::Value::Null, |t, _| {
                t == "validation_completed"
            })
            .await?;

        if let Some(terminal) = events.iter().find(|e| e.event_type == "validation_completed") {
            print_validation_result(project_name, &terminal.payload_json);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&terminal.payload_json) {
                if !v.bool_or("success", false) {
                    any_failed = true;
                }
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

/// Render a `local_skill_install_completed` event in compact inline format.
///
/// Success:  `  → Install Skill: ok — <command>`
/// Failure:  `  → Install Skill: warn — command failed: <stderr_tail>`
fn print_skill_install_event(payload_json: &str, indent: &str) {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload_json) {
        let success = v.bool_or("success", false);
        let command = v.str_or("command", "(unknown command)");
        if success {
            println!("{indent}  \u{2192} Install Skill: ok \u{2014} {command}");
        } else {
            let stderr = v.str_or("stderr_tail", "(no output)");
            let detail = if stderr.is_empty() {
                "(no output)"
            } else {
                stderr
            };
            println!("{indent}  \u{2192} Install Skill: warn \u{2014} command failed: {detail}");
        }
    } else {
        println!("{indent}  \u{2192} Install Skill: (unparseable payload)");
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
    use crate::proto::TraceEvent;

    // -- is_legacy_trace_response tests --

    fn test_event_with_span_id(span_id: &str) -> TraceEvent {
        TraceEvent {
            event_id: "evt_1".to_string(),
            event_type: "x".to_string(),
            project: "p".to_string(),
            occurred_at: String::new(),
            throttle: 0,
            trace_id: String::new(),
            span_id: span_id.to_string(),
            parent_span_id: String::new(),
        }
    }

    #[test]
    fn detects_legacy_trace_when_all_events_lack_span_id() {
        let response = TraceResponse {
            found: true,
            events: vec![test_event_with_span_id(""), test_event_with_span_id("")],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        assert!(is_legacy_trace_response(&response));
    }

    #[test]
    fn detects_modern_trace_when_some_events_have_span_id() {
        let response = TraceResponse {
            found: true,
            events: vec![test_event_with_span_id("abc"), test_event_with_span_id("")],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        assert!(!is_legacy_trace_response(&response));
    }

    #[test]
    fn detects_modern_trace_when_all_events_have_span_id() {
        let response = TraceResponse {
            found: true,
            events: vec![
                test_event_with_span_id("abc"),
                test_event_with_span_id("def"),
            ],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        assert!(!is_legacy_trace_response(&response));
    }

    #[test]
    fn empty_response_is_not_legacy() {
        // An empty response shouldn't be treated as legacy — there's nothing to render
        // flat, and `all_legacy` keys off "we have events but none have span_id".
        let response = TraceResponse {
            found: true,
            events: vec![],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        assert!(!is_legacy_trace_response(&response));
    }

    // -- filter_workflows_by_trace tests --

    fn test_workflow(workflow_id: &str, trace_id: &str) -> WorkflowStatus {
        WorkflowStatus {
            workflow_id: workflow_id.to_string(),
            workflow_type: "x".to_string(),
            project: "p".to_string(),
            state: "running".to_string(),
            started_at: String::new(),
            completed_at: String::new(),
            task_blocks: vec![],
            trace_id: trace_id.to_string(),
        }
    }

    #[test]
    fn filter_workflows_by_trace_returns_all_when_target_is_none() {
        let wfs = vec![test_workflow("a", "t1"), test_workflow("b", "t2")];
        let out = filter_workflows_by_trace(wfs, None);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn filter_workflows_by_trace_keeps_only_matching_trace() {
        let wfs = vec![
            test_workflow("a", "t1"),
            test_workflow("b", "t2"),
            test_workflow("c", "t1"),
        ];
        let out = filter_workflows_by_trace(wfs, Some("t1"));
        let ids: Vec<_> = out.iter().map(|w| w.workflow_id.clone()).collect();
        assert_eq!(ids, vec!["a", "c"]);
    }

    #[test]
    fn filter_workflows_by_trace_returns_empty_when_no_match() {
        let wfs = vec![test_workflow("a", "t1")];
        let out = filter_workflows_by_trace(wfs, Some("nope"));
        assert!(out.is_empty());
    }

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
        // A system-level `maintenance_summary_requested` must not terminate a
        // single-project run — single-project runs key off `project_run_completed`.
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
        // The engine's gather emits `maintenance_cycle_completed` before the
        // summary phase runs — it must not terminate the CLI's wait.
        let gather_payload = r#"{"gather_id":"gth_x","expected":3,"arrived":3}"#;
        assert!(!is_run_complete("maintenance_cycle_completed", gather_payload, true));
    }

    #[test]
    fn system_run_ignores_project_run_completion() {
        // Per-project `project_run_completed` events fly during a system run,
        // but they must not terminate the cycle.
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

    // -- parse_traceparent tests --

    #[test]
    fn parse_traceparent_well_formed() {
        let (t, p) = parse_traceparent("00-0123456789abcdef0123456789abcdef-fedcba9876543210-01");
        assert_eq!(t.as_deref(), Some("0123456789abcdef0123456789abcdef"));
        assert_eq!(p.as_deref(), Some("fedcba9876543210"));
    }

    #[test]
    fn parse_traceparent_well_formed_unsampled_flags() {
        // Flags field content is not inspected; only its presence and the
        // overall four-part structure matters.
        let (t, p) = parse_traceparent("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-00");
        assert_eq!(t.as_deref(), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert_eq!(p.as_deref(), Some("bbbbbbbbbbbbbbbb"));
    }

    #[test]
    fn parse_traceparent_empty_returns_none() {
        assert_eq!(parse_traceparent(""), (None, None));
    }

    #[test]
    fn parse_traceparent_unrecognised_string() {
        assert_eq!(parse_traceparent("malformed"), (None, None));
    }

    #[test]
    fn parse_traceparent_wrong_part_count() {
        // Only three parts.
        assert_eq!(
            parse_traceparent("00-0123456789abcdef0123456789abcdef-fedcba9876543210"),
            (None, None)
        );
        // Five parts.
        assert_eq!(
            parse_traceparent("00-0123456789abcdef0123456789abcdef-fedcba9876543210-01-extra"),
            (None, None)
        );
    }

    #[test]
    fn parse_traceparent_wrong_version() {
        // Version `ff` is reserved; we only accept `00`.
        assert_eq!(
            parse_traceparent("ff-0123456789abcdef0123456789abcdef-fedcba9876543210-01"),
            (None, None)
        );
    }

    #[test]
    fn parse_traceparent_wrong_trace_id_length() {
        // 31 hex chars instead of 32.
        assert_eq!(
            parse_traceparent("00-0123456789abcdef0123456789abcde-fedcba9876543210-01"),
            (None, None)
        );
    }

    #[test]
    fn parse_traceparent_wrong_span_id_length() {
        // 15 hex chars instead of 16.
        assert_eq!(
            parse_traceparent("00-0123456789abcdef0123456789abcdef-fedcba987654321-01"),
            (None, None)
        );
    }

    #[test]
    fn parse_traceparent_short_fields() {
        assert_eq!(parse_traceparent("00-tooshort-also-01"), (None, None));
    }
}
