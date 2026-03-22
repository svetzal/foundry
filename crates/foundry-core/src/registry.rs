use serde::{Deserialize, Serialize};

/// Project registry — describes every project foundry manages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    /// Schema version of the registry file.
    pub version: u32,
    /// All registered projects.
    pub projects: Vec<ProjectEntry>,
}

impl Registry {
    /// Load a registry from a JSON file.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let registry = serde_json::from_str(&content)?;
        Ok(registry)
    }

    /// Return only projects that are not skipped.
    pub fn active_projects(&self) -> Vec<&ProjectEntry> {
        self.projects.iter().filter(|p| p.skip.is_none()).collect()
    }
}

/// A single project in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// Short identifier used in events.
    pub name: String,
    /// Absolute path to the project on disk.
    pub path: String,
    /// Technology stack (determines scanner tool).
    pub stack: Stack,
    /// AI agent name passed to hone commands.
    pub agent: String,
    /// Git remote URL.
    pub repo: String,
    /// Default branch (e.g. "main").
    pub branch: String,
    /// Optional reason to skip this project during maintenance.
    pub skip: Option<String>,
    /// Which maintenance actions are enabled for this project.
    pub actions: ActionFlags,
    /// Optional local install configuration.
    pub install: Option<InstallConfig>,
    /// Optional directory for hone audit artifacts (--audit-dir).
    pub audit_dir: Option<String>,
}

/// Technology stack — controls which scanner and toolchain commands to use.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stack {
    Rust,
    Python,
    #[serde(rename = "typescript")]
    TypeScript,
    Bun,
    Elixir,
    #[serde(other)]
    Unknown,
}

/// Toggles for per-project maintenance actions.
///
/// Each flag independently controls a maintenance step for the project.
/// Clippy's `struct_excessive_bools` is suppressed here because these are
/// genuinely independent boolean feature flags, not a state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActionFlags {
    /// Run `hone iterate` on this project.
    #[serde(default)]
    pub iterate: bool,
    /// Run `hone maintain` on this project.
    #[serde(default)]
    pub maintain: bool,
    /// Push changes to the remote after committing.
    #[serde(default)]
    pub push: bool,
    /// Run release audit after push.
    #[serde(default)]
    pub audit: bool,
    /// Auto-cut a release if audit finds vulnerabilities.
    #[serde(default)]
    pub release: bool,
}

/// How to install this project locally after a new release.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum InstallConfig {
    /// Run an arbitrary shell command in the project directory.
    Command { command: String },
    /// Upgrade a Homebrew formula.
    Brew { formula: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn sample_registry_json() -> &'static str {
        r#"{
            "version": 2,
            "projects": [
                {
                    "name": "hone-cli",
                    "path": "/home/user/projects/hone-cli",
                    "stack": "rust",
                    "agent": "claude-sonnet-4-5",
                    "repo": "https://github.com/example/hone-cli",
                    "branch": "main",
                    "skip": null,
                    "actions": {
                        "iterate": true,
                        "maintain": true,
                        "push": true,
                        "audit": true,
                        "release": false
                    },
                    "install": { "method": "command", "command": "cargo install --path ." },
                    "audit_dir": "/home/user/.hone/audits"
                },
                {
                    "name": "legacy-tool",
                    "path": "/home/user/projects/legacy-tool",
                    "stack": "python",
                    "agent": "claude-sonnet-4-5",
                    "repo": "https://github.com/example/legacy-tool",
                    "branch": "main",
                    "skip": "unmaintained",
                    "actions": {
                        "iterate": false,
                        "maintain": false,
                        "push": false,
                        "audit": false,
                        "release": false
                    },
                    "install": null,
                    "audit_dir": null
                }
            ]
        }"#
    }

    #[test]
    fn deserializes_registry_from_json() {
        let registry: Registry = serde_json::from_str(sample_registry_json()).unwrap();
        assert_eq!(registry.version, 2);
        assert_eq!(registry.projects.len(), 2);
        assert_eq!(registry.projects[0].name, "hone-cli");
        assert!(registry.projects[0].actions.iterate);
        assert!(!registry.projects[1].actions.iterate);
    }

    #[test]
    fn active_projects_filters_skipped() {
        let registry: Registry = serde_json::from_str(sample_registry_json()).unwrap();
        let active = registry.active_projects();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "hone-cli");
    }

    #[test]
    fn load_reads_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(sample_registry_json().as_bytes()).unwrap();

        let registry = Registry::load(&path).unwrap();
        assert_eq!(registry.version, 2);
        assert_eq!(registry.projects.len(), 2);
    }
}
