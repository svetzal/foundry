use anyhow::Result;

use std::collections::HashMap;

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
                render_trace(&trace_resp);
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

pub async fn trace(addr: &str, event_id: &str) -> Result<()> {
    let mut client = FoundryClient::connect(addr.to_string()).await?;

    let request = TraceRequest {
        event_id: event_id.to_string(),
    };

    let response = client.trace(request).await?.into_inner();

    if !response.found {
        println!("No trace found for {event_id} (expired or unknown).");
        return Ok(());
    }

    render_trace(&response);
    let block_sum: u64 = response.block_executions.iter().map(|b| b.duration_ms).sum();
    println!("---");
    println!("Total: {}ms (blocks: {}ms)", response.total_duration_ms, block_sum);

    Ok(())
}

fn render_trace(response: &TraceResponse) {
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
        print_event_tree(root, &events, &blocks_by_trigger, 0);
    }
}

fn print_event_tree(
    event: &crate::proto::TraceEvent,
    events: &HashMap<&str, &crate::proto::TraceEvent>,
    blocks_by_trigger: &HashMap<&str, Vec<&crate::proto::TraceBlockExecution>>,
    depth: usize,
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

            // Recurse into emitted events
            for emitted_id in &block.emitted_event_ids {
                if let Some(emitted_event) = events.get(emitted_id.as_str()) {
                    print_event_tree(emitted_event, events, blocks_by_trigger, depth + 2);
                }
            }
        }
    }
}

pub async fn run(addr: &str, project: Option<String>, throttle: &str) -> Result<()> {
    let project_name = project.unwrap_or_else(|| "system".to_string());

    // Subscribe to the watch stream before emitting so we don't miss events.
    let mut watch_client = FoundryClient::connect(addr.to_string()).await?;
    let watch_request = WatchRequest {
        project: project_name.clone(),
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

    // Stream progress events until the stream ends.
    while let Some(event) = stream.message().await? {
        let status = extract_status(&event.payload_json);
        println!("[{}] {} {}", event.project, event.event_type, status);
    }

    Ok(())
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
