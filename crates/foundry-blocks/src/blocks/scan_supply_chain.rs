//! `ScanSupplyChain` — entry block of the nightly supply-chain formation.
//!
//! Sinks on `SupplyChainScanStarted` (emitted by the `nightly-supply-chain`
//! sentinel). For every active project it runs the stack's working-tree audit
//! tool, classifies each advisory against that repo's committed
//! `.supply-chain-allow.json`, and emits a `SupplyChainScanned` event carrying
//! the per-project results for the digest writer.
//!
//! This is the *detection* half of the supply-chain function (EXP-003): it is
//! advisory and read-only. It never mutates a working tree and never fails a
//! project — a supply-chain advisory is an external, time-triggered fact, not a
//! regression in the project's own code. Remediation lands as a separate block.

use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    ProjectSupplyChainScan, SupplyChainFinding, SupplyChainScannedPayload, SuppressedFinding,
};
use foundry_sdk::registry::{ProjectEntry, Registry};
use foundry_sdk::supply_chain::{self, AllowDecision};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::throttle::Throttle;

use crate::gateway::ScannerGateway;

use super::TriggerContext;

task_block_new! {
    /// Scans every active project's working tree for dependency advisories and
    /// classifies findings against each repo's allowlist. Observer — runs
    /// regardless of throttle because it only reads.
    pub struct ScanSupplyChain {
        scanner: ScannerGateway = crate::gateway::ProcessScannerGateway,
    }
}

impl TaskBlock for ScanSupplyChain {
    task_block_meta! {
        name: "Scan Supply Chain",
        kind: Observer,
        sinks_on: [SupplyChainScanStarted],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project, throttle, ..
        } = TriggerContext::from_trigger(trigger);

        // Snapshot the active project list under a short-lived read guard so the
        // lock is never held across an `.await`.
        let entries: Vec<ProjectEntry> = match super::read_registry(&self.registry) {
            Ok(guard) => guard.active_projects().into_iter().cloned().collect(),
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        let scanner = Arc::clone(&self.scanner);

        Box::pin(async move { scan_all(&project, throttle, &entries, scanner.as_ref()).await })
    }
}

/// Scan every entry, classify findings, and emit the `SupplyChainScanned` event.
async fn scan_all(
    project: &str,
    throttle: Throttle,
    entries: &[ProjectEntry],
    scanner: &dyn ScannerGateway,
) -> anyhow::Result<TaskBlockResult> {
    let today = chrono::Local::now().date_naive();
    let mut scans = Vec::with_capacity(entries.len());

    for entry in entries {
        let path = std::path::Path::new(&entry.path);
        // A missing or malformed allowlist must not abort the scan — fall back
        // to "nothing accepted" so every advisory surfaces.
        let allowlist = supply_chain::read_allowlist(path).unwrap_or_default();
        let audit = scanner.run_audit(path, &entry.stack).await.unwrap_or_default();

        let mut findings = Vec::new();
        let mut suppressed = Vec::new();
        for vuln in audit.vulnerabilities {
            // Advisories the scanner cannot name cannot be allowlisted or acted
            // on; skip them rather than emit anonymous noise.
            let Some(cve) = vuln.cve else { continue };
            match allowlist.decide(&cve, today) {
                AllowDecision::NotListed => findings.push(SupplyChainFinding {
                    cve,
                    package: vuln.package,
                    severity: vuln.severity,
                    version: vuln.version,
                    fix_version: vuln.fix_version,
                    fix_package: vuln.fix_package,
                }),
                AllowDecision::Active { reason } => suppressed.push(SuppressedFinding {
                    cve,
                    reason,
                    status: "allowlisted".to_string(),
                    expired_on: None,
                }),
                AllowDecision::Expired { reason, expired_on } => {
                    // A lapsed acceptance resurfaces as a live finding, and is
                    // also recorded as a lapse so the digest can flag it.
                    findings.push(SupplyChainFinding {
                        cve: cve.clone(),
                        package: vuln.package,
                        severity: vuln.severity,
                        version: vuln.version,
                        fix_version: vuln.fix_version,
                        fix_package: vuln.fix_package,
                    });
                    suppressed.push(SuppressedFinding {
                        cve,
                        reason,
                        status: "expired".to_string(),
                        expired_on: Some(expired_on),
                    });
                }
            }
        }

        scans.push(ProjectSupplyChainScan {
            project: entry.name.clone(),
            stack: entry.stack.to_string(),
            findings,
            suppressed,
            scan_error: audit.error,
        });
    }

    let project_count = scans.len() as u64;
    let finding_count: u64 = scans.iter().map(|s| s.findings.len() as u64).sum();
    let affected_project_count = scans.iter().filter(|s| !s.findings.is_empty()).count() as u64;

    tracing::info!(
        project_count,
        finding_count,
        affected_project_count,
        "supply-chain scan complete"
    );

    super::emit_result(
        format!(
            "Supply-chain scan: {finding_count} live finding(s) across {affected_project_count} of {project_count} project(s)"
        ),
        EventType::SupplyChainScanned,
        project,
        throttle,
        &SupplyChainScannedPayload {
            projects: scans,
            project_count,
            finding_count,
            affected_project_count,
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::EventType;
    use foundry_sdk::payload::SupplyChainScannedPayload;
    use foundry_sdk::registry::Registry;

    use crate::gateway::fakes::FakeScannerGateway;
    use crate::scanner::Vulnerability;

    use super::super::test_helpers;
    use super::*;

    fn registry_with(entries: Vec<ProjectEntry>) -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects: entries,
        }))
    }

    fn vuln(cve: &str) -> Vulnerability {
        Vulnerability {
            cve: Some(cve.to_string()),
            severity: Some("high".to_string()),
            package: "vulnerable-pkg".to_string(),
            version: Some("0.1.0".to_string()),
            fix_version: None,
            fix_package: None,
        }
    }

    fn scanned(result: &TaskBlockResult) -> SupplyChainScannedPayload {
        result.events[0].parse_payload().unwrap()
    }

    #[test]
    fn sinks_on_supply_chain_scan_started() {
        let block = ScanSupplyChain::new(registry_with(vec![]));
        assert_eq!(block.sinks_on(), &[EventType::SupplyChainScanStarted]);
    }

    #[tokio::test]
    async fn empty_registry_emits_clean_scan() {
        let block = ScanSupplyChain::new(registry_with(vec![]));
        let trigger = test_helpers::make_trigger(
            EventType::SupplyChainScanStarted,
            "system",
            serde_json::json!({}),
        );
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        let p = scanned(&result);
        assert_eq!(p.project_count, 0);
        assert_eq!(p.finding_count, 0);
    }

    #[tokio::test]
    async fn finding_not_in_allowlist_is_reported_live() {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry_with(vec![test_helpers::project_entry(
            "alpha",
            dir.path().to_str().unwrap(),
        )]);
        let mut vulnerability = vuln("CVE-2026-1234");
        vulnerability.fix_version = Some("3.2.1".to_string());
        vulnerability.fix_package = Some("direct-parent".to_string());
        let scanner = FakeScannerGateway::with_vulnerabilities(vec![vulnerability]);
        let block = ScanSupplyChain::with_gateways(registry, scanner);

        let trigger = test_helpers::make_trigger(
            EventType::SupplyChainScanStarted,
            "system",
            serde_json::json!({}),
        );
        let result = block.execute(&trigger).await.unwrap();

        let p = scanned(&result);
        assert_eq!(p.project_count, 1);
        assert_eq!(p.finding_count, 1);
        assert_eq!(p.affected_project_count, 1);
        assert_eq!(p.projects[0].findings[0].cve, "CVE-2026-1234");
        assert_eq!(p.projects[0].findings[0].fix_package.as_deref(), Some("direct-parent"));
        assert!(p.projects[0].suppressed.is_empty());
    }

    #[tokio::test]
    async fn active_allowlist_entry_suppresses_finding() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".supply-chain-allow.json"),
            r#"{"version":1,"allowed":[{"cve":"CVE-2026-1234","reason":"dev only","expires":"2099-01-01"}]}"#,
        )
        .unwrap();
        let registry = registry_with(vec![test_helpers::project_entry(
            "alpha",
            dir.path().to_str().unwrap(),
        )]);
        let scanner = FakeScannerGateway::with_vulnerabilities(vec![vuln("CVE-2026-1234")]);
        let block = ScanSupplyChain::with_gateways(registry, scanner);

        let trigger = test_helpers::make_trigger(
            EventType::SupplyChainScanStarted,
            "system",
            serde_json::json!({}),
        );
        let result = block.execute(&trigger).await.unwrap();

        let p = scanned(&result);
        assert_eq!(p.finding_count, 0, "active allowlist entry must suppress the finding");
        assert_eq!(p.affected_project_count, 0);
        assert_eq!(p.projects[0].suppressed.len(), 1);
        assert_eq!(p.projects[0].suppressed[0].status, "allowlisted");
    }

    #[tokio::test]
    async fn expired_allowlist_entry_resurfaces_as_live_and_lapsed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".supply-chain-allow.json"),
            r#"{"version":1,"allowed":[{"cve":"CVE-2026-1234","reason":"was dev only","expires":"2020-01-01"}]}"#,
        )
        .unwrap();
        let registry = registry_with(vec![test_helpers::project_entry(
            "alpha",
            dir.path().to_str().unwrap(),
        )]);
        let scanner = FakeScannerGateway::with_vulnerabilities(vec![vuln("CVE-2026-1234")]);
        let block = ScanSupplyChain::with_gateways(registry, scanner);

        let trigger = test_helpers::make_trigger(
            EventType::SupplyChainScanStarted,
            "system",
            serde_json::json!({}),
        );
        let result = block.execute(&trigger).await.unwrap();

        let p = scanned(&result);
        assert_eq!(p.finding_count, 1, "expired acceptance must resurface as a live finding");
        assert_eq!(p.projects[0].suppressed.len(), 1);
        assert_eq!(p.projects[0].suppressed[0].status, "expired");
        assert_eq!(p.projects[0].suppressed[0].expired_on.as_deref(), Some("2020-01-01"));
    }

    #[tokio::test]
    async fn scanner_error_is_reported_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry_with(vec![test_helpers::project_entry(
            "alpha",
            dir.path().to_str().unwrap(),
        )]);
        let scanner = FakeScannerGateway::with_error("pip-audit not installed");
        let block = ScanSupplyChain::with_gateways(registry, scanner);

        let trigger = test_helpers::make_trigger(
            EventType::SupplyChainScanStarted,
            "system",
            serde_json::json!({}),
        );
        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success, "a scan error must not fail the formation");
        let p = scanned(&result);
        assert_eq!(p.finding_count, 0);
        assert_eq!(p.projects[0].scan_error.as_deref(), Some("pip-audit not installed"));
    }

    #[tokio::test]
    async fn unidentified_advisories_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry_with(vec![test_helpers::project_entry(
            "alpha",
            dir.path().to_str().unwrap(),
        )]);
        let scanner = FakeScannerGateway::with_vulnerabilities(vec![Vulnerability {
            cve: None,
            severity: Some("high".to_string()),
            package: "mystery".to_string(),
            version: None,
            fix_version: None,
            fix_package: None,
        }]);
        let block = ScanSupplyChain::with_gateways(registry, scanner);

        let trigger = test_helpers::make_trigger(
            EventType::SupplyChainScanStarted,
            "system",
            serde_json::json!({}),
        );
        let result = block.execute(&trigger).await.unwrap();

        let p = scanned(&result);
        assert_eq!(p.finding_count, 0, "advisories with no CVE id are skipped");
    }
}
