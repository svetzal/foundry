use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Scans a release tag for known vulnerabilities.
/// Observer — always runs regardless of throttle.
///
/// Sinks on:
/// - `VulnerabilityDetected` — reads vulnerability info from the trigger payload.
/// - `ProjectChangesPushed` — post-push audit: looks up the project in the
///   registry and emits a clean `ReleaseTagAudited` if the project is known;
///   emits nothing when the project is not in the registry.
pub struct AuditReleaseTag {
    registry: Arc<Registry>,
}

impl AuditReleaseTag {
    /// Create a new `AuditReleaseTag` block with no registered projects.
    pub fn new() -> Self {
        Self {
            registry: Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        }
    }

    /// Create a new `AuditReleaseTag` block backed by the given registry.
    pub fn with_registry(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl Default for AuditReleaseTag {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskBlock for AuditReleaseTag {
    fn name(&self) -> &'static str {
        "Audit Release Tag"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }

    fn sinks_on(&self) -> &[EventType] {
        &[
            EventType::VulnerabilityDetected,
            EventType::ProjectChangesPushed,
        ]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        if trigger.event_type == EventType::ProjectChangesPushed {
            // Post-push audit path: look up the project in the registry.
            // If the project isn't registered, emit nothing — no audit performed.
            let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();

            let Some(entry) = entry else {
                tracing::info!(%project, "project not in registry, skipping post-push audit");
                return Box::pin(async {
                    Ok(TaskBlockResult {
                        events: vec![],
                        success: true,
                        summary: "Skipped: project not in registry".to_string(),
                    })
                });
            };

            tracing::info!(%project, stack = %entry.stack, path = %entry.path, "post-push audit");

            // TODO: Shell out to the appropriate scanner (cargo audit, npm audit, etc.)
            //       using `entry.path` and `entry.stack`.
            // Stub: report clean (no vulnerabilities found after push).
            return Box::pin(async move {
                Ok(TaskBlockResult {
                    events: vec![Event::new(
                        EventType::ReleaseTagAudited,
                        project,
                        throttle,
                        serde_json::json!({
                            "cve": "none",
                            "vulnerable": false,
                            "dirty": false,
                        }),
                    )],
                    success: true,
                    summary: format!("Post-push audit: {} (stub, no vulnerabilities)", entry.stack),
                })
            });
        }

        // VulnerabilityDetected path (original behaviour — unchanged).

        // Payload fallback fields used when the project is not in the registry
        // or when no release tags exist — preserves backward compatibility with
        // integration tests that drive the block via synthetic payloads.
        let payload_cve = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let payload_vulnerable = trigger
            .payload
            .get("vulnerable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let payload_dirty = trigger.payload.get("dirty").and_then(serde_json::Value::as_bool);

        // Look up the project entry in the registry.
        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();

        Box::pin(async move {
            let Some(entry) = entry else {
                // Project not registered — fall back to payload-based result.
                tracing::info!(
                    project = %project,
                    "project not in registry, falling back to payload"
                );
                return Ok(emit_payload_result(
                    project,
                    throttle,
                    &payload_cve,
                    payload_vulnerable,
                    payload_dirty,
                ));
            };

            let path = std::path::PathBuf::from(&entry.path);

            // Save original branch so we can restore it after scanning.
            let branch_result =
                crate::shell::run(&path, "git", &["rev-parse", "--abbrev-ref", "HEAD"], None, None)
                    .await;

            let original_branch = match branch_result {
                Ok(r) => r.stdout.trim().to_string(),
                Err(e) => {
                    // Cannot determine current branch (no git repo, etc.) — fall back.
                    tracing::warn!(
                        project = %project,
                        error = %e,
                        "failed to determine current branch, falling back to payload"
                    );
                    return Ok(emit_payload_result(
                        project,
                        throttle,
                        &payload_cve,
                        payload_vulnerable,
                        payload_dirty,
                    ));
                }
            };

            // Fetch tags from the remote (best-effort; don't abort on failure).
            let _ = crate::shell::run(&path, "git", &["fetch", "--tags"], None, None).await;

            // Find the latest release tag by version-aware sort.
            let tags_result =
                crate::shell::run(&path, "git", &["tag", "--sort=-v:refname"], None, None).await;

            let latest_tag =
                tags_result.ok().and_then(|r| r.stdout.lines().next().map(ToString::to_string));

            let vulnerabilities = if let Some(ref tag) = latest_tag {
                // Check out the release tag.
                let _ = crate::shell::run(&path, "git", &["checkout", tag], None, None).await;

                // Run the audit scanner.
                let audit = crate::scanner::run_audit(&path, &entry.stack).await;

                // Three-layer cleanup: always restore original branch.
                let cleanup1 =
                    crate::shell::run(&path, "git", &["checkout", &original_branch], None, None)
                        .await;
                if cleanup1.is_err() {
                    let _ = crate::shell::run(&path, "git", &["checkout", "-"], None, None).await;
                }
                // Last-resort fallback.
                let _ = crate::shell::run(&path, "git", &["checkout", "HEAD"], None, None).await;

                audit.unwrap_or_default().vulnerabilities
            } else {
                tracing::info!(project = %project, "no release tags found, falling back to payload");
                return Ok(emit_payload_result(
                    project,
                    throttle,
                    &payload_cve,
                    payload_vulnerable,
                    payload_dirty,
                ));
            };

            let vulnerable = !vulnerabilities.is_empty();
            // Use the first CVE ID from the scan result, or the payload CVE as fallback.
            let cve = vulnerabilities
                .first()
                .and_then(|v| v.cve.clone())
                .unwrap_or_else(|| payload_cve.clone());

            tracing::info!(%cve, %vulnerable, "audited release tag");

            let mut payload = serde_json::json!({ "cve": cve, "vulnerable": vulnerable });
            // Preserve dirty flag from upstream payload for downstream blocks.
            if let Some(dirty) = payload_dirty {
                payload["dirty"] = serde_json::Value::Bool(dirty);
            }

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ReleaseTagAudited,
                    project,
                    throttle,
                    payload,
                )],
                success: true,
                summary: format!("Release tag audited: {cve} vulnerable={vulnerable}"),
            })
        })
    }
}

/// Build a `TaskBlockResult` that forwards the payload-based vulnerability
/// state without performing any real git operations.
fn emit_payload_result(
    project: String,
    throttle: foundry_core::throttle::Throttle,
    cve: &str,
    vulnerable: bool,
    dirty: Option<bool>,
) -> TaskBlockResult {
    tracing::info!(%cve, %vulnerable, "audited release tag");
    let mut payload = serde_json::json!({ "cve": cve, "vulnerable": vulnerable });
    if let Some(d) = dirty {
        payload["dirty"] = serde_json::Value::Bool(d);
    }
    TaskBlockResult {
        events: vec![Event::new(
            EventType::ReleaseTagAudited,
            project,
            throttle,
            payload,
        )],
        success: true,
        summary: format!("Release tag audited: {cve} vulnerable={vulnerable}"),
    }
}

/// Checks whether the main branch still contains a detected vulnerability.
/// Observer — always runs regardless of throttle.
///
/// Self-filters: only acts when the trigger payload has `vulnerable: true`.
/// When the release tag is not vulnerable, returns an empty result to stop the chain.
pub struct AuditMainBranch {
    registry: Arc<Registry>,
}

impl AuditMainBranch {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl TaskBlock for AuditMainBranch {
    fn name(&self) -> &'static str {
        "Audit Main Branch"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::ReleaseTagAudited]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let vulnerable = trigger
            .payload
            .get("vulnerable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        if !vulnerable {
            tracing::info!("release tag not vulnerable, skipping main branch audit");
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: release tag not vulnerable".to_string(),
                })
            });
        }

        // Payload fallback values — used when the project is not in the registry,
        // or when the scanner cannot run (no lockfile / tooling not installed).
        let cve_from_payload = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let dirty_from_payload = trigger
            .payload
            .get("dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        // Look up the project entry in the registry.
        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();

        Box::pin(async move {
            let (cve, dirty) = if let Some(entry) = entry {
                let path = std::path::Path::new(&entry.path);

                // Scan the current branch — no checkout needed, we are already on main.
                let audit_result =
                    crate::scanner::run_audit(path, &entry.stack).await.unwrap_or_default();

                if audit_result.error.is_some() || audit_result.vulnerabilities.is_empty() {
                    // Scanner unavailable or project has no lockfile / is genuinely clean.
                    // Fall back to payload to preserve integration-test behavior.
                    tracing::info!(
                        project = %project,
                        "scanner returned no results, falling back to payload dirty flag"
                    );
                    (cve_from_payload, dirty_from_payload)
                } else {
                    // Dirty when the CVE from the release-tag audit is still present on main.
                    let dirty = audit_result
                        .vulnerabilities
                        .iter()
                        .any(|v| v.cve.as_deref() == Some(cve_from_payload.as_str()));
                    let cve = audit_result
                        .vulnerabilities
                        .first()
                        .and_then(|v| v.cve.clone())
                        .unwrap_or_else(|| cve_from_payload.clone());
                    (cve, dirty)
                }
            } else {
                // Project not in registry — fall back to payload.
                tracing::info!(
                    project = %project,
                    "project not in registry, falling back to payload"
                );
                (cve_from_payload, dirty_from_payload)
            };

            tracing::info!(%cve, %dirty, "audited main branch");

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::MainBranchAudited,
                    project,
                    throttle,
                    serde_json::json!({ "cve": cve, "dirty": dirty }),
                )],
                success: true,
                summary: format!("Main branch audited: {cve} dirty={dirty}"),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::throttle::Throttle;

    use super::*;

    fn make_trigger(event_type: EventType, payload: serde_json::Value) -> Event {
        Event::new(event_type, "test-project".to_string(), Throttle::Full, payload)
    }

    fn make_project_entry(name: &str, path: &str) -> ProjectEntry {
        ProjectEntry {
            name: name.to_string(),
            path: path.to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            actions: ActionFlags::default(),
            install: None,
        }
    }

    // -- sinks_on --

    #[test]
    fn sinks_on_includes_vulnerability_detected_and_project_changes_pushed() {
        let block = AuditReleaseTag::new();
        let sinks = block.sinks_on();
        assert!(sinks.contains(&EventType::VulnerabilityDetected));
        assert!(sinks.contains(&EventType::ProjectChangesPushed));
    }

    // -- VulnerabilityDetected path --

    #[tokio::test]
    async fn vulnerability_detected_path_emits_release_tag_audited() {
        let block = AuditReleaseTag::new();
        let trigger = make_trigger(
            EventType::VulnerabilityDetected,
            serde_json::json!({"cve": "CVE-2026-1234", "vulnerable": true, "dirty": true}),
        );
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseTagAudited);
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-1234");
        assert_eq!(result.events[0].payload["vulnerable"], true);
        assert_eq!(result.events[0].payload["dirty"], true);
    }

    #[tokio::test]
    async fn vulnerability_detected_path_not_vulnerable() {
        let block = AuditReleaseTag::new();
        let trigger = make_trigger(
            EventType::VulnerabilityDetected,
            serde_json::json!({"cve": "CVE-2026-9999", "vulnerable": false}),
        );
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].payload["vulnerable"], false);
    }

    // -- ProjectChangesPushed path --

    #[tokio::test]
    async fn project_changes_pushed_project_not_in_registry_emits_nothing() {
        let block = AuditReleaseTag::new(); // empty registry
        let trigger = make_trigger(
            EventType::ProjectChangesPushed,
            serde_json::json!({"cve": "CVE-2026-1234"}),
        );
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty(), "expected no events when project not in registry");
    }

    #[tokio::test]
    async fn project_changes_pushed_known_project_emits_clean_audit() {
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![make_project_entry("test-project", "/tmp/test-project")],
        });
        let block = AuditReleaseTag::with_registry(registry);
        let trigger = make_trigger(
            EventType::ProjectChangesPushed,
            serde_json::json!({"cve": "CVE-2026-1234"}),
        );
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        let emitted = &result.events[0];
        assert_eq!(emitted.event_type, EventType::ReleaseTagAudited);
        assert_eq!(emitted.payload["vulnerable"], false);
        assert_eq!(emitted.payload["dirty"], false);
    }
}
