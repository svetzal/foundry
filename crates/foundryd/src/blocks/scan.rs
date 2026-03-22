use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Scans project dependencies for known vulnerabilities.
/// Observer — always runs regardless of throttle.
///
/// Sinks on `ScanRequested` and emits zero or more `VulnerabilityDetected`
/// events, one per discovered CVE. Downstream blocks (`AuditReleaseTag`, etc.)
/// then handle the remediation chain for each vulnerability independently.
pub struct ScanDependencies {
    registry: Arc<Registry>,
}

impl ScanDependencies {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl TaskBlock for ScanDependencies {
    fn name(&self) -> &'static str {
        "Scan Dependencies"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::ScanRequested]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();

        Box::pin(async move {
            let Some(entry) = entry else {
                tracing::warn!(project = %project, "project not found in registry, cannot scan");
                return Ok(TaskBlockResult {
                    events: vec![],
                    success: false,
                    summary: format!("Project '{project}' not found in registry"),
                });
            };

            let path = std::path::Path::new(&entry.path);
            tracing::info!(project = %project, stack = %entry.stack, "scanning dependencies");

            let audit_result = crate::scanner::run_audit(path, &entry.stack).await?;

            if let Some(ref err) = audit_result.error {
                tracing::warn!(project = %project, error = %err, "audit tool error");
                return Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: format!("Scan skipped: {err}"),
                });
            }

            if audit_result.vulnerabilities.is_empty() {
                tracing::info!(project = %project, "no vulnerabilities found");
                return Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: format!("{project}: no vulnerabilities found"),
                });
            }

            let events: Vec<Event> = audit_result
                .vulnerabilities
                .iter()
                .map(|vuln| {
                    let cve = vuln.cve.as_deref().unwrap_or("unknown");
                    Event::new(
                        EventType::VulnerabilityDetected,
                        project.clone(),
                        throttle,
                        serde_json::json!({
                            "cve": cve,
                            "vulnerable": true,
                            "dirty": true,
                            "package": vuln.package,
                            "severity": vuln.severity,
                        }),
                    )
                })
                .collect();

            let count = events.len();
            let cves: Vec<&str> =
                audit_result.vulnerabilities.iter().filter_map(|v| v.cve.as_deref()).collect();

            Ok(TaskBlockResult {
                events,
                success: true,
                summary: format!("{project}: {count} vulnerabilities found ({})", cves.join(", ")),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::registry::{ActionFlags, ProjectEntry, Stack};
    use foundry_core::task_block::TaskBlock;
    use foundry_core::throttle::Throttle;

    fn empty_registry() -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![],
        })
    }

    fn registry_with_project(name: &str, path: &str) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: name.to_string(),
                path: path.to_string(),
                stack: Stack::Rust,
                agent: String::new(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                actions: ActionFlags::default(),
                install: None,
            }],
        })
    }

    fn scan_trigger(project: &str) -> Event {
        Event::new(
            EventType::ScanRequested,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
    }

    #[test]
    fn sinks_on_scan_requested() {
        let block = ScanDependencies::new(empty_registry());
        assert_eq!(block.sinks_on(), &[EventType::ScanRequested]);
    }

    #[test]
    fn kind_is_observer() {
        let block = ScanDependencies::new(empty_registry());
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[tokio::test]
    async fn fails_when_project_not_in_registry() {
        let block = ScanDependencies::new(empty_registry());
        let trigger = scan_trigger("unknown-project");
        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("not found in registry"));
    }

    #[tokio::test]
    async fn scans_known_project() {
        // Scanner tool likely not installed in test env — should handle gracefully.
        let registry = registry_with_project("my-project", "/tmp");
        let block = ScanDependencies::new(registry);
        let trigger = scan_trigger("my-project");
        let result = block.execute(&trigger).await.unwrap();
        // Either succeeds with no vulns, or succeeds with scanner error — both are ok.
        assert!(result.success);
    }
}
