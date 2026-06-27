use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use foundry_sdk::event::Event;
use foundry_sdk::payload::{MainBranchAuditedPayload, ReleaseRequestedPayload};
use foundry_sdk::registry::Registry;
use foundry_sdk::work_block::EventAdapter;

use super::ReleaseInput;

/// Adapts a `MainBranchAudited` event into a [`ReleaseInput`] for the
/// vulnerability-driven release path.
///
/// Returns `None` when `dirty=true` (self-filter: only acts on clean branches).
pub struct VulnReleaseAdapter {
    registry: Arc<RwLock<Registry>>,
}

impl VulnReleaseAdapter {
    pub fn new(registry: Arc<RwLock<Registry>>) -> Self {
        Self { registry }
    }
}

impl EventAdapter<ReleaseInput> for VulnReleaseAdapter {
    fn adapt(&self, trigger: &Event) -> Option<ReleaseInput> {
        let p = trigger.parse_payload::<MainBranchAuditedPayload>().ok()?;
        if p.dirty {
            tracing::info!("main branch is dirty, skipping release");
            return None;
        }

        let project = &trigger.project;
        let cve = p.cve.clone();

        let guard = match super::super::read_registry(&self.registry) {
            Ok(g) => g,
            Err(e) => {
                tracing::error!(error = %e, "registry lock poisoned in VulnReleaseAdapter");
                return None;
            }
        };
        let entry = guard.find_project(project)?;
        let project_path = PathBuf::from(&entry.path);

        let prompt = format!(
            "Cut a patch release for {project} fixing {cve}. \
             Create a changelog entry, bump the patch version, tag the release, and push."
        );

        tracing::info!(%project, %cve, "cutting patch release");

        Some(ReleaseInput {
            project: project.clone(),
            project_path,
            prompt,
        })
    }
}

/// Adapts a `ReleaseRequested` event into a [`ReleaseInput`] for the
/// manual release path.
///
/// Returns `None` when `entry.actions.release` is false.
pub struct ManualReleaseAdapter {
    registry: Arc<RwLock<Registry>>,
}

impl ManualReleaseAdapter {
    pub fn new(registry: Arc<RwLock<Registry>>) -> Self {
        Self { registry }
    }
}

impl EventAdapter<ReleaseInput> for ManualReleaseAdapter {
    fn adapt(&self, trigger: &Event) -> Option<ReleaseInput> {
        let project = &trigger.project;

        let guard = match super::super::read_registry(&self.registry) {
            Ok(g) => g,
            Err(e) => {
                tracing::error!(error = %e, "registry lock poisoned in ManualReleaseAdapter");
                return None;
            }
        };
        let Some(entry) = guard.find_project(project) else {
            tracing::warn!(project = %project, "project not found in registry");
            return None;
        };

        if !entry.actions.release {
            tracing::info!(%project, "release action disabled, skipping");
            return None;
        }

        let project_path = PathBuf::from(&entry.path);
        let bump = trigger.parse_payload::<ReleaseRequestedPayload>().ok().and_then(|p| p.bump);

        let bump_instruction = match &bump {
            Some(b) => format!("The version bump type is {b}."),
            None => {
                "Determine the appropriate version bump from the changelog and unreleased changes."
                    .to_string()
            }
        };

        let prompt = format!(
            "Release {project}. Follow the release process documented in AGENTS.md exactly.\n\
             {bump_instruction}\n\
             Complete all steps: run quality gates, update the changelog, bump the version in all \
             locations, commit (the version-bump commit must be the HEAD commit), then create the \
             git tag pointing at that HEAD commit, and finally push both the commit and the tag. \
             IMPORTANT: create the git tag ONLY after the version-bump/changelog commit so the \
             tag points at the correct commit. Verify that `git rev-parse <tag>^{{commit}}` \
             matches `git rev-parse HEAD` before pushing. \
             Output the new version tag on a line by itself (e.g. v1.2.3)."
        );

        tracing::info!(%project, bump = bump.as_deref().unwrap_or("auto"), "executing release");

        Some(ReleaseInput {
            project: project.clone(),
            project_path,
            prompt,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::work_block::EventAdapter;

    use super::*;

    fn make_registry(entry: ProjectEntry) -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![entry],
        }))
    }

    fn empty_registry() -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        }))
    }

    fn project_entry(name: &str, path: &str) -> ProjectEntry {
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
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        }
    }

    fn make_trigger(event_type: EventType, project: &str, payload: serde_json::Value) -> Event {
        Event::new(event_type, project.to_string(), Throttle::Full, payload)
    }

    // --- VulnReleaseAdapter ---

    #[test]
    fn vuln_adapter_returns_some_for_clean_branch_in_registry() {
        let entry = project_entry("my-project", "/path/to/project");
        let adapter = VulnReleaseAdapter::new(make_registry(entry));
        let trigger = make_trigger(
            EventType::MainBranchAudited,
            "my-project",
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-1234" }),
        );

        let input = adapter.adapt(&trigger);
        assert!(input.is_some());
        let input = input.unwrap();
        assert_eq!(input.project, "my-project");
        assert_eq!(input.project_path, PathBuf::from("/path/to/project"));
        assert!(input.prompt.contains("CVE-2026-1234"));
    }

    #[test]
    fn vuln_adapter_returns_none_when_dirty() {
        let entry = project_entry("my-project", "/path/to/project");
        let adapter = VulnReleaseAdapter::new(make_registry(entry));
        let trigger = make_trigger(
            EventType::MainBranchAudited,
            "my-project",
            serde_json::json!({ "dirty": true, "cve": "CVE-2026-5678" }),
        );

        assert!(adapter.adapt(&trigger).is_none());
    }

    #[test]
    fn vuln_adapter_returns_none_when_project_not_in_registry() {
        let adapter = VulnReleaseAdapter::new(empty_registry());
        let trigger = make_trigger(
            EventType::MainBranchAudited,
            "unknown-project",
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-0001" }),
        );

        assert!(adapter.adapt(&trigger).is_none());
    }

    // --- ManualReleaseAdapter ---

    #[test]
    fn manual_adapter_returns_some_when_release_enabled() {
        let mut entry = project_entry("my-project", "/path/to/project");
        entry.actions = ActionFlags {
            release: true,
            ..ActionFlags::default()
        };
        let adapter = ManualReleaseAdapter::new(make_registry(entry));
        let trigger = make_trigger(
            EventType::ReleaseRequested,
            "my-project",
            serde_json::json!({ "bump": "minor" }),
        );

        let input = adapter.adapt(&trigger);
        assert!(input.is_some());
        let input = input.unwrap();
        assert_eq!(input.project, "my-project");
        assert_eq!(input.project_path, PathBuf::from("/path/to/project"));
        assert!(input.prompt.contains("minor"));
    }

    #[test]
    fn manual_adapter_returns_none_when_release_disabled() {
        let entry = project_entry("my-project", "/path/to/project");
        // ActionFlags::default() has release=false
        let adapter = ManualReleaseAdapter::new(make_registry(entry));
        let trigger =
            make_trigger(EventType::ReleaseRequested, "my-project", serde_json::json!({}));

        assert!(adapter.adapt(&trigger).is_none());
    }

    #[test]
    fn manual_adapter_returns_none_when_project_not_in_registry() {
        let adapter = ManualReleaseAdapter::new(empty_registry());
        let trigger =
            make_trigger(EventType::ReleaseRequested, "unknown-project", serde_json::json!({}));

        assert!(adapter.adapt(&trigger).is_none());
    }

    #[test]
    fn manual_adapter_uses_auto_bump_when_bump_absent() {
        let mut entry = project_entry("my-project", "/path/to/project");
        entry.actions = ActionFlags {
            release: true,
            ..ActionFlags::default()
        };
        let adapter = ManualReleaseAdapter::new(make_registry(entry));
        let trigger =
            make_trigger(EventType::ReleaseRequested, "my-project", serde_json::json!({}));

        let input = adapter.adapt(&trigger).unwrap();
        assert!(input.prompt.contains("Determine the appropriate version bump"));
    }
}
