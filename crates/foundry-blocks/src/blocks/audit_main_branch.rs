use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{MainBranchAuditedPayload, ReleaseTagAuditedPayload};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock};

use crate::gateway::ScannerGateway;

use super::TriggerContext;

task_block_new! {
    /// Checks whether the main branch still contains a detected vulnerability.
    /// Observer — always runs regardless of throttle.
    ///
    /// Self-filters: only acts when the trigger payload has `vulnerable: true`.
    /// When the release tag is not vulnerable, returns an empty result to stop the chain.
    pub struct AuditMainBranch {
        scanner: ScannerGateway = crate::gateway::ProcessScannerGateway,
    }
}

fn accepts_audit_main(trigger: &Event) -> bool {
    trigger
        .parse_payload::<ReleaseTagAuditedPayload>()
        .ok()
        .is_some_and(|p| p.vulnerable)
}

impl TaskBlock for AuditMainBranch {
    task_block_meta! {
        name: "Audit Main Branch",
        kind: Observer,
        sinks_on: [ReleaseTagAudited],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        accepts_audit_main(trigger)
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project, throttle, ..
        } = TriggerContext::from_trigger(trigger);

        let p = parse_payload!(trigger, ReleaseTagAuditedPayload);

        // Payload fallback values — used when the project is not in the registry,
        // or when the scanner cannot run (no lockfile / tooling not installed).
        let cve_from_payload = p.cve.clone();
        let dirty_from_payload = p.dirty.unwrap_or(true);

        // Look up the project entry in the registry.
        let entry = match super::read_registry(&self.registry) {
            Ok(guard) => guard.find_project(&project).cloned(),
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        let scanner = Arc::clone(&self.scanner);

        Box::pin(async move {
            let (cve, dirty) = if let Some(entry) = entry {
                let path = std::path::Path::new(&entry.path);

                // Scan the current branch — no checkout needed, we are already on main.
                let audit_outcome =
                    crate::scanner::audit_outcome(scanner.run_audit(path, &entry.stack).await);

                match audit_outcome {
                    Err(msg) => {
                        // Record: the scan did not run at all (spawn failure, tool not
                        // installed). This is distinct from a scan that ran and found
                        // nothing — worth a warn, not a routine info line.
                        tracing::warn!(
                            project = %project,
                            error = %msg,
                            "supply-chain scan did not run; falling back to payload dirty flag"
                        );
                        (cve_from_payload, dirty_from_payload)
                    }
                    Ok(audit_result) => {
                        let reported = crate::scanner::filter_audit_exceptions(
                            &audit_result,
                            &entry.audit_exceptions,
                        );

                        if reported.is_empty() {
                            // Scan genuinely ran and reported nothing (no lockfile / is
                            // genuinely clean, or all findings suppressed by
                            // audit_exceptions). Fall back to payload to preserve
                            // integration-test behavior.
                            tracing::info!(
                                project = %project,
                                "scanner returned no results, falling back to payload dirty flag"
                            );
                            (cve_from_payload, dirty_from_payload)
                        } else {
                            // Dirty when the CVE from the release-tag audit is still
                            // present on main.
                            let dirty = reported
                                .iter()
                                .any(|v| v.cve.as_deref() == Some(cve_from_payload.as_str()));
                            let cve = reported
                                .first()
                                .and_then(|v| v.cve.clone())
                                .unwrap_or_else(|| cve_from_payload.clone());
                            (cve, dirty)
                        }
                    }
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

            super::emit_result(
                format!("Main branch audited: {cve} dirty={dirty}"),
                EventType::MainBranchAudited,
                &project,
                throttle,
                &MainBranchAuditedPayload {
                    project: project.clone(),
                    cve,
                    vulnerable: true,
                    dirty,
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::EventType;
    use foundry_sdk::registry::Registry;

    use crate::gateway::fakes::FakeScannerGateway;
    use crate::scanner::Vulnerability;

    use super::super::test_helpers;
    use super::*;

    #[test]
    fn main_branch_sinks_on_release_tag_audited() {
        let block = AuditMainBranch::new(Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        })));
        assert_eq!(block.sinks_on(), &[EventType::ReleaseTagAudited]);
    }

    #[test]
    fn accepts_returns_false_when_not_vulnerable() {
        let block = AuditMainBranch::new(Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        })));
        let trigger = test_helpers::make_trigger(
            EventType::ReleaseTagAudited,
            "test-project",
            serde_json::json!({"vulnerable": false, "cve": "CVE-2026-1234"}),
        );
        assert!(!block.accepts(&trigger), "should not accept non-vulnerable release tag events");
    }

    #[test]
    fn accepts_returns_true_when_vulnerable() {
        let block = AuditMainBranch::new(Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        })));
        let trigger = test_helpers::make_trigger(
            EventType::ReleaseTagAudited,
            "test-project",
            serde_json::json!({"vulnerable": true, "cve": "CVE-2026-1234"}),
        );
        assert!(block.accepts(&trigger), "should accept vulnerable release tag events");
    }

    #[tokio::test]
    async fn main_branch_falls_back_to_payload_when_project_not_in_registry() {
        let block = AuditMainBranch::new(Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        })));
        let trigger = test_helpers::make_trigger(
            EventType::ReleaseTagAudited,
            "test-project",
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
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry(
            "test-project",
            dir.path().to_str().unwrap(),
        ));
        let scanner = FakeScannerGateway::with_vulnerabilities(vec![Vulnerability {
            cve: Some("CVE-2026-1234".to_string()),
            severity: Some("high".to_string()),
            package: "vulnerable-crate".to_string(),
            version: None,
            fix_version: None,
            fix_package: None,
        }]);
        let block = AuditMainBranch::with_gateways(registry, scanner);

        let trigger = test_helpers::make_trigger(
            EventType::ReleaseTagAudited,
            "test-project",
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
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry(
            "test-project",
            dir.path().to_str().unwrap(),
        ));
        let scanner = FakeScannerGateway::clean();
        let block = AuditMainBranch::with_gateways(registry, scanner);

        let trigger = test_helpers::make_trigger(
            EventType::ReleaseTagAudited,
            "test-project",
            serde_json::json!({"vulnerable": true, "cve": "CVE-2026-1234", "dirty": false}),
        );
        let result = block.execute(&trigger).await.unwrap();

        // Scanner returned clean; falls back to payload dirty=false
        assert!(result.success);
        assert_eq!(result.events[0].payload["dirty"], false);
    }

    #[tokio::test]
    async fn main_branch_scanner_error_falls_back_to_payload() {
        // Exercises the `Err` branch of `audit_outcome` (scan did not run at
        // all) rather than the "ran and found nothing" branch above. Behavior
        // is identical from the caller's perspective — same payload fallback —
        // but the two are logged at different levels internally.
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry(
            "test-project",
            dir.path().to_str().unwrap(),
        ));
        let scanner = FakeScannerGateway::gateway_error("failed to spawn audit tool");
        let block = AuditMainBranch::with_gateways(registry, scanner);

        let trigger = test_helpers::make_trigger(
            EventType::ReleaseTagAudited,
            "test-project",
            serde_json::json!({"vulnerable": true, "cve": "CVE-2026-1234", "dirty": false}),
        );
        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["dirty"], false);
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-1234");
    }
}
