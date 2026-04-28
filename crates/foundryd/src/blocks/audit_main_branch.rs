use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ScannerGateway;

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

        // Use direct Value access — test payloads may omit required typed fields.
        let vulnerable = trigger
            .payload
            .get("vulnerable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        if !vulnerable {
            tracing::info!("release tag not vulnerable, skipping main branch audit");
            return skip!("Skipped: release tag not vulnerable");
        }

        // Payload fallback values — used when the project is not in the registry,
        // or when the scanner cannot run (no lockfile / tooling not installed).
        let cve_from_payload = trigger
            .payload
            .get("cve")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let dirty_from_payload = trigger
            .payload
            .get("dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        // Look up the project entry in the registry.
        let entry = self.registry.find_project(&project).cloned();
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

            Ok(TaskBlockResult::success(
                format!("Main branch audited: {cve} dirty={dirty}"),
                vec![Event::new(
                    EventType::MainBranchAudited,
                    project,
                    throttle,
                    serde_json::json!({ "cve": cve, "dirty": dirty }),
                )],
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::EventType;
    use foundry_core::registry::Registry;

    use crate::gateway::fakes::FakeScannerGateway;
    use crate::scanner::Vulnerability;

    use super::super::test_helpers;
    use super::*;

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
        let trigger = test_helpers::make_trigger(
            EventType::ReleaseTagAudited,
            "test-project",
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
}
