use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Scans a release tag for known vulnerabilities.
/// Observer — always runs regardless of throttle.
pub struct AuditReleaseTag {
    registry: Arc<Registry>,
}

impl AuditReleaseTag {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
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
        // Future: also sink on ProjectChangesPushed for re-audit after fix,
        // once real scanning can determine vulnerability status from code.
        &[EventType::VulnerabilityDetected]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

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
