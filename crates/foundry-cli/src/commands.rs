use anyhow::Result;

use std::collections::HashMap;

use crate::proto::{
    EmitRequest, StatusRequest, TraceRequest, TraceResponse, foundry_client::FoundryClient,
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
    if !response.workflow_id.is_empty() {
        println!("Workflow started: {}", response.workflow_id);
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

pub async fn watch(addr: &str, workflow_id: Option<String>) -> Result<()> {
    let mut client = FoundryClient::connect(addr.to_string()).await?;

    let request = StatusRequest {
        workflow_id: workflow_id.unwrap_or_default(),
    };

    let mut stream = client.watch(request).await?.into_inner();

    while let Some(status) = stream.message().await? {
        println!(
            "{} [{}] {} — {}",
            status.workflow_id, status.workflow_type, status.project, status.state
        );
        for tb in &status.task_blocks {
            let throttled = if tb.throttled { " (throttled)" } else { "" };
            println!("  {} — {}{}", tb.name, tb.state, throttled);
        }
        println!();
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
    println!("Total: {}ms", response.total_duration_ms);

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
    let mut client = FoundryClient::connect(addr.to_string()).await?;

    let project_name = project.unwrap_or_else(|| "system".to_string());

    let request = EmitRequest {
        event_type: "maintenance_run_started".to_string(),
        project: project_name.clone(),
        throttle: parse_throttle(throttle),
        payload_json: String::new(),
    };

    let response = client.emit(request).await?.into_inner();

    println!("Triggered maintenance run for {project_name}");
    println!("Event: {}", response.event_id);

    Ok(())
}
