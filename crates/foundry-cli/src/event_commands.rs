use std::path::Path;

use anyhow::Result;

use foundry_sdk::trace::{ProcessResult, TraceIndex};

use crate::commands::parse_traceparent_from_env;
use crate::proto::{
    EmitRequest, SpanRequest, StatusRequest, TraceRequest, TraceResponse, WatchRequest,
    WorkflowStatus, foundry_client::FoundryClient,
};
use crate::render;

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
        throttle: crate::commands::parse_throttle(throttle),
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
                print!("{}", render::event::flat_trace(&trace_resp, false));
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
    // Status response.
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
        println!("{}", render::event::no_workflows_message(target_trace_id.as_deref()));
    } else {
        print!("{}", render::event::workflow_status_block(&workflows));
    }

    Ok(())
}

/// Filter workflows by `trace_id`, returning all when `target` is `None`.
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
        print!("{}", render::event::watch_line(&event));
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

    // Legacy fallback: pre-OTel traces have no span_id on any event.
    let legacy = is_legacy_trace_response(&response);

    if flat || legacy {
        if legacy && !flat {
            eprintln!("(legacy trace: rendering flat)");
        }
        print!("{}", render::event::flat_trace(&response, verbose));
    } else {
        let forest = crate::render::trace_tree::build_forest(
            response.events.clone(),
            response.block_executions.clone(),
        );
        let mut out = String::new();
        crate::render::trace_tree::render(&forest, &mut out);
        print!("{out}");
    }

    let block_sum: u64 = response.block_executions.iter().map(|b| b.duration_ms).sum();
    println!("---");
    println!("Total: {}ms (blocks: {}ms)", response.total_duration_ms, block_sum);

    Ok(())
}

/// Returns `true` when no event in the response carries a `span_id`.
fn is_legacy_trace_response(response: &TraceResponse) -> bool {
    !response.events.is_empty() && response.events.iter().all(|e| e.span_id.is_empty())
}

/// Render a flat trace — used by `workflow_commands` after a workflow run.
pub(crate) fn render_trace(response: &TraceResponse, verbose: bool) {
    print!("{}", render::event::flat_trace(response, verbose));
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
        if let Some(filter) = project_filter
            && project != filter
        {
            continue;
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

// The Result return type is consistent with the other command functions even
// though this function's current body never fails.
#[allow(clippy::unnecessary_wraps)]
pub fn history(date: Option<&str>, project: Option<&str>) -> Result<()> {
    let base_dir = foundry_sdk::paths::traces_dir();

    if let Some(date_str) = date {
        let dir = base_dir.join(date_str);
        let indices = read_index_from_dir(&dir, project);
        if indices.is_empty() {
            println!("No traces found for {date_str}.");
        } else {
            print!("{}", render::event::trace_table(date_str, &indices));
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
                print!("{}", render::event::trace_table(&date_str, &indices));
                found_any = true;
            }
        }
        if !found_any {
            println!("No traces found in the last 7 days.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::TraceEvent;

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

    // -- is_legacy_trace_response tests --

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
}
