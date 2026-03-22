use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Minimal project registry entry used by audit blocks.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    /// Project name — matches `Event::project`.
    pub name: String,
    /// Filesystem path to the project root.
    pub path: String,
    /// Technology stack (e.g. "rust", "node", "python").
    pub stack: String,
}

/// Registry of known projects and their metadata.
///
/// Injected into blocks that need project-level context (paths, stacks).
/// Constructed once at daemon startup and shared via `Arc`.
#[derive(Debug, Default)]
pub struct Registry {
    pub projects: Vec<RegistryEntry>,
}

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
            registry: Arc::new(Registry::default()),
        }
    }

    /// Create a new `AuditReleaseTag` block backed by the given registry.
    /// Used by `main.rs` when real project registry injection is wired up.
    #[allow(dead_code)]
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

        // In future: shell out to cargo-audit, npm audit, etc.
        // For now: read vulnerability info from the trigger payload.
        let cve = trigger.payload.get("cve").and_then(|v| v.as_str()).unwrap_or("unknown");
        let vulnerable = trigger
            .payload
            .get("vulnerable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        // Forward additional payload fields for downstream blocks.
        let dirty = trigger.payload.get("dirty").and_then(serde_json::Value::as_bool);

        let cve = cve.to_string();
        tracing::info!(%cve, %vulnerable, "audited release tag");

        Box::pin(async move {
            let mut payload = serde_json::json!({ "cve": cve, "vulnerable": vulnerable });
            if let Some(dirty) = dirty {
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

/// Checks whether the main branch still contains a detected vulnerability.
/// Observer — always runs regardless of throttle.
///
/// Self-filters: only acts when the trigger payload has `vulnerable: true`.
/// When the release tag is not vulnerable, returns an empty result to stop the chain.
pub struct AuditMainBranch;

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

        // In future: check if main branch has the same vulnerability.
        // For now: read dirty flag from payload, defaulting to true.
        let cve = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let dirty = trigger
            .payload
            .get("dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        tracing::info!(%cve, %dirty, "audited main branch");

        Box::pin(async move {
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
    use super::*;
    use foundry_core::throttle::Throttle;

    fn make_trigger(event_type: EventType, payload: serde_json::Value) -> Event {
        Event::new(event_type, "test-project".to_string(), Throttle::Full, payload)
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
            projects: vec![RegistryEntry {
                name: "test-project".to_string(),
                path: "/tmp/test-project".to_string(),
                stack: "rust".to_string(),
            }],
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
