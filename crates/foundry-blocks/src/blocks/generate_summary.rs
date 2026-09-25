use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use chrono::Utc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    GitSyncFailure, LocalInstallCompletedPayload, MajorUpgradesPlannedPayload,
    ProjectCompletedPayload, ProjectValidationCompletedPayload, ReleaseCompletedPayload,
    ReleaseTagAuditedPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::trace::ProcessResult;

use crate::gateway::ShellGateway;
use crate::summary::{
    AppliedUpdate, AutoReleaseEntry, LocalInstallEntry, MaintenanceRunSummary, MajorsSummary,
    ProjectDependencyReport, ProjectResult, ProjectStatus, ReleaseAuditEntry, ScannerFailureEntry,
    UnpushedEntry, UnpushedStatus, WrongBranchEntry,
};
use crate::trace_writer::TraceWriter;
use foundry_sdk::payload::{
    ClassificationPhase, DependencyUpdatesClassifiedPayload, OutdatedDependency,
};

/// Generates a markdown summary report after a full maintenance run completes.
///
/// Observer — always runs regardless of throttle.
///
/// Sinks on `MajorUpgradesPlanned` (nightly, not a review) — emitted by
/// `Plan Major Upgrades` on `MaintenanceSummaryRequested`, which the service
/// layer emits once a cycle's per-project traces are persisted. The majors
/// plan rides on it, carrying the trace locations forward. Reads per-project trace data
/// via the `TraceWriter`, builds a `MaintenanceRunSummary`, renders it as
/// markdown, and writes it to the audits directory.
///
/// Post-run check: for every push-enabled project in the run it counts the
/// commits the branch holds that `origin/<branch>` does not, and reports any
/// non-zero count (or a failed check) at the top of the summary. Commits
/// silently stranded on the build host were the failure this exists to catch.
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
    registry: Arc<RwLock<Registry>>,
    shell: Arc<dyn ShellGateway>,
}

impl GenerateSummary {
    pub fn new(
        trace_writer: Arc<TraceWriter>,
        audits_dir: String,
        registry: Arc<RwLock<Registry>>,
        shell: Arc<dyn ShellGateway>,
    ) -> Self {
        Self {
            trace_writer,
            audits_dir: PathBuf::from(audits_dir),
            registry,
            shell,
        }
    }
}

/// Count unpushed commits for each push-enabled project named in the run.
/// Uses the remote-tracking ref as the commit step left it (no fetch).
async fn find_unpushed(
    names: &[String],
    registry: &Arc<RwLock<Registry>>,
    shell: &dyn ShellGateway,
) -> Vec<UnpushedEntry> {
    // Extract before any .await so the lock is not held across yields.
    let targets: Vec<(String, String, String)> = match super::read_registry(registry) {
        Ok(guard) => names
            .iter()
            .filter_map(|name| guard.find_project(name))
            .filter(|e| e.actions.push)
            .map(|e| (e.name.clone(), e.path.clone(), e.branch.clone()))
            .collect(),
        Err(e) => {
            // Record: the check did not run, which the summary must say.
            tracing::error!(error = %e, "registry unreadable; unpushed-commit check skipped");
            return vec![UnpushedEntry {
                name: "(all projects)".to_string(),
                status: UnpushedStatus::Unknown(format!("registry unreadable: {e}")),
            }];
        }
    };

    let mut unpushed = Vec::new();
    for (name, path, branch) in targets {
        let status =
            match super::checkout_sync::commits_ahead(shell, std::path::Path::new(&path), &branch)
                .await
            {
                Ok(Some(0)) => continue,
                Ok(Some(commits)) => UnpushedStatus::Ahead { commits, branch },
                Ok(None) => UnpushedStatus::Unknown(format!("could not resolve origin/{branch}")),
                Err(e) => UnpushedStatus::Unknown(e.to_string()),
            };
        tracing::warn!(project = %name, ?status, "project has unpushed commits after the run");
        unpushed.push(UnpushedEntry { name, status });
    }
    unpushed
}

/// Terminal event types whose `success` payload field determines overall outcome.
///
/// Must stay in sync with `foundry_sdk::trace::TERMINAL_EVENT_TYPES`.
const TERMINAL_EVENT_TYPES: &[EventType] = &[
    EventType::ProjectIterationCompleted,
    EventType::ProjectMaintenanceCompleted,
    EventType::InnerIterationCompleted,
];

/// Extract per-project status from a trace's `ProcessResult`.
///
/// Prefers the `success` field of a terminal completion event when one is
/// present — this aligns with `ProcessResult::is_success()` and avoids
/// reporting success when the terminal event says otherwise (e.g. mojentic-ex
/// where gates failed but earlier blocks succeeded).
///
/// Falls back to inspecting block executions when no terminal event exists
/// (legacy traces, or very early chain aborts before any block runs).
fn extract_project_result(project: &str, result: &ProcessResult) -> ProjectResult {
    let terminal = result.events.iter().find(|e| TERMINAL_EVENT_TYPES.contains(&e.event_type));

    let status = if let Some(event) = terminal {
        match event.parse_payload::<ProjectCompletedPayload>() {
            Ok(p) if p.success => ProjectStatus::Success,
            Ok(p) => {
                let summary = if p.summary.is_empty() {
                    "terminal event reported failure".to_string()
                } else {
                    p.summary
                };
                // Check whether the agent concluded no real correction was needed.
                // "triage rejected", "no correction warranted", etc. are benign outcomes.
                if super::triage_core::is_benign_decline(&summary) {
                    ProjectStatus::Success
                } else {
                    ProjectStatus::Failed(summary)
                }
            }
            Err(e) => {
                // Record: a malformed terminal-event payload is not the same
                // fault as a genuine project failure — keep the two
                // distinguishable in the summary text rather than falling
                // back to the generic "terminal event reported failure".
                tracing::warn!(
                    %project,
                    error = %e,
                    "terminal event payload unreadable while generating summary"
                );
                ProjectStatus::Failed(format!("payload unreadable: {e}"))
            }
        }
    } else {
        // Fallback: no terminal event — use block executions.
        let failed_block = result.block_executions.iter().find(|b| !b.success);
        if let Some(block) = failed_block {
            ProjectStatus::Failed(block.summary.clone())
        } else {
            ProjectStatus::Success
        }
    };

    ProjectResult {
        name: project.to_string(),
        status,
        duration_secs: Some(result.total_duration_ms / 1000),
    }
}

/// Extract release audit entries from a trace's events.
///
/// Trace-event payload extraction goes through `ProcessResult::parsed_events_of`;
/// future blocks that scan traces should reuse it rather than re-rolling the
/// filter-parse loop.
fn extract_release_audits(project: &str, result: &ProcessResult) -> Vec<ReleaseAuditEntry> {
    result
        .parsed_events_of::<ReleaseTagAuditedPayload>(EventType::ReleaseTagAudited)
        .map(|p| ReleaseAuditEntry {
            name: project.to_string(),
            tag: p.tag,
            status: if p.scan_error.is_some() {
                "scanner failed"
            } else if p.vulnerable {
                "vulnerable"
            } else {
                "clean"
            }
            .to_string(),
        })
        .collect()
}

/// Audits in a trace that did not run.
fn extract_scanner_failures(project: &str, result: &ProcessResult) -> Vec<ScannerFailureEntry> {
    result
        .parsed_events_of::<ReleaseTagAuditedPayload>(EventType::ReleaseTagAudited)
        .filter_map(|p| p.scan_error)
        .map(|error| ScannerFailureEntry {
            name: project.to_string(),
            error,
        })
        .collect()
}

/// Validation that stopped because the checkout was on another branch.
fn extract_wrong_branch(project: &str, result: &ProcessResult) -> Vec<WrongBranchEntry> {
    result
        .parsed_events_of::<ProjectValidationCompletedPayload>(
            EventType::ProjectValidationCompleted,
        )
        .filter(|p| p.sync_failure == Some(GitSyncFailure::WrongBranch))
        .map(|p| WrongBranchEntry {
            name: project.to_string(),
            reason: p.reason.unwrap_or_else(|| "wrong branch".to_string()),
        })
        .collect()
}

/// Extract auto-release entries from a trace's events.
fn extract_auto_releases(project: &str, result: &ProcessResult) -> Vec<AutoReleaseEntry> {
    result
        .parsed_events_of::<ReleaseCompletedPayload>(EventType::ReleaseCompleted)
        .map(|p| AutoReleaseEntry {
            name: project.to_string(),
            new_tag: p.new_tag,
            success: p.success,
        })
        .collect()
}

/// Extract local install entries from a trace's events.
fn extract_local_installs(project: &str, result: &ProcessResult) -> Vec<LocalInstallEntry> {
    result
        .parsed_events_of::<LocalInstallCompletedPayload>(EventType::LocalInstallCompleted)
        .map(|p| LocalInstallEntry {
            name: project.to_string(),
            method: p.method.unwrap_or_else(|| "unknown".to_string()),
            success: p.success,
        })
        .collect()
}

/// The key a dependency is compared by across the before and after phases.
fn dependency_key(d: &OutdatedDependency) -> (foundry_sdk::payload::Ecosystem, &str, &str) {
    (d.ecosystem, d.manifest.as_str(), d.package.as_str())
}

/// What maintenance did to a project's dependencies, from the classification
/// before the agent ran and the one after maintenance completed.
///
/// A dependency counts as moved when its locked version changed, or when it is
/// no longer outdated (it reached the newest release). A scope the after
/// classification could not read is not counted either way.
pub(crate) fn dependency_report(
    project: &str,
    before: &DependencyUpdatesClassifiedPayload,
    after: Option<&DependencyUpdatesClassifiedPayload>,
) -> ProjectDependencyReport {
    let brief = &before.brief;
    let applied = after.map(|after| {
        let unreadable = |d: &OutdatedDependency| {
            let label = format!("{} ({})", d.ecosystem, d.manifest);
            after
                .classification
                .unclassified
                .iter()
                .any(|u| u.scope == label || u.scope == format!("{label} {}", d.package))
        };
        before
            .classification
            .outdated
            .iter()
            .filter(|d| !unreadable(d))
            .filter_map(|d| {
                let to = match after
                    .classification
                    .outdated
                    .iter()
                    .find(|a| dependency_key(a) == dependency_key(d))
                {
                    Some(a) if a.current != d.current => a.current.clone(),
                    Some(_) => return None,
                    None => d
                        .major
                        .clone()
                        .or_else(|| d.non_major.clone())
                        .or_else(|| d.in_range.clone())?,
                };
                let class =
                    crate::dependency_updates::version::Version::parse(d.ecosystem, &d.current)
                        .zip(crate::dependency_updates::version::Version::parse(d.ecosystem, &to))
                        .and_then(|(f, t)| f.class_to(&t))
                        .unwrap_or(foundry_sdk::payload::UpdateClass::Patch);
                let in_brief = brief.apply.iter().any(|u| {
                    u.ecosystem == d.ecosystem && u.manifest == d.manifest && u.package == d.package
                });
                Some(AppliedUpdate {
                    ecosystem: d.ecosystem,
                    manifest: d.manifest.clone(),
                    package: d.package.clone(),
                    from: d.current.clone(),
                    to,
                    class,
                    in_brief,
                })
            })
            .collect::<Vec<_>>()
    });
    let not_applied = match &applied {
        Some(applied) => brief
            .apply
            .iter()
            .filter(|u| {
                // Transitive security moves are not in the outdated list; only
                // direct dependencies can be checked.
                before
                    .classification
                    .outdated
                    .iter()
                    .any(|d| d.package == u.package && d.manifest == u.manifest)
                    && !applied.iter().any(|a| a.package == u.package && a.manifest == u.manifest)
            })
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    let source = after.unwrap_or(before);
    ProjectDependencyReport {
        name: project.to_string(),
        policy: brief.policy,
        policy_set: brief.policy_set,
        applied,
        not_applied,
        held_by_policy: brief.held_by_policy.clone(),
        held_by_hold: brief.held_by_hold.clone(),
        lapsed_holds: source.classification.lapsed_holds.clone(),
        stale_holds: source.classification.stale_holds.clone(),
        vendored: source.classification.vendored.clone(),
        unclassified: source.classification.unclassified.clone(),
        holds_warning: source.classification.holds_warning.clone(),
    }
}

/// A project's dependency report from its trace, when maintenance classified it.
fn extract_dependency_report(
    project: &str,
    result: &ProcessResult,
) -> Option<ProjectDependencyReport> {
    let all: Vec<DependencyUpdatesClassifiedPayload> = result
        .parsed_events_of::<DependencyUpdatesClassifiedPayload>(
            EventType::DependencyUpdatesClassified,
        )
        .collect();
    let before = all.iter().find(|p| p.phase == ClassificationPhase::Before)?;
    let after = all.iter().find(|p| p.phase == ClassificationPhase::After);
    Some(dependency_report(project, before, after))
}

/// Everything the summary reads out of the per-project traces.
#[derive(Default)]
struct LoadedResults {
    projects: Vec<ProjectResult>,
    release_audits: Vec<ReleaseAuditEntry>,
    auto_releases: Vec<AutoReleaseEntry>,
    local_installs: Vec<LocalInstallEntry>,
    scanner_failures: Vec<ScannerFailureEntry>,
    wrong_branch: Vec<WrongBranchEntry>,
    dependencies: Vec<ProjectDependencyReport>,
}

fn load_project_results(
    trace_writer: &TraceWriter,
    project_trace_ids: &std::collections::HashMap<String, String>,
) -> LoadedResults {
    let mut loaded = LoadedResults::default();

    for (project_name, event_id) in project_trace_ids {
        if let Some(result) = trace_writer.read(event_id) {
            loaded.projects.push(extract_project_result(project_name, &result));
            loaded.release_audits.extend(extract_release_audits(project_name, &result));
            loaded.auto_releases.extend(extract_auto_releases(project_name, &result));
            loaded.local_installs.extend(extract_local_installs(project_name, &result));
            loaded.scanner_failures.extend(extract_scanner_failures(project_name, &result));
            loaded.wrong_branch.extend(extract_wrong_branch(project_name, &result));
            loaded.dependencies.extend(extract_dependency_report(project_name, &result));
        } else {
            tracing::warn!(
                project = %project_name,
                event_id = %event_id,
                "trace not found for project"
            );
            loaded.projects.push(ProjectResult {
                name: project_name.clone(),
                status: ProjectStatus::Failed("trace not found".to_string()),
                duration_secs: None,
            });
        }
    }

    loaded.scanner_failures.sort_by(|a, b| a.name.cmp(&b.name));
    loaded.wrong_branch.sort_by(|a, b| a.name.cmp(&b.name));
    loaded.dependencies.sort_by(|a, b| a.name.cmp(&b.name));
    loaded
}

/// The loud problems in a run, for the block's result line.
fn summary_warnings(summary: &MaintenanceRunSummary) -> Vec<String> {
    let mut warnings = Vec::new();
    if !summary.unpushed.is_empty() {
        warnings.push(format!("{} project(s) have unpushed commits", summary.unpushed.len()));
    }
    if !summary.scanner_failures.is_empty() {
        let names: std::collections::BTreeSet<&str> =
            summary.scanner_failures.iter().map(|f| f.name.as_str()).collect();
        warnings.push(format!("{} project(s) with scanner failures", names.len()));
    }
    if !summary.wrong_branch.is_empty() {
        warnings.push(format!("{} project(s) skipped: wrong branch", summary.wrong_branch.len()));
    }
    let beyond = summary
        .dependencies
        .iter()
        .filter(|d| {
            d.applied
                .iter()
                .flatten()
                .any(|a| !a.in_brief || a.class == foundry_sdk::payload::UpdateClass::Major)
        })
        .count();
    if beyond > 0 {
        warnings.push(format!("{beyond} project(s) applied dependency updates beyond the brief"));
    }
    let stale: usize = summary.dependencies.iter().map(|d| d.stale_holds.len()).sum();
    if stale > 0 {
        warnings.push(format!("{stale} stale dependency hold(s) to re-decide"));
    }
    let lapsed: usize = summary.dependencies.iter().map(|d| d.lapsed_holds.len()).sum();
    if lapsed > 0 {
        warnings.push(format!("{lapsed} lapsed dependency hold(s) to re-decide"));
    }
    warnings
}

fn write_summary(audits_dir: &std::path::Path, markdown: &str) -> anyhow::Result<String> {
    let date = Utc::now().format("%Y-%m-%d").to_string();
    let runs_dir = audits_dir.join("runs").join(&date);
    if let Err(e) = std::fs::create_dir_all(&runs_dir) {
        tracing::error!(
            error = %e,
            path = %runs_dir.display(),
            "failed to create summary directory"
        );
        return Err(anyhow::anyhow!("Failed to create directory: {e}"));
    }

    let summary_path = runs_dir.join("summary.md");
    if let Err(e) = std::fs::write(&summary_path, markdown) {
        tracing::error!(
            error = %e,
            path = %summary_path.display(),
            "failed to write summary"
        );
        return Err(anyhow::anyhow!("Failed to write summary: {e}"));
    }

    let path_str = summary_path.to_string_lossy().to_string();
    tracing::info!(path = %path_str, "maintenance summary written");
    Ok(path_str)
}

impl TaskBlock for GenerateSummary {
    task_block_meta! {
        name: "Generate Summary",
        kind: Observer,
        sinks_on: [MajorUpgradesPlanned],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        trigger.parse_payload::<MajorUpgradesPlannedPayload>().is_ok_and(|p| !p.review)
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let p = parse_payload!(trigger, MajorUpgradesPlannedPayload);
        let trace_writer = Arc::clone(&self.trace_writer);
        let audits_dir = self.audits_dir.clone();
        let registry = Arc::clone(&self.registry);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let project_trace_ids = p.project_trace_ids;
            let skipped_projects = p.skipped_projects;
            let total_duration_ms = p.total_duration_ms;

            let LoadedResults {
                mut projects,
                release_audits,
                auto_releases,
                local_installs,
                scanner_failures,
                wrong_branch,
                dependencies,
            } = load_project_results(&trace_writer, &project_trace_ids);
            let majors = MajorsSummary {
                upgrades: p.upgrades,
                dispatch_enabled: p.dispatch_enabled,
                per_project_cap: p.per_project_cap,
                per_night_cap: p.per_night_cap,
                history_warning: p.history_warning,
            };

            for name in &skipped_projects {
                projects.push(ProjectResult {
                    name: name.clone(),
                    status: ProjectStatus::Skipped("already active".to_string()),
                    duration_secs: None,
                });
            }

            projects.sort_by(|a, b| a.name.cmp(&b.name));

            let names: Vec<String> = projects.iter().map(|p| p.name.clone()).collect();
            let unpushed = find_unpushed(&names, &registry, &*shell).await;

            let summary = MaintenanceRunSummary {
                run_at: Utc::now(),
                total_duration_secs: Some(total_duration_ms / 1000),
                projects,
                release_audits,
                auto_releases,
                local_installs,
                unpushed,
                scanner_failures,
                wrong_branch,
                dependencies,
                majors,
            };
            let warnings = summary_warnings(&summary);

            let markdown = crate::summary::render(&summary);

            let path_str = match write_summary(&audits_dir, &markdown) {
                Ok(p) => p,
                Err(e) => return Ok(TaskBlockResult::failure(e.to_string())),
            };

            let headline = if warnings.is_empty() {
                format!("Summary written to {path_str}")
            } else {
                format!("Summary written to {path_str}; WARNING: {}", warnings.join("; "))
            };
            Ok(TaskBlockResult::success(headline, vec![])
                .with_output(Some(markdown), None)
                .with_audit_artifacts(vec![path_str]))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::trace::{BlockExecution, ProcessResult};

    use super::super::test_helpers;

    fn make_trace_writer(dir: &std::path::Path) -> Arc<TraceWriter> {
        Arc::new(TraceWriter::new(dir.to_str().unwrap()))
    }

    /// A summary block whose projects are not in the registry, so the
    /// unpushed-commit check has nothing to inspect.
    fn summary_block(tw: Arc<TraceWriter>, audits_dir: &std::path::Path) -> GenerateSummary {
        GenerateSummary::new(
            tw,
            audits_dir.to_str().unwrap().to_string(),
            test_helpers::empty_registry(),
            crate::gateway::fakes::FakeShellGateway::success(),
        )
    }

    fn successful_trace(project: &str) -> ProcessResult {
        let root = Event::new(
            EventType::ProjectRunStarted,
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
                span_id: None,
                parent_span_id: None,
            }],
            total_duration_ms: 5000,
        }
    }

    fn failed_trace(project: &str) -> ProcessResult {
        let root = Event::new(
            EventType::ProjectRunStarted,
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
                span_id: None,
                parent_span_id: None,
            }],
            total_duration_ms: 12000,
        }
    }

    fn trace_with_release_audit(project: &str) -> ProcessResult {
        let root = Event::new(
            EventType::ProjectRunStarted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let audit_event = Event::new(
            EventType::ReleaseTagAudited,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"project": project, "cve": "CVE-test", "tag": "v1.0.0", "vulnerable": false}),
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
                span_id: None,
                parent_span_id: None,
            }],
            total_duration_ms: 8000,
        }
    }

    // -- Metadata tests --

    #[test]
    fn sinks_on_expected() {
        let dir = tempfile::tempdir().unwrap();
        let block = summary_block(make_trace_writer(dir.path()), dir.path());
        assert_eq!(block.sinks_on(), &[EventType::MajorUpgradesPlanned]);
    }

    #[test]
    fn kind_is() {
        let dir = tempfile::tempdir().unwrap();
        let block = summary_block(make_trace_writer(dir.path()), dir.path());
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

        let block = summary_block(tw, audits_dir.path());

        let trigger = test_helpers::make_trigger(
            EventType::MajorUpgradesPlanned,
            "_system",
            serde_json::json!({
                "project_trace_ids": {"alpha": "evt_alpha", "beta": "evt_beta"},
                "skipped_projects": [],
                "total_duration_ms": 10000
            }),
        );

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

    fn push_registry(entries: &[(&str, bool)]) -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects: entries
                .iter()
                .map(|(name, push)| foundry_sdk::registry::ProjectEntry {
                    actions: foundry_sdk::registry::ActionFlags {
                        push: *push,
                        ..Default::default()
                    },
                    ..test_helpers::project_entry(name, &format!("/p/{name}"))
                })
                .collect(),
        }))
    }

    fn summary_request(names: &[&str]) -> Event {
        let ids: serde_json::Map<String, serde_json::Value> =
            names.iter().map(|n| ((*n).to_string(), format!("evt_{n}").into())).collect();
        test_helpers::make_trigger(
            EventType::MajorUpgradesPlanned,
            "_system",
            serde_json::json!({
                "project_trace_ids": ids,
                "skipped_projects": [],
                "total_duration_ms": 1000
            }),
        )
    }

    #[tokio::test]
    async fn summary_reports_unpushed_commits_loudly() {
        let traces_dir = tempfile::tempdir().unwrap();
        let audits_dir = tempfile::tempdir().unwrap();
        let tw = make_trace_writer(traces_dir.path());
        tw.write("evt_foundry", &successful_trace("foundry")).unwrap();
        tw.write("evt_local", &successful_trace("local")).unwrap();
        let shell =
            crate::gateway::fakes::FakeShellGateway::sequence(vec![crate::shell::CommandResult {
                stdout: "4\t0\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            }]);
        let block = GenerateSummary::new(
            tw,
            audits_dir.path().to_str().unwrap().to_string(),
            // `local` has push disabled: commits ahead are by design, not reported.
            push_registry(&[("foundry", true), ("local", false)]),
            Arc::clone(&shell) as _,
        );

        let result = block.execute(&summary_request(&["foundry", "local"])).await.unwrap();

        assert!(
            result.summary.contains("WARNING: 1 project(s) have unpushed commits"),
            "{}",
            result.summary
        );
        let md = std::fs::read_to_string(&result.audit_artifacts[0]).unwrap();
        assert!(md.contains("| foundry | **4 commit(s) ahead of origin/main** |"), "{md}");
        assert!(!md.contains("| local | **"), "push-disabled project not reported: {md}");
        assert_eq!(shell.invocations().len(), 1, "only the push-enabled project is checked");
        assert_eq!(
            shell.invocations()[0].args,
            ["rev-list", "--left-right", "--count", "HEAD...origin/main"]
        );
    }

    #[tokio::test]
    async fn summary_stays_quiet_when_everything_is_pushed() {
        let traces_dir = tempfile::tempdir().unwrap();
        let audits_dir = tempfile::tempdir().unwrap();
        let tw = make_trace_writer(traces_dir.path());
        tw.write("evt_foundry", &successful_trace("foundry")).unwrap();
        let shell =
            crate::gateway::fakes::FakeShellGateway::sequence(vec![crate::shell::CommandResult {
                stdout: "0\t0\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            }]);
        let block = GenerateSummary::new(
            tw,
            audits_dir.path().to_str().unwrap().to_string(),
            push_registry(&[("foundry", true)]),
            shell,
        );

        let result = block.execute(&summary_request(&["foundry"])).await.unwrap();

        assert!(!result.summary.contains("WARNING"), "{}", result.summary);
    }

    #[tokio::test]
    async fn summary_reports_a_check_that_could_not_run() {
        let traces_dir = tempfile::tempdir().unwrap();
        let audits_dir = tempfile::tempdir().unwrap();
        let tw = make_trace_writer(traces_dir.path());
        tw.write("evt_foundry", &successful_trace("foundry")).unwrap();
        let shell = crate::gateway::fakes::FakeShellGateway::failure("fatal: bad revision");
        let block = GenerateSummary::new(
            tw,
            audits_dir.path().to_str().unwrap().to_string(),
            push_registry(&[("foundry", true)]),
            shell,
        );

        let result = block.execute(&summary_request(&["foundry"])).await.unwrap();

        let md = std::fs::read_to_string(&result.audit_artifacts[0]).unwrap();
        assert!(
            md.contains("| foundry | could not check: could not resolve origin/main |"),
            "{md}"
        );
    }

    fn trace_with(project: &str, events: Vec<(EventType, serde_json::Value)>) -> ProcessResult {
        let mut trace = successful_trace(project);
        for (event_type, payload) in events {
            trace
                .events
                .push(Event::new(event_type, project.to_string(), Throttle::Full, payload));
        }
        trace
    }

    #[tokio::test]
    async fn summary_lists_scanner_failures_and_does_not_call_them_clean() {
        let traces_dir = tempfile::tempdir().unwrap();
        let audits_dir = tempfile::tempdir().unwrap();
        let tw = make_trace_writer(traces_dir.path());
        tw.write(
            "evt_bedrock",
            &trace_with(
                "bedrock",
                vec![(
                    EventType::ReleaseTagAudited,
                    serde_json::json!({"project": "bedrock", "cve": "none", "tag": "", "vulnerable": false,
                        "scan_error": "mix deps.audit could not be found"}),
                )],
            ),
        )
        .unwrap();
        let block = summary_block(tw, audits_dir.path());

        let result = block.execute(&summary_request(&["bedrock"])).await.unwrap();

        let md = std::fs::read_to_string(&result.audit_artifacts[0]).unwrap();
        assert!(md.contains("Scanner failures"), "{md}");
        assert!(md.contains("| bedrock | mix deps.audit could not be found |"), "{md}");
        assert!(
            md.contains("| bedrock |  | \u{26a0}\u{fe0f} scanner failed |"),
            "release audit row: {md}"
        );
        assert!(
            result.summary.contains("1 project(s) with scanner failures"),
            "{}",
            result.summary
        );
    }

    #[tokio::test]
    async fn summary_lists_projects_skipped_for_wrong_branch() {
        let traces_dir = tempfile::tempdir().unwrap();
        let audits_dir = tempfile::tempdir().unwrap();
        let tw = make_trace_writer(traces_dir.path());
        tw.write(
            "evt_reaction_new",
            &trace_with(
                "reaction_new",
                vec![(
                    EventType::ProjectValidationCompleted,
                    serde_json::json!({"project": "reaction_new", "status": "error",
                        "reason": "wrong branch: chore/dependency-update-2026-09-24, expected main",
                        "sync_failure": "wrong_branch"}),
                )],
            ),
        )
        .unwrap();
        let block = summary_block(tw, audits_dir.path());

        let result = block.execute(&summary_request(&["reaction_new"])).await.unwrap();

        let md = std::fs::read_to_string(&result.audit_artifacts[0]).unwrap();
        assert!(md.contains("Projects skipped: wrong branch"), "{md}");
        assert!(md.contains(
            "| reaction_new | wrong branch: chore/dependency-update-2026-09-24, expected main |"
        ));
        assert!(
            result.summary.contains("1 project(s) skipped: wrong branch"),
            "{}",
            result.summary
        );
    }

    #[tokio::test]
    async fn includes_failed_projects_in_summary() {
        let traces_dir = tempfile::tempdir().unwrap();
        let audits_dir = tempfile::tempdir().unwrap();
        let tw = make_trace_writer(traces_dir.path());

        tw.write("evt_good", &successful_trace("good-project")).unwrap();
        tw.write("evt_bad", &failed_trace("bad-project")).unwrap();

        let block = summary_block(tw, audits_dir.path());

        let trigger = test_helpers::make_trigger(
            EventType::MajorUpgradesPlanned,
            "_system",
            serde_json::json!({
                "project_trace_ids": {"good-project": "evt_good", "bad-project": "evt_bad"},
                "total_duration_ms": 17000
            }),
        );

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

        let block = summary_block(tw, audits_dir.path());

        let trigger = test_helpers::make_trigger(
            EventType::MajorUpgradesPlanned,
            "_system",
            serde_json::json!({
                "project_trace_ids": {"alpha": "evt_alpha"},
                "skipped_projects": ["gamma"],
                "total_duration_ms": 5000
            }),
        );

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
        let block = summary_block(tw, audits_dir.path());

        let trigger = test_helpers::make_trigger(
            EventType::MajorUpgradesPlanned,
            "_system",
            serde_json::json!({
                "project_trace_ids": {"missing-project": "evt_missing"},
                "total_duration_ms": 0
            }),
        );

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

        let block = summary_block(tw, audits_dir.path());

        let trigger = test_helpers::make_trigger(
            EventType::MajorUpgradesPlanned,
            "_system",
            serde_json::json!({
                "project_trace_ids": {"my-project": "evt_proj"},
                "total_duration_ms": 8000
            }),
        );

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

        let block = summary_block(tw, audits_dir.path());

        let trigger = test_helpers::make_trigger(
            EventType::MajorUpgradesPlanned,
            "_system",
            serde_json::json!({}),
        );

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
    fn extract_project_result_uses_terminal_event_over_block_executions() {
        // When a terminal event with success=false is present, it should win
        // even if all block executions were marked success.  This covers the
        // mojentic-ex scenario where the summary.md incorrectly showed "success"
        // because extract_project_result was reading block-level results.
        let mut trace = successful_trace("mojentic-ex");
        // Inject a failed terminal event (gates failed after a maintain run)
        let terminal = Event::new(
            EventType::ProjectMaintenanceCompleted,
            "mojentic-ex".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "mojentic-ex",
                "success": false,
                "summary": "gates failed after 3 retries",
                "workflow": "maintain",
            }),
        );
        trace.events.push(terminal);

        let result = extract_project_result("mojentic-ex", &trace);
        assert_eq!(result.name, "mojentic-ex");
        assert!(
            matches!(result.status, ProjectStatus::Failed(_)),
            "terminal event success=false must override block-level success"
        );
        if let ProjectStatus::Failed(reason) = &result.status {
            assert!(
                reason.contains("gates failed"),
                "failure reason should come from terminal event summary, got: {reason}"
            );
        }
    }

    #[test]
    fn extract_project_result_terminal_success_overrides_block_failures() {
        // When a terminal event with success=true is present, the trace
        // succeeded even if some intermediate blocks failed (e.g. after a
        // successful retry that followed an initial gate failure).
        let mut trace = failed_trace("retry-project");
        // Inject a success terminal event (retry recovered)
        let terminal = Event::new(
            EventType::ProjectMaintenanceCompleted,
            "retry-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "retry-project",
                "success": true,
                "summary": "all required gates passed",
                "workflow": "maintain",
            }),
        );
        trace.events.push(terminal);

        let result = extract_project_result("retry-project", &trace);
        assert_eq!(
            result.status,
            ProjectStatus::Success,
            "terminal success must win over intermediate block failures"
        );
    }

    #[test]
    fn extract_project_result_benign_decline_counts_as_success() {
        // When the terminal event carries a "triage rejected" / "no correction
        // warranted" summary, the project should be treated as a success —
        // the agent found no real violation to fix.
        let mut trace = failed_trace("benign-project");
        let terminal = Event::new(
            EventType::ProjectMaintenanceCompleted,
            "benign-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "benign-project",
                "success": false,
                "summary": "triage rejected: no correction warranted at this time",
                "workflow": "maintain",
            }),
        );
        trace.events.push(terminal);

        let result = extract_project_result("benign-project", &trace);
        assert_eq!(
            result.status,
            ProjectStatus::Success,
            "benign triage-rejection outcomes must count as success"
        );
    }

    #[test]
    fn extract_project_result_unparseable_terminal_payload_distinguishable_from_genuine_failure() {
        // "success" is a string, not a bool — parse_payload fails. This must
        // not be silently reported with the generic "terminal event reported
        // failure" text used for a genuine failure with an empty summary.
        let mut trace = successful_trace("broken-payload");
        let terminal = Event::new(
            EventType::ProjectMaintenanceCompleted,
            "broken-payload".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "broken-payload",
                "success": "not-a-bool",
                "workflow": "maintain",
            }),
        );
        trace.events.push(terminal);

        let result = extract_project_result("broken-payload", &trace);
        assert!(matches!(result.status, ProjectStatus::Failed(_)));
        if let ProjectStatus::Failed(reason) = &result.status {
            assert!(
                reason.contains("payload unreadable"),
                "reason should name the parse fault, not read as a real failure: {reason}"
            );
            assert_ne!(reason, "terminal event reported failure");
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

    // -- dependency reporting --

    mod dependencies {
        use foundry_sdk::event::{Event, EventType};
        use foundry_sdk::payload::{
            ChainContext, ChangeKind, ClassificationPhase, DependencyBrief,
            DependencyClassification, DependencyUpdatesClassifiedPayload, Ecosystem, LapsedHold,
            OutdatedDependency, PlannedUpdate, UnclassifiedScope, UpdateClass,
        };
        use foundry_sdk::registry::UpdatePolicy;
        use foundry_sdk::task_block::TaskBlock;
        use foundry_sdk::throttle::Throttle;
        use foundry_sdk::trace::ProcessResult;

        use super::super::dependency_report;
        use super::{make_trace_writer, summary_block};

        fn outdated(
            package: &str,
            current: &str,
            non_major: Option<&str>,
            major: Option<&str>,
        ) -> OutdatedDependency {
            OutdatedDependency {
                ecosystem: Ecosystem::Hex,
                manifest: ".".to_string(),
                package: package.to_string(),
                current: current.to_string(),
                requirement: None,
                in_range: non_major.map(str::to_string),
                non_major: non_major.map(str::to_string),
                major: major.map(str::to_string),
                constraint_admits_major: false,
                hold: None,
                advisories: vec![],
            }
        }

        fn planned(package: &str, from: &str, to: &str) -> PlannedUpdate {
            PlannedUpdate {
                ecosystem: Ecosystem::Hex,
                manifest: ".".to_string(),
                package: package.to_string(),
                from: from.to_string(),
                to: to.to_string(),
                class: UpdateClass::Patch,
                change: ChangeKind::Lockfile,
                security: None,
                beyond_policy: false,
                beyond_hold: false,
            }
        }

        fn classified(
            phase: ClassificationPhase,
            outdated: Vec<OutdatedDependency>,
            apply: Vec<PlannedUpdate>,
            policy_set: bool,
        ) -> DependencyUpdatesClassifiedPayload {
            DependencyUpdatesClassifiedPayload {
                project: "bedrock".to_string(),
                phase,
                workflow: Some("maintain".to_string()),
                success: None,
                classification: DependencyClassification {
                    outdated,
                    ..DependencyClassification::default()
                },
                brief: DependencyBrief {
                    policy: UpdatePolicy::Minor,
                    policy_set,
                    apply,
                    held_by_policy: vec![],
                    held_by_hold: vec![],
                    majors: vec![],
                },
                chain: ChainContext::default(),
            }
        }

        #[test]
        fn applied_moves_come_from_comparing_before_and_after() {
            let before = classified(
                ClassificationPhase::Before,
                vec![
                    outdated("jason", "1.4.4", Some("1.4.5"), None),
                    outdated("phoenix", "1.8.1", Some("1.8.3"), Some("2.0.0")),
                    outdated("plug", "1.15.0", Some("1.16.0"), None),
                    outdated("req", "0.7.4", None, Some("0.8.0")),
                ],
                vec![
                    planned("jason", "1.4.4", "1.4.5"),
                    planned("plug", "1.15.0", "1.16.0"),
                ],
                true,
            );
            let after = classified(
                ClassificationPhase::After,
                vec![
                    // phoenix moved within its line; req jumped a major (not briefed).
                    outdated("phoenix", "1.8.3", None, Some("2.0.0")),
                    outdated("plug", "1.15.0", Some("1.16.0"), None),
                ],
                vec![],
                true,
            );

            let r = dependency_report("bedrock", &before, Some(&after));

            let applied: Vec<(&str, &str, &str, UpdateClass, bool)> = r
                .applied
                .as_ref()
                .unwrap()
                .iter()
                .map(|a| (a.package.as_str(), a.from.as_str(), a.to.as_str(), a.class, a.in_brief))
                .collect();
            assert_eq!(
                applied,
                [
                    ("jason", "1.4.4", "1.4.5", UpdateClass::Patch, true),
                    ("phoenix", "1.8.1", "1.8.3", UpdateClass::Patch, false),
                    ("req", "0.7.4", "0.8.0", UpdateClass::Major, false),
                ]
            );
            assert_eq!(r.not_applied.len(), 1);
            assert_eq!(r.not_applied[0].package, "plug");
        }

        #[test]
        fn without_an_after_classification_applied_is_unknown() {
            let before = classified(
                ClassificationPhase::Before,
                vec![outdated("jason", "1.4.4", Some("1.4.5"), None)],
                vec![],
                false,
            );
            let r = dependency_report("bedrock", &before, None);
            assert_eq!(r.applied, None);
            assert!(!r.policy_set);
        }

        #[test]
        fn a_scope_the_after_run_could_not_read_is_not_counted() {
            let before = classified(
                ClassificationPhase::Before,
                vec![outdated("jason", "1.4.4", Some("1.4.5"), None)],
                vec![],
                true,
            );
            let mut after = classified(ClassificationPhase::After, vec![], vec![], true);
            after.classification.unclassified = vec![UnclassifiedScope {
                scope: "hex (.)".to_string(),
                reason: "mix.lock not found".to_string(),
            }];
            let r = dependency_report("bedrock", &before, Some(&after));
            assert_eq!(r.applied, Some(vec![]));
            assert_eq!(r.unclassified.len(), 1);
        }

        fn event(p: &DependencyUpdatesClassifiedPayload) -> Event {
            Event::new(
                EventType::DependencyUpdatesClassified,
                "bedrock".to_string(),
                Throttle::Full,
                serde_json::to_value(p).unwrap(),
            )
        }

        #[tokio::test]
        async fn the_summary_reports_drift_up_top_and_details_per_project() {
            let traces_dir = tempfile::tempdir().unwrap();
            let audits_dir = tempfile::tempdir().unwrap();
            let tw = make_trace_writer(traces_dir.path());
            let before = classified(
                ClassificationPhase::Before,
                vec![
                    outdated("jason", "1.4.4", Some("1.4.5"), None),
                    outdated("req", "0.7.4", None, Some("0.8.0")),
                ],
                vec![planned("jason", "1.4.4", "1.4.5")],
                false,
            );
            let mut after = classified(
                ClassificationPhase::After,
                vec![outdated("req", "0.7.4", None, Some("0.8.0"))],
                vec![],
                false,
            );
            after.classification.vendored = vec!["hex (vendor/roost)".to_string()];
            after.classification.stale_holds = vec![foundry_sdk::payload::StaleHold {
                ecosystem: Ecosystem::Hex,
                manifest: ".".to_string(),
                package: "lv".to_string(),
                locked: "1.2.12".to_string(),
                max: "1.1".to_string(),
                reason: "Roost".to_string(),
            }];
            after.classification.lapsed_holds = vec![LapsedHold {
                package: "phoenix_live_view".to_string(),
                max: "1.1".to_string(),
                reason: "vendored Roost".to_string(),
                expired_on: "2026-09-01".to_string(),
            }];
            let trace = ProcessResult {
                events: vec![event(&before), event(&after)],
                block_executions: vec![],
                total_duration_ms: 1000,
            };
            tw.write("evt_bedrock", &trace).unwrap();
            let block = summary_block(tw, audits_dir.path());
            let trigger = Event::new(
                EventType::MajorUpgradesPlanned,
                "system".to_string(),
                Throttle::Full,
                serde_json::json!({
                    "project_trace_ids": {"bedrock": "evt_bedrock"},
                    "skipped_projects": [],
                    "total_duration_ms": 1000,
                    "per_project_cap": 2, "per_night_cap": 6, "dispatch_enabled": true,
                    "upgrades": [{
                        "project": "bedrock", "ecosystem": "hex", "manifest": ".", "package": "req",
                        "from": "0.7.4", "to": "0.8.0",
                        "objective": "Upgrade req from 0.7.4 to 0.8.0 in bedrock: adapt call sites, keep all gates green.",
                        "command": "foundry task bedrock 'Upgrade req ...'",
                        "status": "proposed", "reason": "policy is minor; majors are proposed, not dispatched"
                    }],
                }),
            );

            let result = block.execute(&trigger).await.unwrap();
            let md = result.raw_output.clone().unwrap();

            let drift = md.find("## Dependency drift").expect("drift section");
            let status = md.find("## Project Status").unwrap();
            assert!(drift < status, "drift sits near the top");
            assert!(
                md.contains("| bedrock | minor (not set) | 1/0/0 | 0 | 0 | 1 proposed | 0 |"),
                "{md}"
            );
            assert!(md.contains("**No update policy set** (behaving as minor): bedrock"));
            assert!(md.contains("**Lapsed holds \u{2014} re-decide:** bedrock: phoenix_live_view"));
            assert!(md.contains("Vendored, updated upstream (1):\n- hex (vendor/roost)"), "{md}");
            assert!(
                md.contains("**Stale holds \u{2014} re-decide:** bedrock: lv (locked 1.2.12 is above cap 1.1)"),
                "{md}"
            );
            assert!(md.contains("### bedrock \u{2014} policy minor (not set)"));
            assert!(md.contains("- [hex .] jason 1.4.4 -> 1.4.5 (patch)"));
            assert!(md.contains("proposed: req 0.7.4 -> 0.8.0"));
            assert!(md.contains("run: `foundry task bedrock 'Upgrade req ...'`"));
            assert!(
                result.summary.contains("1 lapsed dependency hold(s) to re-decide"),
                "{}",
                result.summary
            );
        }

        #[test]
        fn a_review_plan_is_not_a_summary_trigger() {
            let dir = tempfile::tempdir().unwrap();
            let block = summary_block(make_trace_writer(dir.path()), dir.path());
            let review = Event::new(
                EventType::MajorUpgradesPlanned,
                "p".to_string(),
                Throttle::Full,
                serde_json::json!({"review": true}),
            );
            assert!(!block.accepts(&review));
            let nightly = Event::new(
                EventType::MajorUpgradesPlanned,
                "system".to_string(),
                Throttle::Full,
                serde_json::json!({}),
            );
            assert!(block.accepts(&nightly));
        }
    }
}
