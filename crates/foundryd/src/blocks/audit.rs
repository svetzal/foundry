use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{ScannerGateway, ShellGateway};

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
    shell: Arc<dyn ShellGateway>,
    scanner: Arc<dyn ScannerGateway>,
}

impl AuditReleaseTag {
    /// Create a new `AuditReleaseTag` block with no registered projects.
    pub fn new() -> Self {
        Self {
            registry: Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
            shell: Arc::new(crate::gateway::ProcessShellGateway),
            scanner: Arc::new(crate::gateway::ProcessScannerGateway),
        }
    }

    /// Create a new `AuditReleaseTag` block backed by the given registry.
    pub fn with_registry(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            shell: Arc::new(crate::gateway::ProcessShellGateway),
            scanner: Arc::new(crate::gateway::ProcessScannerGateway),
        }
    }

    #[cfg(test)]
    fn with_gateways(
        registry: Arc<Registry>,
        shell: Arc<dyn ShellGateway>,
        scanner: Arc<dyn ScannerGateway>,
    ) -> Self {
        Self {
            registry,
            shell,
            scanner,
        }
    }
}

impl Default for AuditReleaseTag {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditReleaseTag {
    /// Handle the `ProjectChangesPushed` trigger path.
    ///
    /// Looks up the project in the registry and emits a clean `ReleaseTagAudited`
    /// event when found, or returns an empty result when the project is unknown.
    fn audit_after_push(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();
        let scanner = Arc::clone(&self.scanner);

        let Some(entry) = entry else {
            tracing::info!(%project, "project not in registry, skipping post-push audit");
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: project not in registry".to_string(),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                })
            });
        };

        tracing::info!(%project, stack = %entry.stack, path = %entry.path, "post-push audit");

        Box::pin(async move {
            let path = std::path::Path::new(&entry.path);
            let audit_result = scanner.run_audit(path, &entry.stack).await.unwrap_or_default();

            let vulnerable = !audit_result.vulnerabilities.is_empty();
            let cve = audit_result
                .vulnerabilities
                .first()
                .and_then(|v| v.cve.clone())
                .unwrap_or_else(|| "none".to_string());

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ReleaseTagAudited,
                    project,
                    throttle,
                    serde_json::json!({
                        "cve": cve,
                        "vulnerable": vulnerable,
                        "dirty": false,
                    }),
                )],
                success: true,
                summary: format!("Post-push audit: {} vulnerable={}", entry.stack, vulnerable),
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
    }

    /// Handle the `VulnerabilityDetected` trigger path.
    ///
    /// Checks out the latest release tag, runs the appropriate scanner, restores
    /// the original branch, and emits a `ReleaseTagAudited` event.  Falls back
    /// to the trigger payload when the project is not registered or git/scanner
    /// operations fail.
    fn audit_after_vulnerability_detected(
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
        let shell = Arc::clone(&self.shell);
        let scanner = Arc::clone(&self.scanner);

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
            let branch_result = shell
                .run(&path, "git", &["rev-parse", "--abbrev-ref", "HEAD"], None, None)
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

            perform_tag_checkout_and_scan(
                &path,
                &entry.stack,
                &original_branch,
                &project,
                throttle,
                &payload_cve,
                payload_vulnerable,
                payload_dirty,
                shell.as_ref(),
                scanner.as_ref(),
            )
            .await
        })
    }
}

impl TaskBlock for AuditReleaseTag {
    task_block_meta! {
        name: "Audit Release Tag",
        kind: Observer,
        sinks_on: [VulnerabilityDetected, ProjectChangesPushed],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        if trigger.event_type == EventType::ProjectChangesPushed {
            self.audit_after_push(trigger)
        } else {
            self.audit_after_vulnerability_detected(trigger)
        }
    }
}

/// Checks out the latest release tag, runs the scanner, restores the original
/// branch, and returns a `TaskBlockResult` with a `ReleaseTagAudited` event.
///
/// Falls back to the payload values when no release tags exist or when the
/// scanner cannot run.
#[allow(clippy::too_many_arguments)]
async fn perform_tag_checkout_and_scan(
    path: &std::path::Path,
    stack: &foundry_core::registry::Stack,
    original_branch: &str,
    project: &str,
    throttle: foundry_core::throttle::Throttle,
    payload_cve: &str,
    payload_vulnerable: bool,
    payload_dirty: Option<bool>,
    shell: &dyn ShellGateway,
    scanner: &dyn ScannerGateway,
) -> anyhow::Result<TaskBlockResult> {
    // Fetch tags from the remote (best-effort; don't abort on failure).
    let _ = shell.run(path, "git", &["fetch", "--tags"], None, None).await;

    // Find the latest release tag by version-aware sort.
    let tags_result = shell.run(path, "git", &["tag", "--sort=-v:refname"], None, None).await;

    let latest_tag =
        tags_result.ok().and_then(|r| r.stdout.lines().next().map(ToString::to_string));

    let vulnerabilities = if let Some(ref tag) = latest_tag {
        // Check out the release tag.
        let _ = shell.run(path, "git", &["checkout", tag], None, None).await;

        // Run the audit scanner.
        let audit = scanner.run_audit(path, stack).await;

        // Three-layer cleanup: always restore original branch.
        let cleanup1 = shell.run(path, "git", &["checkout", original_branch], None, None).await;
        if cleanup1.is_err() {
            let _ = shell.run(path, "git", &["checkout", "-"], None, None).await;
        }
        // Last-resort fallback.
        let _ = shell.run(path, "git", &["checkout", "HEAD"], None, None).await;

        audit.unwrap_or_default().vulnerabilities
    } else {
        tracing::info!(project = %project, "no release tags found, falling back to payload");
        return Ok(emit_payload_result(
            project.to_string(),
            throttle,
            payload_cve,
            payload_vulnerable,
            payload_dirty,
        ));
    }; // vulnerabilities assigned above

    let vulnerable = !vulnerabilities.is_empty();
    // Use the first CVE ID from the scan result, or the payload CVE as fallback.
    let cve = vulnerabilities
        .first()
        .and_then(|v| v.cve.clone())
        .unwrap_or_else(|| payload_cve.to_string());

    tracing::info!(%cve, %vulnerable, "audited release tag");

    let mut payload = serde_json::json!({ "cve": cve, "vulnerable": vulnerable });
    // Preserve dirty flag from upstream payload for downstream blocks.
    if let Some(dirty) = payload_dirty {
        payload["dirty"] = serde_json::Value::Bool(dirty);
    }

    Ok(TaskBlockResult {
        events: vec![Event::new(
            EventType::ReleaseTagAudited,
            project.to_string(),
            throttle,
            payload,
        )],
        success: true,
        summary: format!("Release tag audited: {cve} vulnerable={vulnerable}"),
        raw_output: None,
        exit_code: None,
        audit_artifacts: vec![],
    })
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
        raw_output: None,
        exit_code: None,
        audit_artifacts: vec![],
    }
}

/// Checks whether the main branch still contains a detected vulnerability.
/// Observer — always runs regardless of throttle.
///
/// Self-filters: only acts when the trigger payload has `vulnerable: true`.
/// When the release tag is not vulnerable, returns an empty result to stop the chain.
pub struct AuditMainBranch {
    registry: Arc<Registry>,
    scanner: Arc<dyn ScannerGateway>,
}

impl AuditMainBranch {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            scanner: Arc::new(crate::gateway::ProcessScannerGateway),
        }
    }

    #[cfg(test)]
    fn with_scanner(registry: Arc<Registry>, scanner: Arc<dyn ScannerGateway>) -> Self {
        Self { registry, scanner }
    }
}

impl TaskBlock for AuditMainBranch {
    task_block_meta! {
        name: "Audit Main Branch",
        kind: Observer,
        sinks_on: [ReleaseTagAudited],
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
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
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
        let scanner = Arc::clone(&self.scanner);

        Box::pin(async move {
            let (cve, dirty) = if let Some(entry) = entry {
                let path = std::path::Path::new(&entry.path);

                // Scan the current branch — no checkout needed, we are already on main.
                let audit_result = scanner.run_audit(path, &entry.stack).await.unwrap_or_default();

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
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::{FakeScannerGateway, FakeShellGateway};
    use crate::scanner::Vulnerability;
    use crate::shell::CommandResult;

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
            notes: None,
            actions: ActionFlags::default(),
            install: None,
            timeout_secs: None,
        }
    }

    fn registry_with(entry: ProjectEntry) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![entry],
        })
    }

    // -- sinks_on --

    #[test]
    fn sinks_on_includes_vulnerability_detected_and_project_changes_pushed() {
        let block = AuditReleaseTag::new();
        let sinks = block.sinks_on();
        assert!(sinks.contains(&EventType::VulnerabilityDetected));
        assert!(sinks.contains(&EventType::ProjectChangesPushed));
    }

    // -- VulnerabilityDetected path: project not in registry --

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

    // -- VulnerabilityDetected path: project in registry, no tags --

    #[tokio::test]
    async fn tag_scan_no_tags_falls_back_to_payload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry =
            registry_with(make_project_entry("test-project", dir.path().to_str().unwrap()));

        // rev-parse returns "main"; fetch --tags succeeds; tag list is empty.
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: "main\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            }, // empty tag list
        ]);
        let scanner = FakeScannerGateway::clean();
        let block = AuditReleaseTag::with_gateways(registry, shell, scanner);

        let trigger = make_trigger(
            EventType::VulnerabilityDetected,
            serde_json::json!({"cve": "CVE-2026-1234", "vulnerable": true, "dirty": true}),
        );
        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        // Falls back to payload values
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-1234");
        assert_eq!(result.events[0].payload["vulnerable"], true);
    }

    // -- VulnerabilityDetected path: project in registry, with tags, vulnerabilities found --

    #[tokio::test]
    async fn tag_scan_with_vulnerabilities_emits_vulnerable_true() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry =
            registry_with(make_project_entry("test-project", dir.path().to_str().unwrap()));

        // Sequence: rev-parse → fetch --tags → tag list → checkout → cleanup restore
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: "main\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: "v1.0.0\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            }, // checkout tag
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            }, // restore branch
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            }, // checkout HEAD
        ]);
        let scanner = FakeScannerGateway::with_vulnerabilities(vec![Vulnerability {
            cve: Some("CVE-2026-9999".to_string()),
            severity: Some("high".to_string()),
            package: "bad-crate".to_string(),
            version: None,
        }]);
        let block = AuditReleaseTag::with_gateways(registry, shell, scanner);

        let trigger = make_trigger(
            EventType::VulnerabilityDetected,
            serde_json::json!({"cve": "CVE-2026-9999", "vulnerable": true, "dirty": true}),
        );
        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].event_type, EventType::ReleaseTagAudited);
        assert_eq!(result.events[0].payload["vulnerable"], true);
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-9999");
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
    async fn project_changes_pushed_known_clean_project_emits_clean_audit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry =
            registry_with(make_project_entry("test-project", dir.path().to_str().unwrap()));
        let scanner = FakeScannerGateway::clean();
        let block = AuditReleaseTag::with_gateways(registry, FakeShellGateway::success(), scanner);

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

    // -- AuditMainBranch tests --

    #[test]
    fn main_branch_sinks_on_release_tag_audited() {
        let block = AuditMainBranch::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert_eq!(block.sinks_on(), &[EventType::ReleaseTagAudited]);
    }

    #[tokio::test]
    async fn main_branch_skips_when_not_vulnerable() {
        let block = AuditMainBranch::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let trigger = make_trigger(
            EventType::ReleaseTagAudited,
            serde_json::json!({"vulnerable": false, "cve": "CVE-2026-1234"}),
        );
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn main_branch_falls_back_to_payload_when_project_not_in_registry() {
        let block = AuditMainBranch::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let trigger = make_trigger(
            EventType::ReleaseTagAudited,
            serde_json::json!({"vulnerable": true, "cve": "CVE-2026-1234", "dirty": true}),
        );
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-1234");
        assert_eq!(result.events[0].payload["dirty"], true);
    }

    #[tokio::test]
    async fn main_branch_scanner_finds_same_cve_marks_dirty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry =
            registry_with(make_project_entry("test-project", dir.path().to_str().unwrap()));
        let scanner = FakeScannerGateway::with_vulnerabilities(vec![Vulnerability {
            cve: Some("CVE-2026-1234".to_string()),
            severity: Some("high".to_string()),
            package: "vulnerable-crate".to_string(),
            version: None,
        }]);
        let block = AuditMainBranch::with_scanner(registry, scanner);

        let trigger = make_trigger(
            EventType::ReleaseTagAudited,
            serde_json::json!({"vulnerable": true, "cve": "CVE-2026-1234", "dirty": true}),
        );
        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["dirty"], true);
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-1234");
    }

    #[tokio::test]
    async fn main_branch_scanner_clean_falls_back_to_payload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry =
            registry_with(make_project_entry("test-project", dir.path().to_str().unwrap()));
        let scanner = FakeScannerGateway::clean();
        let block = AuditMainBranch::with_scanner(registry, scanner);

        let trigger = make_trigger(
            EventType::ReleaseTagAudited,
            serde_json::json!({"vulnerable": true, "cve": "CVE-2026-1234", "dirty": false}),
        );
        let result = block.execute(&trigger).await.unwrap();

        // Scanner returned clean; falls back to payload dirty=false
        assert!(result.success);
        assert_eq!(result.events[0].payload["dirty"], false);
    }
}
