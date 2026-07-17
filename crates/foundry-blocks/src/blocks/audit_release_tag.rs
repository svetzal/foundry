use std::pin::Pin;
use std::sync::{Arc, RwLock};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{ReleaseTagAuditedPayload, VulnerabilityDetectedPayload};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{ScannerGateway, ShellGateway};

use super::single_event_result;

/// Scans a release tag for known vulnerabilities.
/// Observer — always runs regardless of throttle.
///
/// Sinks on:
/// - `VulnerabilityDetected` — reads vulnerability info from the trigger payload.
/// - `ProjectChangesPushed` — post-push audit: looks up the project in the
///   registry and emits a clean `ReleaseTagAudited` if the project is known;
///   emits nothing when the project is not in the registry.
pub struct AuditReleaseTag {
    registry: Arc<RwLock<Registry>>,
    shell: Arc<dyn ShellGateway>,
    scanner: Arc<dyn ScannerGateway>,
}

impl AuditReleaseTag {
    /// Create a new `AuditReleaseTag` block with no registered projects.
    pub fn new() -> Self {
        Self {
            registry: Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
            shell: Arc::new(crate::gateway::ProcessShellGateway),
            scanner: Arc::new(crate::gateway::ProcessScannerGateway),
        }
    }

    /// Create a new `AuditReleaseTag` block backed by the given registry.
    pub fn with_registry(registry: Arc<RwLock<Registry>>) -> Self {
        Self {
            registry,
            shell: Arc::new(crate::gateway::ProcessShellGateway),
            scanner: Arc::new(crate::gateway::ProcessScannerGateway),
        }
    }

    #[cfg(test)]
    fn with_gateways(
        registry: Arc<RwLock<Registry>>,
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

        let entry = match super::read_registry(&self.registry) {
            Ok(guard) => guard.find_project(&project).cloned(),
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        let scanner = Arc::clone(&self.scanner);

        // Domain skip: when the project is not in the registry this block has no
        // audit target, so it returns an empty result to halt the chain.  This
        // is not a routing guard — the block legitimately sinks on every
        // ProjectChangesPushed event and decides at runtime whether to act.
        // Per the accepts() convention, this stays in execute() because "project
        // unknown" is a meaningful runtime condition, not a payload filter.
        let Some(entry) = entry else {
            tracing::info!(%project, "project not in registry, skipping post-push audit");
            return skip!("Skipped: project not in registry");
        };

        tracing::info!(%project, stack = %entry.stack, path = %entry.path, "post-push audit");

        Box::pin(async move {
            let path = std::path::Path::new(&entry.path);
            let audit_result =
                match crate::scanner::audit_outcome(scanner.run_audit(path, &entry.stack).await) {
                    Err(msg) => {
                        tracing::warn!(
                            project = %project,
                            error = %msg,
                            "post-push audit scanner failed"
                        );
                        let event_payload = Event::serialize_payload(&ReleaseTagAuditedPayload {
                            project: project.clone(),
                            cve: "none".to_string(),
                            tag: String::new(),
                            vulnerable: false,
                            dirty: Some(false),
                            scan_error: Some(msg),
                        })
                        .expect("ReleaseTagAuditedPayload is infallibly serializable");
                        return Ok(single_event_result(
                            "Post-push audit: scanner failed".to_string(),
                            EventType::ReleaseTagAudited,
                            project,
                            throttle,
                            event_payload,
                        ));
                    }
                    Ok(result) => result,
                };
            let reported =
                crate::scanner::filter_audit_exceptions(&audit_result, &entry.audit_exceptions);
            let vulnerable = !reported.is_empty();
            let cve = reported
                .first()
                .and_then(|v| v.cve.clone())
                .unwrap_or_else(|| "none".to_string());

            let event_payload = Event::serialize_payload(&ReleaseTagAuditedPayload {
                project: project.clone(),
                cve: cve.clone(),
                tag: String::new(),
                vulnerable,
                dirty: Some(false),
                scan_error: None,
            })
            .expect("ReleaseTagAuditedPayload is infallibly serializable");
            Ok(single_event_result(
                format!("Post-push audit: {} vulnerable={}", entry.stack, vulnerable),
                EventType::ReleaseTagAudited,
                project,
                throttle,
                event_payload,
            ))
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

        let p = parse_payload!(trigger, VulnerabilityDetectedPayload);
        let payload_cve = p.cve;
        let payload_vulnerable = p.vulnerable;
        let payload_dirty = Some(p.dirty);

        // Look up the project entry in the registry.
        let entry = match super::read_registry(&self.registry) {
            Ok(guard) => guard.find_project(&project).cloned(),
            Err(e) => return Box::pin(async move { Err(e) }),
        };
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
                    None,
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
                        None,
                    ));
                }
            };

            perform_tag_checkout_and_scan(
                &path,
                &entry.stack,
                &original_branch,
                &project,
                throttle,
                PayloadFallback {
                    cve: payload_cve,
                    vulnerable: payload_vulnerable,
                    dirty: payload_dirty,
                },
                ScanGateways {
                    shell: shell.as_ref(),
                    scanner: scanner.as_ref(),
                },
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

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        if trigger.event_type == EventType::ProjectChangesPushed {
            self.audit_after_push(trigger)
        } else {
            self.audit_after_vulnerability_detected(trigger)
        }
    }
}

struct PayloadFallback {
    cve: String,
    vulnerable: bool,
    dirty: Option<bool>,
}

struct ScanGateways<'a> {
    shell: &'a dyn ShellGateway,
    scanner: &'a dyn ScannerGateway,
}

/// Restores the working tree to `original_branch` after a tag checkout.
///
/// This is the single source of truth for the three-layer git branch-recovery
/// strategy used by [`perform_tag_checkout_and_scan`]:
///
/// 1. Try `git checkout <original_branch>` (nominal path).
/// 2. On failure, fall back to `git checkout -` (previous HEAD shorthand).
/// 3. Always run `git checkout HEAD` as a last-resort detach-guard.
///
/// All operations are best-effort; errors are silently discarded because the
/// caller has already collected its scan result and any failure here is
/// non-fatal.
async fn restore_original_branch(
    shell: &dyn ShellGateway,
    path: &std::path::Path,
    original_branch: &str,
) {
    let cleanup1 = shell.run(path, "git", &["checkout", original_branch], None, None).await;
    if cleanup1.is_err() {
        let _ = shell.run(path, "git", &["checkout", "-"], None, None).await;
    }
    let _ = shell.run(path, "git", &["checkout", "HEAD"], None, None).await;
}

/// Checks out the latest release tag, runs the scanner, restores the original
/// branch, and returns a `TaskBlockResult` with a `ReleaseTagAudited` event.
///
/// Falls back to the payload values when no release tags exist or when the
/// scanner cannot run.
async fn perform_tag_checkout_and_scan(
    path: &std::path::Path,
    stack: &foundry_sdk::registry::Stack,
    original_branch: &str,
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    fallback: PayloadFallback,
    gateways: ScanGateways<'_>,
) -> anyhow::Result<TaskBlockResult> {
    let ScanGateways { shell, scanner } = gateways;
    // Fetch tags from the remote (best-effort; don't abort on failure).
    let _ = shell.run(path, "git", &["fetch", "--tags"], None, None).await;

    // Find the latest release tag by version-aware sort.
    let tags_result = shell.run(path, "git", &["tag", "--sort=-v:refname"], None, None).await;

    let latest_tag =
        tags_result.ok().and_then(|r| r.stdout.lines().next().map(ToString::to_string));

    let vulnerabilities = if let Some(ref tag) = latest_tag {
        // Check out the release tag.
        let checkout_result = shell.run(path, "git", &["checkout", tag], None, None).await;
        let checkout_success = checkout_result.as_ref().is_ok_and(|r| r.success);
        let checkout_stderr = match &checkout_result {
            Ok(r) => r.stderr.clone(),
            Err(e) => e.to_string(),
        };
        if !checkout_success {
            tracing::warn!(
                project = %project,
                tag = %tag,
                stderr = %checkout_stderr,
                "git checkout tag failed"
            );
            // Run cleanup even though we may not have moved.
            restore_original_branch(shell, path, original_branch).await;
            return Ok(emit_payload_result(
                project.to_string(),
                throttle,
                &fallback.cve,
                fallback.vulnerable,
                fallback.dirty,
                Some(format!("git checkout {tag} failed: {checkout_stderr}")),
            ));
        }

        // Run the audit scanner.
        let audit = scanner.run_audit(path, stack).await;

        // Always restore the original branch after scanning.
        restore_original_branch(shell, path, original_branch).await;

        match crate::scanner::audit_outcome(audit) {
            Err(msg) => {
                tracing::warn!(
                    project = %project,
                    error = %msg,
                    "release tag scanner failed"
                );
                return Ok(emit_payload_result(
                    project.to_string(),
                    throttle,
                    &fallback.cve,
                    fallback.vulnerable,
                    fallback.dirty,
                    Some(msg),
                ));
            }
            Ok(result) => result.vulnerabilities,
        }
    } else {
        tracing::info!(project = %project, "no release tags found, falling back to payload");
        return Ok(emit_payload_result(
            project.to_string(),
            throttle,
            &fallback.cve,
            fallback.vulnerable,
            fallback.dirty,
            None,
        ));
    }; // vulnerabilities assigned above

    let vulnerable = !vulnerabilities.is_empty();
    // Use the first CVE ID from the scan result, or the payload CVE as fallback.
    let cve = vulnerabilities
        .first()
        .and_then(|v| v.cve.clone())
        .unwrap_or_else(|| fallback.cve.clone());

    tracing::info!(%cve, %vulnerable, "audited release tag");

    let event_payload = Event::serialize_payload(&ReleaseTagAuditedPayload {
        project: project.to_string(),
        cve: cve.clone(),
        tag: String::new(),
        vulnerable,
        dirty: fallback.dirty,
        scan_error: None,
    })
    .expect("ReleaseTagAuditedPayload is infallibly serializable");

    Ok(single_event_result(
        format!("Release tag audited: {cve} vulnerable={vulnerable}"),
        EventType::ReleaseTagAudited,
        project.to_string(),
        throttle,
        event_payload,
    ))
}

/// Build a `TaskBlockResult` that forwards the payload-based vulnerability
/// state without performing any real git operations.
///
/// When `scan_error` is `Some`, the caller knows the scan did not run;
/// `vulnerable` is the upstream value rather than a fresh scan result.
fn emit_payload_result(
    project: String,
    throttle: foundry_sdk::throttle::Throttle,
    cve: &str,
    vulnerable: bool,
    dirty: Option<bool>,
    scan_error: Option<String>,
) -> TaskBlockResult {
    tracing::info!(%cve, %vulnerable, "audited release tag");
    let event_payload = Event::serialize_payload(&ReleaseTagAuditedPayload {
        project: project.clone(),
        cve: cve.to_string(),
        tag: String::new(),
        vulnerable,
        dirty,
        scan_error,
    })
    .expect("ReleaseTagAuditedPayload is infallibly serializable");
    single_event_result(
        format!("Release tag audited: {cve} vulnerable={vulnerable}"),
        EventType::ReleaseTagAudited,
        project,
        throttle,
        event_payload,
    )
}

#[cfg(test)]
mod tests {
    use foundry_sdk::event::EventType;

    use crate::gateway::fakes::{FakeScannerGateway, FakeShellGateway};
    use crate::scanner::Vulnerability;
    use crate::shell::CommandResult;

    use super::super::test_helpers;
    use super::*;

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
        let trigger = test_helpers::make_trigger(
            EventType::VulnerabilityDetected,
            "test-project",
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
        let trigger = test_helpers::make_trigger(
            EventType::VulnerabilityDetected,
            "test-project",
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
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry(
            "test-project",
            dir.path().to_str().unwrap(),
        ));

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

        let trigger = test_helpers::make_trigger(
            EventType::VulnerabilityDetected,
            "test-project",
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
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry(
            "test-project",
            dir.path().to_str().unwrap(),
        ));

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
            fix_version: None,
            fix_package: None,
        }]);
        let block = AuditReleaseTag::with_gateways(registry, shell, scanner);

        let trigger = test_helpers::make_trigger(
            EventType::VulnerabilityDetected,
            "test-project",
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
        let trigger = test_helpers::make_trigger(
            EventType::ProjectChangesPushed,
            "test-project",
            serde_json::json!({"cve": "CVE-2026-1234"}),
        );
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty(), "expected no events when project not in registry");
    }

    #[tokio::test]
    async fn project_changes_pushed_known_clean_project_emits_clean_audit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry(
            "test-project",
            dir.path().to_str().unwrap(),
        ));
        let scanner = FakeScannerGateway::clean();
        let block = AuditReleaseTag::with_gateways(registry, FakeShellGateway::success(), scanner);

        let trigger = test_helpers::make_trigger(
            EventType::ProjectChangesPushed,
            "test-project",
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

    #[tokio::test]
    async fn project_changes_pushed_scanner_error_emits_scan_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry(
            "test-project",
            dir.path().to_str().unwrap(),
        ));
        let scanner = FakeScannerGateway::with_error("audit tool not installed");
        let block = AuditReleaseTag::with_gateways(registry, FakeShellGateway::success(), scanner);

        let trigger = test_helpers::make_trigger(
            EventType::ProjectChangesPushed,
            "test-project",
            serde_json::json!({"cve": "CVE-2026-1234"}),
        );
        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        let emitted = &result.events[0];
        assert_eq!(emitted.event_type, EventType::ReleaseTagAudited);
        assert_eq!(emitted.payload["vulnerable"], false);
        assert_eq!(emitted.payload["dirty"], false);
        assert!(
            emitted.payload["scan_error"].as_str().is_some(),
            "scan_error must be set when scanner fails"
        );
        assert!(
            emitted.payload["scan_error"]
                .as_str()
                .unwrap()
                .contains("audit tool not installed"),
            "scan_error should contain the error message"
        );
    }

    // -- VulnerabilityDetected path: git checkout failure --

    #[tokio::test]
    async fn tag_scan_checkout_failure_does_not_scan_and_reports_scan_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry(
            "test-project",
            dir.path().to_str().unwrap(),
        ));

        // Sequence: rev-parse → fetch → tag list ("v1.0.0") → checkout tag (FAIL) →
        //           cleanup checkout_original → cleanup checkout HEAD
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                // rev-parse
                stdout: "main\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                // fetch --tags
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                // tag list
                stdout: "v1.0.0\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                // checkout v1.0.0 — FAIL
                stdout: String::new(),
                stderr: "pathspec 'v1.0.0' did not match any file".to_string(),
                exit_code: 1,
                success: false,
            },
            CommandResult {
                // cleanup: checkout main
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                // cleanup: checkout HEAD
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        // Scanner must NOT be called when checkout fails.
        let scanner = FakeScannerGateway::clean();
        let block = AuditReleaseTag::with_gateways(registry, shell, scanner);

        let trigger = test_helpers::make_trigger(
            EventType::VulnerabilityDetected,
            "test-project",
            serde_json::json!({"cve": "CVE-2026-1234", "vulnerable": true, "dirty": true}),
        );
        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        let emitted = &result.events[0];
        assert_eq!(emitted.event_type, EventType::ReleaseTagAudited);
        // Upstream payload values are forwarded.
        assert_eq!(emitted.payload["cve"], "CVE-2026-1234");
        assert_eq!(emitted.payload["vulnerable"], true);
        // scan_error must be set.
        assert!(
            emitted.payload["scan_error"].as_str().is_some(),
            "scan_error must be set on checkout failure"
        );
        assert!(
            emitted.payload["scan_error"]
                .as_str()
                .unwrap()
                .contains("git checkout v1.0.0 failed"),
            "scan_error should mention the checkout failure"
        );
    }

    // -- VulnerabilityDetected path: scanner errors after successful checkout --

    #[tokio::test]
    async fn tag_scan_scanner_error_falls_back_to_payload_with_scan_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry(
            "test-project",
            dir.path().to_str().unwrap(),
        ));

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
        // Scanner returns a tool-level error (e.g. not installed).
        let scanner = FakeScannerGateway::with_error("cargo audit not found");
        let block = AuditReleaseTag::with_gateways(registry, shell, scanner);

        let trigger = test_helpers::make_trigger(
            EventType::VulnerabilityDetected,
            "test-project",
            serde_json::json!({"cve": "CVE-2026-9999", "vulnerable": true, "dirty": false}),
        );
        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        let emitted = &result.events[0];
        assert_eq!(emitted.event_type, EventType::ReleaseTagAudited);
        // Upstream payload values are preserved.
        assert_eq!(emitted.payload["cve"], "CVE-2026-9999");
        assert_eq!(emitted.payload["vulnerable"], true);
        assert!(
            emitted.payload["scan_error"].as_str().is_some(),
            "scan_error must be set when scanner has tool error"
        );
        assert!(
            emitted.payload["scan_error"]
                .as_str()
                .unwrap()
                .contains("cargo audit not found"),
        );
    }

    #[tokio::test]
    async fn tag_scan_scanner_gateway_failure_falls_back_to_payload_with_scan_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry(
            "test-project",
            dir.path().to_str().unwrap(),
        ));

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
        // Scanner gateway itself returns an Err (e.g. I/O failure).
        let scanner = FakeScannerGateway::gateway_error("I/O error spawning audit tool");
        let block = AuditReleaseTag::with_gateways(registry, shell, scanner);

        let trigger = test_helpers::make_trigger(
            EventType::VulnerabilityDetected,
            "test-project",
            serde_json::json!({"cve": "CVE-2026-9999", "vulnerable": true, "dirty": false}),
        );
        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        let emitted = &result.events[0];
        assert_eq!(emitted.event_type, EventType::ReleaseTagAudited);
        // Upstream payload values are preserved.
        assert_eq!(emitted.payload["cve"], "CVE-2026-9999");
        assert_eq!(emitted.payload["vulnerable"], true);
        assert!(
            emitted.payload["scan_error"].as_str().is_some(),
            "scan_error must be set when gateway returns Err"
        );
        assert!(
            emitted.payload["scan_error"]
                .as_str()
                .unwrap()
                .contains("I/O error spawning audit tool"),
        );
    }
}
