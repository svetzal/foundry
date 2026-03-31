use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use chrono::Utc;

use foundry_core::event::{Event, EventType, PayloadExt};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::trace::ProcessResult;

use crate::summary::{
    AutoReleaseEntry, LocalInstallEntry, MaintenanceRunSummary, ProjectResult, ProjectStatus,
    ReleaseAuditEntry,
};
use crate::trace_writer::TraceWriter;

/// Generates a markdown summary report after a full maintenance run completes.
///
/// Observer — always runs regardless of throttle.
///
/// Sinks on `MaintenanceRunCompleted`. Reads per-project trace data via the
/// `TraceWriter`, builds a `MaintenanceRunSummary`, renders it as markdown,
/// and writes it to the audits directory.
///
/// Expected trigger payload:
/// ```json
/// {
///   "project_trace_ids": { "alpha": "evt_abc", "beta": "evt_def" },
///   "skipped_projects": ["gamma"],
///   "total_duration_ms": 57000
/// }
/// ```
pub struct GenerateSummary {
    trace_writer: Arc<TraceWriter>,
    audits_dir: PathBuf,
}

impl GenerateSummary {
    pub fn new(trace_writer: Arc<TraceWriter>, audits_dir: String) -> Self {
        Self {
            trace_writer,
            audits_dir: PathBuf::from(audits_dir),
        }
    }
}

/// Extract per-project status from a trace's `ProcessResult`.
fn extract_project_result(project: &str, result: &ProcessResult) -> ProjectResult {
    let failed_block = result.block_executions.iter().find(|b| !b.success);

    let status = if let Some(block) = failed_block {
        ProjectStatus::Failed(block.summary.clone())
    } else {
        ProjectStatus::Success
    };

    ProjectResult {
        name: project.to_string(),
        status,
        duration_secs: Some(result.total_duration_ms / 1000),
    }
}

/// Extract release audit entries from a trace's events.
fn extract_release_audits(project: &str, result: &ProcessResult) -> Vec<ReleaseAuditEntry> {
    result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::ReleaseTagAudited)
        .map(|e| {
            let tag = e.payload.str_or("tag", "").to_string();
            let vulnerable = e.payload.bool_or("vulnerable", false);
            let status = if vulnerable { "vulnerable" } else { "clean" }.to_string();
            ReleaseAuditEntry {
                name: project.to_string(),
                tag,
                status,
            }
        })
        .collect()
}

/// Extract auto-release entries from a trace's events.
fn extract_auto_releases(project: &str, result: &ProcessResult) -> Vec<AutoReleaseEntry> {
    result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::ReleaseCompleted)
        .map(|e| {
            let new_tag =
                e.payload.get("new_tag").and_then(|v| v.as_str()).map(ToString::to_string);
            let success = e.payload.bool_or("success", false);
            AutoReleaseEntry {
                name: project.to_string(),
                new_tag,
                success,
            }
        })
        .collect()
}

/// Extract local install entries from a trace's events.
fn extract_local_installs(project: &str, result: &ProcessResult) -> Vec<LocalInstallEntry> {
    result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::LocalInstallCompleted)
        .map(|e| {
            let method = e.payload.str_or("method", "unknown").to_string();
            let success = e.payload.bool_or("success", false);
            LocalInstallEntry {
                name: project.to_string(),
                method,
                success,
            }
        })
        .collect()
}

impl TaskBlock for GenerateSummary {
    task_block_meta! {
        name: "Generate Summary",
        kind: Observer,
        sinks_on: [MaintenanceRunCompleted],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let payload = trigger.payload.clone();
        let trace_writer = Arc::clone(&self.trace_writer);
        let audits_dir = self.audits_dir.clone();

        Box::pin(async move {
            // Extract per-project trace IDs from the payload.
            let project_trace_ids: std::collections::HashMap<String, String> = payload
                .get("project_trace_ids")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();

            let skipped_projects: Vec<String> = payload
                .get("skipped_projects")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();

            let total_duration_ms: u64 = payload.u64_or("total_duration_ms", 0);

            let mut projects = Vec::new();
            let mut release_audits = Vec::new();
            let mut auto_releases = Vec::new();
            let mut local_installs = Vec::new();

            // Load each project's trace and extract results.
            for (project_name, event_id) in &project_trace_ids {
                if let Some(result) = trace_writer.read(event_id) {
                    projects.push(extract_project_result(project_name, &result));
                    release_audits.extend(extract_release_audits(project_name, &result));
                    auto_releases.extend(extract_auto_releases(project_name, &result));
                    local_installs.extend(extract_local_installs(project_name, &result));
                } else {
                    tracing::warn!(
                        project = %project_name,
                        event_id = %event_id,
                        "trace not found for project"
                    );
                    projects.push(ProjectResult {
                        name: project_name.clone(),
                        status: ProjectStatus::Failed("trace not found".to_string()),
                        duration_secs: None,
                    });
                }
            }

            // Add skipped projects.
            for name in &skipped_projects {
                projects.push(ProjectResult {
                    name: name.clone(),
                    status: ProjectStatus::Skipped("already active".to_string()),
                    duration_secs: None,
                });
            }

            // Sort projects by name for stable output.
            projects.sort_by(|a, b| a.name.cmp(&b.name));

            let summary = MaintenanceRunSummary {
                run_at: Utc::now(),
                total_duration_secs: Some(total_duration_ms / 1000),
                projects,
                release_audits,
                auto_releases,
                local_installs,
            };

            let markdown = crate::summary::render(&summary);

            // Write summary to {audits_dir}/runs/YYYY-MM-DD/summary.md
            let date = Utc::now().format("%Y-%m-%d").to_string();
            let runs_dir = audits_dir.join("runs").join(&date);
            if let Err(e) = std::fs::create_dir_all(&runs_dir) {
                tracing::error!(
                    error = %e,
                    path = %runs_dir.display(),
                    "failed to create summary directory"
                );
                return Ok(TaskBlockResult::failure(format!("Failed to create directory: {e}")));
            }

            let summary_path = runs_dir.join("summary.md");
            if let Err(e) = std::fs::write(&summary_path, &markdown) {
                tracing::error!(
                    error = %e,
                    path = %summary_path.display(),
                    "failed to write summary"
                );
                return Ok(TaskBlockResult::failure(format!("Failed to write summary: {e}")));
            }

            let path_str = summary_path.to_string_lossy().to_string();
            tracing::info!(path = %path_str, "maintenance summary written");

            Ok(TaskBlockResult::success(format!("Summary written to {path_str}"), vec![])
                .with_output(Some(markdown), None)
                .with_audit_artifacts(vec![path_str]))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::event::{Event, EventType};
    use foundry_core::throttle::Throttle;
    use foundry_core::trace::{BlockExecution, ProcessResult};

    fn make_trace_writer(dir: &std::path::Path) -> Arc<TraceWriter> {
        Arc::new(TraceWriter::new(dir.to_str().unwrap()))
    }

    fn make_trigger(payload: serde_json::Value) -> Event {
        Event::new(
            EventType::MaintenanceRunCompleted,
            "_system".to_string(),
            Throttle::Full,
            payload,
        )
    }

    fn successful_trace(project: &str) -> ProcessResult {
        let root = Event::new(
            EventType::MaintenanceRunStarted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        ProcessResult {
            events: vec![root],
            block_executions: vec![BlockExecution {
                block_name: "Validate Project".to_string(),
                trigger_event_id: "evt_root".to_string(),
                success: true,
                summary: "Project validated".to_string(),
                emitted_event_ids: vec![],
                duration_ms: 100,
                raw_output: None,
                exit_code: None,
                trigger_payload: serde_json::json!({}),
                emitted_payloads: vec![],
                audit_artifacts: vec![],
            }],
            total_duration_ms: 5000,
        }
    }

    fn failed_trace(project: &str) -> ProcessResult {
        let root = Event::new(
            EventType::MaintenanceRunStarted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        ProcessResult {
            events: vec![root],
            block_executions: vec![BlockExecution {
                block_name: "Run Hone Maintain".to_string(),
                trigger_event_id: "evt_root".to_string(),
                success: false,
                summary: "cargo clippy failed: error[E0308]".to_string(),
                emitted_event_ids: vec![],
                duration_ms: 12000,
                raw_output: None,
                exit_code: Some(1),
                trigger_payload: serde_json::json!({}),
                emitted_payloads: vec![],
                audit_artifacts: vec![],
            }],
            total_duration_ms: 12000,
        }
    }

    fn trace_with_release_audit(project: &str) -> ProcessResult {
        let root = Event::new(
            EventType::MaintenanceRunStarted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let audit_event = Event::new(
            EventType::ReleaseTagAudited,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"tag": "v1.0.0", "vulnerable": false}),
        );
        let auto_release = Event::new(
            EventType::ReleaseCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"new_tag": "v1.0.1", "success": true}),
        );
        let install = Event::new(
            EventType::LocalInstallCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"method": "brew", "success": true}),
        );
        ProcessResult {
            events: vec![root, audit_event, auto_release, install],
            block_executions: vec![BlockExecution {
                block_name: "Validate Project".to_string(),
                trigger_event_id: "evt_root".to_string(),
                success: true,
                summary: "ok".to_string(),
                emitted_event_ids: vec![],
                duration_ms: 100,
                raw_output: None,
                exit_code: None,
                trigger_payload: serde_json::json!({}),
                emitted_payloads: vec![],
                audit_artifacts: vec![],
            }],
            total_duration_ms: 8000,
        }
    }

    // -- Metadata tests --

    #[test]
    fn sinks_on_maintenance_run_completed() {
        let dir = tempfile::tempdir().unwrap();
        let block = GenerateSummary::new(
            make_trace_writer(dir.path()),
            dir.path().to_str().unwrap().to_string(),
        );
        assert_eq!(block.sinks_on(), &[EventType::MaintenanceRunCompleted]);
    }

    #[test]
    fn kind_is_observer() {
        let dir = tempfile::tempdir().unwrap();
        let block = GenerateSummary::new(
            make_trace_writer(dir.path()),
            dir.path().to_str().unwrap().to_string(),
        );
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    // -- Summary generation tests --

    #[tokio::test]
    async fn writes_summary_for_successful_projects() {
        let traces_dir = tempfile::tempdir().unwrap();
        let audits_dir = tempfile::tempdir().unwrap();
        let tw = make_trace_writer(traces_dir.path());

        // Write traces for two projects.
        tw.write("evt_alpha", &successful_trace("alpha")).unwrap();
        tw.write("evt_beta", &successful_trace("beta")).unwrap();

        let block = GenerateSummary::new(tw, audits_dir.path().to_str().unwrap().to_string());

        let trigger = make_trigger(serde_json::json!({
            "project_trace_ids": {"alpha": "evt_alpha", "beta": "evt_beta"},
            "skipped_projects": [],
            "total_duration_ms": 10000
        }));

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.summary.contains("summary.md"));
        assert!(!result.audit_artifacts.is_empty());

        // Verify the file was written.
        let artifact_path = &result.audit_artifacts[0];
        let content = std::fs::read_to_string(artifact_path).unwrap();
        assert!(content.contains("alpha"));
        assert!(content.contains("beta"));
        assert!(content.contains("success"));
    }

    #[tokio::test]
    async fn includes_failed_projects_in_summary() {
        let traces_dir = tempfile::tempdir().unwrap();
        let audits_dir = tempfile::tempdir().unwrap();
        let tw = make_trace_writer(traces_dir.path());

        tw.write("evt_good", &successful_trace("good-project")).unwrap();
        tw.write("evt_bad", &failed_trace("bad-project")).unwrap();

        let block = GenerateSummary::new(tw, audits_dir.path().to_str().unwrap().to_string());

        let trigger = make_trigger(serde_json::json!({
            "project_trace_ids": {"good-project": "evt_good", "bad-project": "evt_bad"},
            "total_duration_ms": 17000
        }));

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let md = result.raw_output.unwrap();
        assert!(md.contains("failed"));
        assert!(md.contains("## Failures"));
        assert!(md.contains("bad-project"));
        assert!(md.contains("cargo clippy failed"));
    }

    #[tokio::test]
    async fn includes_skipped_projects() {
        let traces_dir = tempfile::tempdir().unwrap();
        let audits_dir = tempfile::tempdir().unwrap();
        let tw = make_trace_writer(traces_dir.path());

        tw.write("evt_alpha", &successful_trace("alpha")).unwrap();

        let block = GenerateSummary::new(tw, audits_dir.path().to_str().unwrap().to_string());

        let trigger = make_trigger(serde_json::json!({
            "project_trace_ids": {"alpha": "evt_alpha"},
            "skipped_projects": ["gamma"],
            "total_duration_ms": 5000
        }));

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let md = result.raw_output.unwrap();
        assert!(md.contains("gamma"));
        assert!(md.contains("skipped"));
    }

    #[tokio::test]
    async fn handles_missing_trace_gracefully() {
        let traces_dir = tempfile::tempdir().unwrap();
        let audits_dir = tempfile::tempdir().unwrap();
        let tw = make_trace_writer(traces_dir.path());

        // Don't write any trace — evt_missing won't be found.
        let block = GenerateSummary::new(tw, audits_dir.path().to_str().unwrap().to_string());

        let trigger = make_trigger(serde_json::json!({
            "project_trace_ids": {"missing-project": "evt_missing"},
            "total_duration_ms": 0
        }));

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let md = result.raw_output.unwrap();
        assert!(md.contains("missing-project"));
        assert!(md.contains("failed"));
        assert!(md.contains("trace not found"));
    }

    #[tokio::test]
    async fn extracts_release_audit_and_install_data() {
        let traces_dir = tempfile::tempdir().unwrap();
        let audits_dir = tempfile::tempdir().unwrap();
        let tw = make_trace_writer(traces_dir.path());

        tw.write("evt_proj", &trace_with_release_audit("my-project")).unwrap();

        let block = GenerateSummary::new(tw, audits_dir.path().to_str().unwrap().to_string());

        let trigger = make_trigger(serde_json::json!({
            "project_trace_ids": {"my-project": "evt_proj"},
            "total_duration_ms": 8000
        }));

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let md = result.raw_output.unwrap();
        assert!(md.contains("## Release Audit"));
        assert!(md.contains("v1.0.0"));
        assert!(md.contains("## Auto-Releases"));
        assert!(md.contains("v1.0.1"));
        assert!(md.contains("## Local Installs"));
        assert!(md.contains("brew"));
    }

    #[tokio::test]
    async fn empty_payload_produces_empty_summary() {
        let traces_dir = tempfile::tempdir().unwrap();
        let audits_dir = tempfile::tempdir().unwrap();
        let tw = make_trace_writer(traces_dir.path());

        let block = GenerateSummary::new(tw, audits_dir.path().to_str().unwrap().to_string());

        let trigger = make_trigger(serde_json::json!({}));

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let md = result.raw_output.unwrap();
        assert!(md.contains("# Foundry Maintenance Run"));
        assert!(md.contains("- Total projects: 0"));
    }

    // -- Extract function unit tests --

    #[test]
    fn extract_project_result_success() {
        let trace = successful_trace("alpha");
        let result = extract_project_result("alpha", &trace);
        assert_eq!(result.name, "alpha");
        assert_eq!(result.status, ProjectStatus::Success);
        assert_eq!(result.duration_secs, Some(5));
    }

    #[test]
    fn extract_project_result_failure() {
        let trace = failed_trace("beta");
        let result = extract_project_result("beta", &trace);
        assert_eq!(result.name, "beta");
        assert!(matches!(result.status, ProjectStatus::Failed(_)));
        if let ProjectStatus::Failed(reason) = &result.status {
            assert!(reason.contains("cargo clippy failed"));
        }
    }

    #[test]
    fn extract_release_audits_from_trace() {
        let trace = trace_with_release_audit("proj");
        let audits = extract_release_audits("proj", &trace);
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].tag, "v1.0.0");
        assert_eq!(audits[0].status, "clean");
    }

    #[test]
    fn extract_auto_releases_from_trace() {
        let trace = trace_with_release_audit("proj");
        let releases = extract_auto_releases("proj", &trace);
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].new_tag, Some("v1.0.1".to_string()));
        assert!(releases[0].success);
    }

    #[test]
    fn extract_local_installs_from_trace() {
        let trace = trace_with_release_audit("proj");
        let installs = extract_local_installs("proj", &trace);
        assert_eq!(installs.len(), 1);
        assert_eq!(installs[0].method, "brew");
        assert!(installs[0].success);
    }
}
