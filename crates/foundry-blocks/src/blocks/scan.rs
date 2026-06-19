use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::VulnerabilityDetectedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ScannerGateway;

task_block_new! {
    /// Scans project dependencies for known vulnerabilities.
    /// Observer — always runs regardless of throttle.
    ///
    /// Sinks on `ScanRequested` and emits zero or more `VulnerabilityDetected`
    /// events, one per discovered CVE. Downstream blocks (`AuditReleaseTag`, etc.)
    /// then handle the remediation chain for each vulnerability independently.
    pub struct ScanDependencies {
        scanner: ScannerGateway = crate::gateway::ProcessScannerGateway
    }
}

impl TaskBlock for ScanDependencies {
    task_block_meta! {
        name: "Scan Dependencies",
        kind: Observer,
        sinks_on: [ScanRequested],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let entry = require_project!(self, project);
        let scanner = Arc::clone(&self.scanner);

        Box::pin(async move {
            let path = std::path::Path::new(&entry.path);
            tracing::info!(project = %project, stack = %entry.stack, "scanning dependencies");

            let audit_result = scanner.run_audit(path, &entry.stack).await?;

            if let Some(ref err) = audit_result.error {
                tracing::warn!(project = %project, error = %err, "audit tool error");
                return Ok(TaskBlockResult::success(format!("Scan skipped: {err}"), vec![]));
            }

            if audit_result.vulnerabilities.is_empty() {
                tracing::info!(project = %project, "no vulnerabilities found");
                return Ok(TaskBlockResult::success(
                    format!("{project}: no vulnerabilities found"),
                    vec![],
                ));
            }

            let events: Vec<Event> = audit_result
                .vulnerabilities
                .iter()
                .map(|vuln| {
                    let cve = vuln.cve.as_deref().unwrap_or("unknown").to_string();
                    super::event_from_infallible_payload(
                        EventType::VulnerabilityDetected,
                        &project,
                        throttle,
                        &VulnerabilityDetectedPayload {
                            cve,
                            vulnerable: true,
                            dirty: true,
                            package: vuln.package.clone(),
                            severity: vuln.severity.clone().unwrap_or_default(),
                        },
                    )
                })
                .collect();

            let count = events.len();
            let cves: Vec<&str> =
                audit_result.vulnerabilities.iter().filter_map(|v| v.cve.as_deref()).collect();

            Ok(TaskBlockResult::success(
                format!("{project}: {count} vulnerabilities found ({})", cves.join(", ")),
                events,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use crate::gateway::fakes::FakeScannerGateway;
    use crate::scanner::Vulnerability;
    use foundry_sdk::event::EventType;
    use foundry_sdk::registry::Registry;
    use foundry_sdk::task_block::TaskBlock;

    use super::super::test_helpers;
    use super::ScanDependencies;

    assert_block_meta!(
        ScanDependencies::new(Arc::new(RwLock::new(Registry { version: 2, projects: vec![] }))),
        kind: Observer,
        sinks_on: [ScanRequested],
    );

    #[tokio::test]
    async fn fails_when_project_not_in_registry() {
        let block = ScanDependencies::new(test_helpers::empty_registry());
        let trigger = test_event!(EventType::ScanRequested, "unknown-project", {});
        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("not found in registry"));
    }

    #[tokio::test]
    async fn clean_project_emits_no_events() {
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            "/tmp",
            "",
        ));
        let scanner = FakeScannerGateway::clean();
        let block = ScanDependencies::with_gateways(registry, scanner);
        let trigger = test_event!(EventType::ScanRequested, "my-project", {});

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("no vulnerabilities found"));
    }

    #[tokio::test]
    async fn vulnerabilities_emitted_correctly() {
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            "/tmp",
            "",
        ));
        let vulns = vec![Vulnerability {
            cve: Some("CVE-2026-1234".to_string()),
            severity: Some("high".to_string()),
            package: "some-crate".to_string(),
            version: Some("0.1.0".to_string()),
            fix_version: None,
        }];
        let scanner = FakeScannerGateway::with_vulnerabilities(vulns);
        let block = ScanDependencies::with_gateways(registry, scanner);
        let trigger = test_event!(EventType::ScanRequested, "my-project", {});

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::VulnerabilityDetected);
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-1234");
        assert_eq!(result.events[0].payload["package"], "some-crate");
        assert_eq!(result.events[0].payload["severity"], "high");
        assert!(result.summary.contains("CVE-2026-1234"));
    }

    #[tokio::test]
    async fn multiple_vulnerabilities_emit_one_event_each() {
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            "/tmp",
            "",
        ));
        let vulns = vec![
            Vulnerability {
                cve: Some("CVE-2026-0001".to_string()),
                severity: Some("high".to_string()),
                package: "crate-a".to_string(),
                version: None,
                fix_version: None,
            },
            Vulnerability {
                cve: Some("CVE-2026-0002".to_string()),
                severity: Some("medium".to_string()),
                package: "crate-b".to_string(),
                version: None,
                fix_version: None,
            },
        ];
        let scanner = FakeScannerGateway::with_vulnerabilities(vulns);
        let block = ScanDependencies::with_gateways(registry, scanner);
        let trigger = test_event!(EventType::ScanRequested, "my-project", {});

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 2);
        assert!(result.summary.contains("2 vulnerabilities"));
    }

    #[tokio::test]
    async fn scanner_error_handled_gracefully() {
        let registry = test_helpers::registry_with_entry(test_helpers::project_entry_with_agent(
            "my-project",
            "/tmp",
            "",
        ));
        let scanner = FakeScannerGateway::with_error("cargo audit not installed");
        let block = ScanDependencies::with_gateways(registry, scanner);
        let trigger = test_event!(EventType::ScanRequested, "my-project", {});

        let result = block.execute(&trigger).await.unwrap();

        // A scanner error is not a block failure — the scan is simply skipped.
        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("Scan skipped"));
    }
}
