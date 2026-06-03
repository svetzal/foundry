use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use foundry_core::event::Event;
use foundry_core::payload::{MainBranchAuditedPayload, ReleaseRequestedPayload};
use foundry_core::registry::Registry;
use foundry_core::work_block::EventAdapter;

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
