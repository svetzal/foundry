use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// The project registry — the source of truth for which projects exist and what automation applies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    /// Registry format version. Must be 2 for the v2 format.
    pub version: u32,
    /// All projects declared in this registry.
    pub projects: Vec<ProjectEntry>,
}

impl Registry {
    /// Deserialize a registry from a JSON file at the given path.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let registry = serde_json::from_str(&content)?;
        Ok(registry)
    }

    /// Return only the projects that are not marked as skipped.
    pub fn active_projects(&self) -> Vec<&ProjectEntry> {
        self.projects.iter().filter(|p| !p.skip.unwrap_or(false)).collect()
    }
}

/// A single project entry in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// Human-readable project name.
    pub name: String,
    /// Absolute path to the project on the local filesystem.
    pub path: String,
    /// Technology stack the project uses.
    pub stack: Stack,
    /// Name of the AI agent assigned to this project.
    pub agent: String,
    /// GitHub repository slug (`owner/repo`).
    pub repo: String,
    /// Default branch to operate on.
    pub branch: String,
    /// When `true` (or `null`-absent-treated-as-false), this project is excluded from runs.
    pub skip: Option<bool>,
    /// Which automation actions are enabled for this project.
    #[serde(default)]
    pub actions: ActionFlags,
    /// Optional local installation configuration.
    pub install: Option<InstallConfig>,
}

/// Flags controlling which automation actions run for a project.
///
/// Each flag maps 1-to-1 to a JSON boolean in the v2 registry format; a
/// state-machine or enum refactor would introduce indirection without benefit
/// for a pure config-deserialization type.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActionFlags {
    /// Run iterative feature/fix work.
    #[serde(default)]
    pub iterate: bool,
    /// Run maintenance tasks (dependency updates, etc.).
    #[serde(default)]
    pub maintain: bool,
    /// Push changes to the remote after automation.
    #[serde(default)]
    pub push: bool,
    /// Run security audit.
    #[serde(default)]
    pub audit: bool,
    /// Trigger a release pipeline.
    #[serde(default)]
    pub release: bool,
}

/// Technology stack identifier for a project.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Stack {
    Rust,
    Python,
    TypeScript,
    Elixir,
}

/// How to install the project locally after automation completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallConfig {
    /// Run an arbitrary shell command to install (e.g. `cargo install --path .`).
    Command(String),
    /// Install via a Homebrew formula.
    Brew(String),
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use tempfile::NamedTempFile;

    use super::*;

    const FULL_REGISTRY_JSON: &str = r#"{
        "version": 2,
        "projects": [
            {
                "name": "my-project",
                "path": "/Users/user/projects/my-project",
                "stack": "rust",
                "agent": "claude",
                "repo": "owner/my-project",
                "branch": "main",
                "skip": false,
                "actions": {
                    "iterate": true,
                    "maintain": true,
                    "push": true,
                    "audit": true,
                    "release": false
                },
                "install": {
                    "command": "cargo install --path ."
                }
            }
        ]
    }"#;

    #[test]
    fn deserialize_v2_registry_json() {
        let registry: Registry = serde_json::from_str(FULL_REGISTRY_JSON).unwrap();
        assert_eq!(registry.version, 2);
        assert_eq!(registry.projects.len(), 1);

        let project = &registry.projects[0];
        assert_eq!(project.name, "my-project");
        assert_eq!(project.path, "/Users/user/projects/my-project");
        assert_eq!(project.stack, Stack::Rust);
        assert_eq!(project.agent, "claude");
        assert_eq!(project.repo, "owner/my-project");
        assert_eq!(project.branch, "main");
        assert_eq!(project.skip, Some(false));

        let actions = &project.actions;
        assert!(actions.iterate);
        assert!(actions.maintain);
        assert!(actions.push);
        assert!(actions.audit);
        assert!(!actions.release);

        assert!(
            matches!(&project.install, Some(InstallConfig::Command(cmd)) if cmd == "cargo install --path .")
        );
    }

    #[test]
    fn active_projects_filters_skipped_entries() {
        let registry: Registry = serde_json::from_str(
            r#"{
                "version": 2,
                "projects": [
                    {
                        "name": "active",
                        "path": "/projects/active",
                        "stack": "rust",
                        "agent": "claude",
                        "repo": "owner/active",
                        "branch": "main",
                        "skip": false
                    },
                    {
                        "name": "skipped",
                        "path": "/projects/skipped",
                        "stack": "python",
                        "agent": "claude",
                        "repo": "owner/skipped",
                        "branch": "main",
                        "skip": true
                    }
                ]
            }"#,
        )
        .unwrap();

        let active = registry.active_projects();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "active");
    }

    #[test]
    fn active_projects_includes_project_with_null_skip() {
        let registry: Registry = serde_json::from_str(
            r#"{
                "version": 2,
                "projects": [
                    {
                        "name": "no-skip-field",
                        "path": "/projects/no-skip",
                        "stack": "typescript",
                        "agent": "claude",
                        "repo": "owner/no-skip",
                        "branch": "main"
                    }
                ]
            }"#,
        )
        .unwrap();

        let active = registry.active_projects();
        assert_eq!(active.len(), 1);
    }

    #[test]
    fn project_entry_with_all_optional_fields() {
        let registry: Registry = serde_json::from_str(FULL_REGISTRY_JSON).unwrap();
        let project = &registry.projects[0];
        assert!(project.skip.is_some());
        assert!(project.install.is_some());
    }

    #[test]
    fn project_entry_with_minimal_required_fields() {
        let registry: Registry = serde_json::from_str(
            r#"{
                "version": 2,
                "projects": [
                    {
                        "name": "minimal",
                        "path": "/projects/minimal",
                        "stack": "elixir",
                        "agent": "claude",
                        "repo": "owner/minimal",
                        "branch": "main"
                    }
                ]
            }"#,
        )
        .unwrap();

        let project = &registry.projects[0];
        assert_eq!(project.name, "minimal");
        assert_eq!(project.stack, Stack::Elixir);
        assert!(project.skip.is_none());
        assert!(project.install.is_none());
        // ActionFlags default to false when the "actions" key is absent
        assert!(!project.actions.iterate);
        assert!(!project.actions.maintain);
    }

    #[test]
    fn empty_projects_array() {
        let registry: Registry = serde_json::from_str(r#"{"version": 2, "projects": []}"#).unwrap();
        assert_eq!(registry.projects.len(), 0);
        assert_eq!(registry.active_projects().len(), 0);
    }

    #[test]
    fn install_config_brew_variant() {
        let registry: Registry = serde_json::from_str(
            r#"{
                "version": 2,
                "projects": [
                    {
                        "name": "brew-project",
                        "path": "/projects/brew",
                        "stack": "rust",
                        "agent": "claude",
                        "repo": "owner/brew",
                        "branch": "main",
                        "install": {"brew": "my-formula"}
                    }
                ]
            }"#,
        )
        .unwrap();

        assert!(
            matches!(&registry.projects[0].install, Some(InstallConfig::Brew(f)) if f == "my-formula")
        );
    }

    #[test]
    fn load_registry_from_tempfile() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(FULL_REGISTRY_JSON.as_bytes()).unwrap();
        file.flush().unwrap();

        let registry = Registry::load(file.path()).unwrap();
        assert_eq!(registry.version, 2);
        assert_eq!(registry.projects.len(), 1);
        assert_eq!(registry.projects[0].name, "my-project");
    }

    #[test]
    fn all_stack_variants_deserialize() {
        for (json, expected) in [
            ("rust", Stack::Rust),
            ("python", Stack::Python),
            ("typescript", Stack::TypeScript),
            ("elixir", Stack::Elixir),
        ] {
            let stack: Stack = serde_json::from_str(&format!(r#""{json}""#)).unwrap();
            assert_eq!(stack, expected);
        }
    }
}
