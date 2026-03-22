use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Attempts to fix a vulnerability on the main branch.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Self-filters: only acts when `dirty=true` in the trigger payload.
///
/// Invokes `hone maintain <agent> <path>` to fix the vulnerable dependency.
pub struct RemediateVulnerability {
    registry: Arc<Registry>,
}

impl RemediateVulnerability {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl TaskBlock for RemediateVulnerability {
    fn name(&self) -> &'static str {
        "Remediate Vulnerability"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::MainBranchAudited]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        // Self-filter: only remediate when main branch is dirty.
        let dirty = trigger
            .payload
            .get("dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        if !dirty {
            tracing::info!("main branch is clean, skipping remediation");
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: main branch is clean".to_string(),
                })
            });
        }

        let cve = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // Resolve project agent and path from registry.
        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();

        tracing::info!(%cve, "remediating vulnerability");

        Box::pin(async move {
            let Some(entry) = entry else {
                tracing::warn!(project = %project, "project not found in registry, cannot remediate");
                return Ok(TaskBlockResult {
                    events: vec![],
                    success: false,
                    summary: format!("Project '{project}' not found in registry"),
                });
            };

            let agent = if entry.agent.is_empty() {
                "claude"
            } else {
                &entry.agent
            };
            let project_path = &entry.path;

            tracing::info!(
                project = %project,
                agent = agent,
                path = %project_path,
                %cve,
                "invoking hone maintain"
            );

            let run_result = crate::shell::run(
                Path::new(project_path),
                "hone",
                &["maintain", agent, project_path, "--json"],
                None,
                None,
            )
            .await;

            let (success, hone_summary) = match run_result {
                Ok(result) => {
                    let s = result.success;
                    let summary = parse_hone_summary(&result.stdout, s);
                    (s, summary)
                }
                Err(err) => {
                    tracing::warn!(error = %err, "hone not available or failed to spawn");
                    (false, format!("hone unavailable: {err}"))
                }
            };

            tracing::info!(
                project = %project,
                success = success,
                summary = %hone_summary,
                "hone maintain completed"
            );

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::RemediationCompleted,
                    project,
                    throttle,
                    serde_json::json!({
                        "cve": cve,
                        "success": success,
                        "summary": hone_summary,
                    }),
                )],
                success,
                summary: if success {
                    format!("Remediated {cve}: {hone_summary}")
                } else {
                    format!("Remediation of {cve} failed: {hone_summary}")
                },
            })
        })
    }
}

/// Extract a human-readable summary from hone's JSON output.
/// Falls back gracefully when the output is not valid JSON or lacks the expected field.
fn parse_hone_summary(stdout: &str, success: bool) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) {
        if let Some(summary) =
            value.get("summary").or_else(|| value.get("message")).and_then(|v| v.as_str())
        {
            return summary.to_string();
        }
    }

    // Fall back to first non-empty line of raw output.
    stdout.lines().find(|l| !l.trim().is_empty()).map_or_else(
        || {
            if success {
                "completed".to_string()
            } else {
                "failed (no output)".to_string()
            }
        },
        str::to_string,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::registry::{ActionFlags, ProjectEntry};
    use foundry_core::throttle::Throttle;

    fn registry_with_project(name: &str, path: &str, agent: &str) -> Arc<Registry> {
        use foundry_core::registry::Stack;
        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: name.to_string(),
                path: path.to_string(),
                stack: Stack::Rust,
                agent: agent.to_string(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                actions: ActionFlags::default(),
                install: None,
            }],
        })
    }

    fn dirty_trigger(project: &str, cve: &str) -> Event {
        Event::new(
            EventType::MainBranchAudited,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": true, "cve": cve }),
        )
    }

    fn clean_trigger(project: &str) -> Event {
        Event::new(
            EventType::MainBranchAudited,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-9999" }),
        )
    }

    #[tokio::test]
    async fn skips_when_main_branch_is_clean() {
        let block = RemediateVulnerability::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let trigger = clean_trigger("any-project");

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("clean"));
    }

    #[tokio::test]
    async fn fails_when_project_not_in_registry() {
        let block = RemediateVulnerability::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let trigger = dirty_trigger("unknown-project", "CVE-2026-1234");

        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("not found in registry"));
    }

    #[tokio::test]
    async fn emits_remediation_completed_when_project_found() {
        // When hone is unavailable the block still emits RemediationCompleted
        // (with success=false) so the event chain can continue.
        let registry = registry_with_project("my-project", "/tmp", "claude");
        let block = RemediateVulnerability::new(registry);
        let trigger = dirty_trigger("my-project", "CVE-2026-9999");

        let result = block.execute(&trigger).await.unwrap();
        // Exactly one event should be emitted regardless of hone availability.
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::RemediationCompleted);
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-9999");
    }

    #[test]
    fn parse_hone_summary_extracts_json_summary_field() {
        let json = r#"{"summary":"fixed 2 vulnerabilities","changed":true}"#;
        assert_eq!(parse_hone_summary(json, true), "fixed 2 vulnerabilities");
    }

    #[test]
    fn parse_hone_summary_falls_back_to_first_line() {
        let raw = "Patching dependency tree\nDone.";
        assert_eq!(parse_hone_summary(raw, true), "Patching dependency tree");
    }

    #[test]
    fn parse_hone_summary_empty_output_failure() {
        assert_eq!(parse_hone_summary("", false), "failed (no output)");
    }

    #[test]
    fn parse_hone_summary_empty_output_success() {
        assert_eq!(parse_hone_summary("", true), "completed");
    }
}
