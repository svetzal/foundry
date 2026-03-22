use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Scans project dependencies for known vulnerabilities.
/// Observer — always runs regardless of throttle.
///
/// Sinks on `ScanRequested` and emits zero or more `VulnerabilityDetected`
/// events, one per discovered CVE. Downstream blocks (AuditReleaseTag, etc.)
/// then handle the remediation chain for each vulnerability independently.
pub struct ScanDependencies;

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

        // TODO: Shell out to the appropriate scanner for the project type:
        //   - Rust:   `cargo audit --json`
        //   - Node:   `npm audit --json`
        //   - Python: `pip-audit --format=json`
        // TODO: Parse scanner output into a list of (cve, severity, affected_package) tuples.
        // TODO: Determine `dirty` by comparing the release tag against HEAD on main.
        // TODO: Handle scanner not installed / not on PATH gracefully.

        // Stub: emit a single placeholder vulnerability so the chain can be exercised.
        let vulnerabilities: Vec<serde_json::Value> = vec![serde_json::json!({
            "cve": "STUB-0000",
            "vulnerable": true,
            "dirty": true,
        })];

        tracing::info!(
            found = vulnerabilities.len(),
            "dependency scan complete (stub)"
        );

        Box::pin(async move {
            let events: Vec<Event> = vulnerabilities
                .into_iter()
                .map(|payload| {
                    Event::new(
                        EventType::VulnerabilityDetected,
                        project.clone(),
                        throttle,
                        payload,
                    )
                })
                .collect();

            let count = events.len();
            Ok(TaskBlockResult {
                events,
                success: true,
                // TODO: Include real CVE identifiers in the summary once scanning is implemented.
                summary: format!("Scanned dependencies: {count} vulnerabilities found (stub)"),
            })
        })
    }
}
