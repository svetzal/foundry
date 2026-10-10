use std::path::Path;

use anyhow::Result;

use foundry_sdk::event::PayloadExt;

use crate::commands::parse_throttle;
use crate::proto::{EmitRequest, WatchRequest, WatchResponse, foundry_client::FoundryClient};
use crate::render;

/// Connect, emit, and stream watch events until `is_terminal` returns true.
///
/// Only events on the emitted root's own trace are printed and offered to
/// `is_terminal`: the stream carries every workflow on the project, and a
/// paced task may wait while another one on the same project finishes.
pub(crate) struct WorkflowRunner {
    addr: String,
    project: String,
}

/// What a watched run produced: the root's id, every event on its trace, and
/// the root the pacing scheduler started it from, when it was paced.
pub(crate) struct WatchedRun {
    pub(crate) event_id: String,
    pub(crate) events: Vec<WatchResponse>,
}

impl WatchedRun {
    /// The event whose trace `foundry trace` should show: the started root
    /// the scheduler emitted for a paced task, else the root this client
    /// emitted.
    pub(crate) fn trace_root_id(&self) -> &str {
        self.events
            .iter()
            .rev()
            .find(|event| {
                event.event_type == "execution_requested"
                    && event.event_id != self.event_id
                    && serde_json::from_str::<serde_json::Value>(&event.payload_json)
                        .is_ok_and(|v| v.get("admitted_work_item_id").is_some())
            })
            .map_or(self.event_id.as_str(), |event| event.event_id.as_str())
    }
}

impl WorkflowRunner {
    pub(crate) fn new(addr: &str, project: &str) -> Self {
        Self {
            addr: addr.to_string(),
            project: project.to_string(),
        }
    }

    /// Subscribe to the watch stream, emit `event_type` with `payload`, then
    /// stream events until `is_terminal` returns `true`.
    pub(crate) async fn run_workflow(
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
                source: Some(crate::origin::operator_source()),
            })
            .await?
            .into_inner();
        println!("Event: {}", response.event_id);
        println!();

        let mut events = Vec::new();
        let mut own_trace: Option<String> = None;
        while let Some(event) = stream.message().await? {
            if event.event_id == response.event_id {
                own_trace = Some(event.trace_id.clone());
            }
            if !on_own_trace(own_trace.as_deref(), &event) {
                continue;
            }
            let done = is_terminal(&event.event_type, &event.payload_json);
            print!("{}", render::workflow::watch_event_line(&event));
            events.push(event);
            if done {
                break;
            }
        }

        Ok((response.event_id, events))
    }

    /// Fetch and render the trace for `event_id` after a 1-second delay.
    pub(crate) async fn show_trace(&self, event_id: &str) -> Result<()> {
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
        source: Some(crate::origin::operator_source()),
    };

    let response = emit_client.emit(request).await?.into_inner();
    println!("Triggered maintenance run for {project_name}");
    println!("Event: {}", response.event_id);
    println!();

    // Stream progress events until the maintenance run completes.
    while let Some(event) = stream.message().await? {
        print!("{}", render::workflow::watch_event_line(&event));

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

/// The `execution_requested` payload for one `foundry task` dispatch.
///
/// `operator_origin`, `depends_on` and `not_before` are written only when
/// there is something to carry, so a dispatch without them is byte-for-byte
/// the payload this command has always sent.
fn task_payload(
    project: &str,
    description: &str,
    agent_provider: Option<&str>,
    operator_origin: Option<&str>,
    schedule: &TaskSchedule<'_>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "project": project,
        "workflow": "task",
        "prompt": description,
    });
    if let Some(provider) = agent_provider {
        payload["agent_provider"] = serde_json::json!(provider);
    }
    if let Some(origin) = operator_origin {
        payload["operator_origin"] = serde_json::json!(origin);
    }
    if !schedule.after.is_empty() {
        payload["depends_on"] = serde_json::json!(schedule.after);
    }
    if let Some(at) = schedule.not_before {
        payload["not_before"] = serde_json::json!(at.to_rfc3339());
    }
    payload
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

/// Whether `event` belongs to the run this client emitted.
///
/// Until the root's own trace is known (its root has not been seen yet) every
/// event passes, exactly as before pacing; after that only the trace's events
/// do. A paced task's started root inherits the admitted root's trace, so the
/// whole chain still shows.
fn on_own_trace(own_trace: Option<&str>, event: &WatchResponse) -> bool {
    own_trace.is_none_or(|trace| trace.is_empty() || event.trace_id == trace)
}

#[allow(clippy::too_many_arguments)]
pub async fn task(
    addr: &str,
    project: &str,
    description: &str,
    agent: Option<&str>,
    origin: Option<&str>,
    after: &[String],
    not_before: Option<&str>,
) -> Result<()> {
    let agent_provider = resolve_agent_override(agent)?;
    let operator_origin = crate::origin::local_operator_origin(origin);
    let not_before = not_before
        .map(|text| crate::commands::parse_not_before(text, chrono::Utc::now()))
        .transpose()?;
    let payload = task_payload(
        project,
        description,
        agent_provider.as_deref(),
        Some(&operator_origin),
        &TaskSchedule { after, not_before },
    );
    let runner = WorkflowRunner::new(addr, project);
    println!("Running task for {project}...");
    let (event_id, events) = runner
        .run_workflow("execution_requested", payload, |t, _| t == "task_run_completed")
        .await?;

    let run = WatchedRun { event_id, events };
    runner.show_trace(run.trace_root_id()).await?;
    Ok(())
}

/// The owner's pacing constraints on a `foundry task` dispatch.
#[derive(Debug, Default)]
struct TaskSchedule<'a> {
    after: &'a [String],
    not_before: Option<chrono::DateTime<chrono::Utc>>,
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

/// Classify one project's dependency updates and show the brief and the
/// majors plan. Applies nothing and dispatches nothing.
pub async fn deps(addr: &str, project: &str, policy: Option<&str>) -> Result<()> {
    let payload = match policy {
        Some(p) => {
            p.parse::<foundry_sdk::registry::UpdatePolicy>()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            serde_json::json!({ "policy": p })
        }
        None => serde_json::Value::Null,
    };
    let runner = WorkflowRunner::new(addr, project);
    println!("Classifying dependency updates for {project}...");
    let (_event_id, events) = runner
        .run_workflow("dependency_review_requested", payload, |t, p| {
            t == "major_upgrades_planned" || review_block_failed(t, p)
        })
        .await?;
    println!();
    if let Some(failed) =
        events.iter().find(|e| review_block_failed(&e.event_type, &e.payload_json))
    {
        let summary = serde_json::from_str::<serde_json::Value>(&failed.payload_json)
            .ok()
            .and_then(|v| v.get("summary").and_then(serde_json::Value::as_str).map(str::to_string))
            .unwrap_or_default();
        anyhow::bail!("dependency review failed: {summary}");
    }

    let payload_of = |event_type: &str| {
        events
            .iter()
            .rev()
            .find(|e| e.event_type == event_type)
            .map(|e| e.payload_json.as_str())
    };
    let mut out = String::new();
    match payload_of("dependency_updates_classified").map(serde_json::from_str) {
        Some(Ok(classified)) => out.push_str(&render::dependencies::classification(&classified)),
        Some(Err(e)) => anyhow::bail!("unreadable classification payload: {e}"),
        None => anyhow::bail!("the daemon emitted no classification for {project}"),
    }
    if let Some(Ok(plan)) = payload_of("major_upgrades_planned").map(serde_json::from_str) {
        out.push_str(&render::dependencies::majors_plan(&plan));
    }
    print!("{out}");
    Ok(())
}

/// Whether a watched event says a block in the review chain failed, so the
/// review would never reach its terminal event.
fn review_block_failed(event_type: &str, payload_json: &str) -> bool {
    event_type == "block_completed"
        && serde_json::from_str::<serde_json::Value>(payload_json).is_ok_and(|v| {
            matches!(
                v.get("block").and_then(serde_json::Value::as_str),
                Some("Classify Dependency Updates" | "Plan Major Upgrades")
            ) && v.get("success").and_then(serde_json::Value::as_bool) == Some(false)
        })
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
        print!("{}", render::workflow::scout_result(project, &terminal.payload_json));
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
            print!("{}", render::workflow::validation_result(project_name, &terminal.payload_json));
            if render::workflow::validation_failed(&terminal.payload_json) {
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

    #[test]
    fn a_failed_classify_or_plan_block_ends_the_review() {
        let failed = r#"{"block":"Classify Dependency Updates","success":false,"summary":"project not found"}"#;
        let ok = r#"{"block":"Classify Dependency Updates","success":true}"#;
        let other = r#"{"block":"Install Locally","success":false}"#;
        assert!(super::review_block_failed("block_completed", failed));
        assert!(!super::review_block_failed("block_completed", ok));
        assert!(!super::review_block_failed("block_completed", other));
        assert!(!super::review_block_failed("dependency_updates_classified", failed));
    }

    // -- operator origin on `foundry task` --

    #[test]
    fn task_payload_carries_the_hostname_and_the_origin_text_verbatim() {
        let origin = crate::origin::operator_origin(Some("workbench"), Some("asked by Stacey"));
        let payload =
            super::task_payload("p", "do the thing", None, Some(&origin), &TaskSchedule::default());

        assert_eq!(payload["operator_origin"], "host workbench: asked by Stacey");
        assert_eq!(payload["prompt"], "do the thing");
        assert_eq!(payload["workflow"], "task");
    }

    #[test]
    fn a_failed_hostname_lookup_still_dispatches_with_the_stated_fallback() {
        // The lookup failing yields `None` for the hostname; the dispatch still
        // carries its prompt and gains a stated origin rather than no origin.
        let origin = crate::origin::operator_origin(None, Some("by hand"));
        let payload =
            super::task_payload("p", "do the thing", None, Some(&origin), &TaskSchedule::default());

        assert_eq!(payload["operator_origin"], "host unknown host: by hand");
        assert_eq!(payload["prompt"], "do the thing");
    }

    #[test]
    fn task_payload_without_operator_origin_is_unchanged() {
        let payload =
            super::task_payload("p", "do the thing", None, None, &TaskSchedule::default());

        assert_eq!(
            payload,
            serde_json::json!({ "project": "p", "workflow": "task", "prompt": "do the thing" })
        );
    }

    #[test]
    fn task_payload_carries_after_and_not_before_only_when_given() {
        let after = vec!["wi_a".to_string(), "wi_b".to_string()];
        let not_before: chrono::DateTime<chrono::Utc> = "2026-10-11T09:00:00Z".parse().unwrap();
        let payload = super::task_payload(
            "p",
            "do the thing",
            None,
            None,
            &TaskSchedule {
                after: &after,
                not_before: Some(not_before),
            },
        );
        assert_eq!(payload["depends_on"], serde_json::json!(["wi_a", "wi_b"]));
        assert_eq!(payload["not_before"], "2026-10-11T09:00:00+00:00");
    }

    fn watched(event_id: &str, event_type: &str, trace: &str, payload: &str) -> WatchResponse {
        WatchResponse {
            event_id: event_id.to_string(),
            event_type: event_type.to_string(),
            project: "p".to_string(),
            payload_json: payload.to_string(),
            trace_id: trace.to_string(),
            span_id: String::new(),
            parent_span_id: String::new(),
        }
    }

    #[test]
    fn a_paced_run_shows_the_trace_of_the_started_root() {
        let run = WatchedRun {
            event_id: "evt_admitted".to_string(),
            events: vec![
                watched("evt_admitted", "execution_requested", "t", "{}"),
                watched("evt_submitted", "work_item_submitted", "t", "{}"),
                watched(
                    "evt_started",
                    "execution_requested",
                    "t",
                    r#"{"admitted_work_item_id":"wi_x"}"#,
                ),
                watched("evt_done", "task_run_completed", "t", "{}"),
            ],
        };
        assert_eq!(run.trace_root_id(), "evt_started");

        let unpaced = WatchedRun {
            event_id: "evt_root".to_string(),
            events: vec![watched("evt_root", "execution_requested", "t", "{}")],
        };
        assert_eq!(unpaced.trace_root_id(), "evt_root");
    }

    #[test]
    fn only_the_runs_own_trace_passes_once_the_root_is_seen() {
        let mine = watched("evt_1", "task_run_completed", "mine", "{}");
        let other = watched("evt_2", "task_run_completed", "other", "{}");
        assert!(super::on_own_trace(None, &other), "before the root is seen, everything passes");
        assert!(super::on_own_trace(Some("mine"), &mine));
        assert!(!super::on_own_trace(Some("mine"), &other), "another run's terminal is not ours");
        assert!(super::on_own_trace(Some(""), &other), "a traceless root filters nothing");
    }
}
