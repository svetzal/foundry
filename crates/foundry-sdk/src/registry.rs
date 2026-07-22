use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::StoreError;

/// Deserialize `skip` as either a string reason, a boolean (`true` → `"skipped"`), or null.
fn deserialize_skip<'de, D>(deserializer: D) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    match value {
        Some(serde_json::Value::Bool(true)) => Ok(Some("skipped".to_string())),
        None | Some(serde_json::Value::Null | serde_json::Value::Bool(false)) => Ok(None),
        Some(serde_json::Value::String(s)) if s.is_empty() => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected string, bool, or null for skip, got {other}"
        ))),
    }
}

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
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when the file does not exist,
    /// [`StoreError::Io`] on read failure, and [`StoreError::Parse`] when
    /// the file contains malformed JSON.
    pub fn load(path: &Path) -> Result<Self, StoreError> {
        if !path.exists() {
            return Err(StoreError::NotFound {
                path: path.to_owned(),
            });
        }
        let content = std::fs::read_to_string(path).map_err(|source| StoreError::Io {
            path: path.to_owned(),
            source,
        })?;
        let registry = serde_json::from_str(&content).map_err(|source| StoreError::Parse {
            path: path.to_owned(),
            source,
        })?;
        Ok(registry)
    }

    /// Serialize the registry to a JSON file at the given path.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if parent directory creation or the file
    /// write fails, or [`StoreError::Parse`] if serialization fails (extremely
    /// rare in practice).
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
        let content = serde_json::to_string_pretty(self).map_err(|source| StoreError::Parse {
            path: path.to_owned(),
            source,
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, content).map_err(|source| StoreError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| StoreError::Io {
            path: path.to_owned(),
            source,
        })
    }

    /// Return only the projects that are not marked as skipped.
    pub fn active_projects(&self) -> Vec<&ProjectEntry> {
        self.projects.iter().filter(|p| p.skip.is_none()).collect()
    }

    /// Look up a project by name.
    pub fn find_project(&self, name: &str) -> Option<&ProjectEntry> {
        self.projects.iter().find(|p| p.name == name)
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
    /// When set, this project is excluded from runs. The string is the reason why.
    /// Absent or `null` means not skipped. Accepts `true`/`false` for backwards compatibility.
    #[serde(default, deserialize_with = "deserialize_skip")]
    pub skip: Option<String>,
    /// Which automation actions are enabled for this project.
    #[serde(default)]
    pub actions: ActionFlags,
    /// Optional local installation configuration.
    pub install: Option<InstallConfig>,
    /// Whether to run a skill-install command after the binary install step.
    ///
    /// `None` or `Default(false)` → no skill install (current behaviour unchanged).
    /// `Default(true)` → derive `<binary> init --global --force`.
    /// `Custom { command }` → run the specified command verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installs_skill: Option<InstallsSkill>,
    /// Human-readable notes about the project.
    pub notes: Option<String>,
    /// Per-project timeout in seconds for long-running commands.
    /// Defaults to 3600 (60 minutes) when absent.
    pub timeout_secs: Option<u64>,
    /// CVE/advisory IDs this project has formally accepted as not-applicable
    /// (e.g. an unreachable attack path). Matching vulnerabilities are suppressed
    /// from the post-push auditor's output. Record the rationale in `notes`, and
    /// remove the entry once the upstream advisory is patched. Absent/empty = no
    /// suppression (default, fully backwards compatible).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audit_exceptions: Vec<String>,
}

/// Default timeout for long-running commands: 60 minutes.
const DEFAULT_TIMEOUT_SECS: u64 = 3600;

impl ProjectEntry {
    /// Returns the configured timeout or the default (60 minutes).
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS))
    }
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
    Cpp,
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

/// Whether and how to run a skill-install command after the binary install step.
///
/// # JSON forms
///
/// - `true` — use the default derived command: `<binary> init --global --force`
/// - `false` — no skill install (same as absent)
/// - `{ "command": "..." }` — run this exact shell command
///
/// `false` and absent both resolve to "skip". Only `true` and the object form
/// trigger an actual invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InstallsSkill {
    /// Boolean shorthand: `true` derives the default command; `false` disables.
    Default(bool),
    /// Object form: run the specified command verbatim.
    Custom { command: String },
}

/// Derive the default skill-install command for a project.
///
/// If the project uses a Homebrew install with a non-empty formula, the formula
/// name is used as the binary; otherwise the project name is used. The suffix
/// ` init --global --force` is always appended.
///
/// # Examples
///
/// ```
/// use foundry_sdk::registry::{InstallConfig, derive_default_skill_install_command};
///
/// let cmd = derive_default_skill_install_command(
///     Some(&InstallConfig::Brew("gilt".to_string())),
///     "my-project",
/// );
/// assert_eq!(cmd, "gilt init --global --force");
///
/// let cmd = derive_default_skill_install_command(None, "my-project");
/// assert_eq!(cmd, "my-project init --global --force");
/// ```
pub fn derive_default_skill_install_command(
    install: Option<&InstallConfig>,
    project_name: &str,
) -> String {
    let binary = match install {
        Some(InstallConfig::Brew(formula)) if !formula.is_empty() => formula.as_str(),
        _ => project_name,
    };
    format!("{binary} init --global --force")
}

impl std::fmt::Display for Stack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rust => write!(f, "rust"),
            Self::Python => write!(f, "python"),
            Self::TypeScript => write!(f, "typescript"),
            Self::Elixir => write!(f, "elixir"),
            Self::Cpp => write!(f, "cpp"),
        }
    }
}

/// Parse a stack name, returning an error on unrecognised values.
///
/// # Errors
///
/// Returns `RegistryMutationError::InvalidStack` when the input string does not
/// match any known stack name.
pub fn parse_stack(s: &str) -> Result<Stack, RegistryMutationError> {
    serde_json::from_str(&format!("\"{s}\""))
        .map_err(|_| RegistryMutationError::InvalidStack(s.to_string()))
}

/// Errors that can occur when mutating the registry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryMutationError {
    /// A project with this name already exists.
    #[error("project '{0}' already exists in registry")]
    DuplicateName(String),
    /// No project with this name exists.
    #[error("project '{0}' not found in registry")]
    NotFound(String),
    /// The given stack name is not recognised.
    #[error("invalid stack '{0}'; use: rust, python, typescript, elixir, cpp")]
    InvalidStack(String),
    /// Both `install_command` and `install_brew` were provided; only one is allowed.
    #[error("provide at most one of install_command or install_brew")]
    ConflictingInstall,
}

/// All fields required when adding a new project.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct ProjectSpec {
    pub name: String,
    pub path: String,
    pub stack: Stack,
    pub agent: String,
    pub repo: String,
    pub branch: String,
    pub iterate: bool,
    pub maintain: bool,
    pub push: bool,
    pub audit: bool,
    pub release: bool,
    pub install_command: Option<String>,
    pub install_brew: Option<String>,
    pub notes: Option<String>,
    pub timeout_secs: Option<u64>,
}

/// Optional overrides when editing an existing project.
///
/// `None` on a field means "leave unchanged".
#[derive(Debug, Clone, Default)]
pub struct ProjectEdits {
    pub path: Option<String>,
    pub stack: Option<Stack>,
    pub agent: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// `Some(Some(reason))` sets skip; `Some(None)` clears skip; `None` leaves unchanged.
    pub skip: Option<Option<String>>,
    pub iterate: Option<bool>,
    pub maintain: Option<bool>,
    pub push: Option<bool>,
    pub audit: Option<bool>,
    pub release: Option<bool>,
    /// Set a command-based install. Mutually exclusive with `install_brew`.
    pub install_command: Option<String>,
    /// Set a Homebrew formula install. Mutually exclusive with `install_command`.
    pub install_brew: Option<String>,
    /// When `true`, clear the install config entirely (set to `None`).
    /// Takes precedence over `install_command` / `install_brew` when both are absent.
    pub clear_install: bool,
    /// `Some(s)` sets notes (empty string clears); `None` leaves unchanged.
    pub notes: Option<String>,
    /// `Some(v)` sets timeout; `None` leaves unchanged.
    pub timeout_secs: Option<u64>,
    /// When `true`, clear the timeout (revert to daemon default).
    pub clear_timeout: bool,
}

impl Registry {
    /// Add a new project to the registry.
    ///
    /// # Errors
    ///
    /// - `DuplicateName` if a project with the same name already exists.
    /// - `ConflictingInstall` if both `install_command` and `install_brew` are set.
    pub fn add_project(
        &mut self,
        spec: ProjectSpec,
    ) -> Result<&ProjectEntry, RegistryMutationError> {
        if self.projects.iter().any(|p| p.name == spec.name) {
            return Err(RegistryMutationError::DuplicateName(spec.name));
        }
        if spec.install_command.is_some() && spec.install_brew.is_some() {
            return Err(RegistryMutationError::ConflictingInstall);
        }
        let install = match (spec.install_command, spec.install_brew) {
            (Some(cmd), _) => Some(InstallConfig::Command(cmd)),
            (_, Some(formula)) => Some(InstallConfig::Brew(formula)),
            _ => None,
        };
        self.projects.push(ProjectEntry {
            name: spec.name,
            path: spec.path,
            stack: spec.stack,
            agent: spec.agent,
            repo: spec.repo,
            branch: spec.branch,
            skip: None,
            notes: spec.notes,
            actions: ActionFlags {
                iterate: spec.iterate,
                maintain: spec.maintain,
                push: spec.push,
                audit: spec.audit,
                release: spec.release,
            },
            install,
            installs_skill: None,
            timeout_secs: spec.timeout_secs,
            audit_exceptions: Vec::new(),
        });
        Ok(self.projects.last().expect("just pushed"))
    }

    /// Remove a project from the registry by name.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` if no project with the given name exists.
    pub fn remove_project(&mut self, name: &str) -> Result<(), RegistryMutationError> {
        let before = self.projects.len();
        self.projects.retain(|p| p.name != name);
        if self.projects.len() == before {
            return Err(RegistryMutationError::NotFound(name.to_string()));
        }
        Ok(())
    }

    /// Apply partial edits to an existing project.
    ///
    /// # Errors
    ///
    /// - `NotFound` if no project with the given name exists.
    /// - `ConflictingInstall` if both `install_command` and `install_brew` are set.
    pub fn edit_project(
        &mut self,
        name: &str,
        edits: ProjectEdits,
    ) -> Result<&ProjectEntry, RegistryMutationError> {
        if edits.install_command.is_some() && edits.install_brew.is_some() {
            return Err(RegistryMutationError::ConflictingInstall);
        }
        let project = self
            .projects
            .iter_mut()
            .find(|p| p.name == name)
            .ok_or_else(|| RegistryMutationError::NotFound(name.to_string()))?;

        if let Some(v) = edits.path {
            project.path = v;
        }
        if let Some(v) = edits.stack {
            project.stack = v;
        }
        if let Some(v) = edits.agent {
            project.agent = v;
        }
        if let Some(v) = edits.repo {
            project.repo = v;
        }
        if let Some(v) = edits.branch {
            project.branch = v;
        }
        if let Some(skip_val) = edits.skip {
            project.skip = skip_val;
        }
        if let Some(v) = edits.iterate {
            project.actions.iterate = v;
        }
        if let Some(v) = edits.maintain {
            project.actions.maintain = v;
        }
        if let Some(v) = edits.push {
            project.actions.push = v;
        }
        if let Some(v) = edits.audit {
            project.actions.audit = v;
        }
        if let Some(v) = edits.release {
            project.actions.release = v;
        }
        if let Some(cmd) = edits.install_command {
            project.install = Some(InstallConfig::Command(cmd));
        } else if let Some(formula) = edits.install_brew {
            project.install = Some(InstallConfig::Brew(formula));
        } else if edits.clear_install {
            project.install = None;
        }
        if let Some(v) = edits.notes {
            project.notes = if v.is_empty() { None } else { Some(v) };
        }
        if edits.clear_timeout {
            project.timeout_secs = None;
        } else if let Some(v) = edits.timeout_secs {
            project.timeout_secs = Some(v);
        }

        Ok(project)
    }
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
                "actions": {
                    "iterate": true,
                    "maintain": true,
                    "push": true,
                    "audit": true,
                    "release": false
                },
                "notes": "Test project",
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
        assert!(project.skip.is_none());
        assert_eq!(project.notes.as_deref(), Some("Test project"));

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
                        "branch": "main"
                    },
                    {
                        "name": "skipped",
                        "path": "/projects/skipped",
                        "stack": "python",
                        "agent": "claude",
                        "repo": "owner/skipped",
                        "branch": "main",
                        "skip": "Quality gates not configured"
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
        assert!(project.notes.is_some());
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
    fn save_and_load_round_trip() {
        let original: Registry = serde_json::from_str(FULL_REGISTRY_JSON).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");

        original.save(&path).unwrap();
        let loaded = Registry::load(&path).unwrap();

        assert_eq!(loaded.version, original.version);
        assert_eq!(loaded.projects.len(), original.projects.len());
        assert_eq!(loaded.projects[0].name, original.projects[0].name);
        assert_eq!(loaded.projects[0].stack, original.projects[0].stack);
    }

    #[test]
    fn save_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("registry.json");

        let registry = Registry {
            version: 2,
            projects: vec![],
        };
        registry.save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_replaces_file_atomically_without_leaving_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let original: Registry = serde_json::from_str(FULL_REGISTRY_JSON).unwrap();
        original.save(&path).unwrap();

        let updated = Registry {
            version: 2,
            projects: vec![],
        };
        updated.save(&path).unwrap();

        let loaded = Registry::load(&path).unwrap();
        assert_eq!(loaded.projects.len(), 0);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "atomic save should not leave a temporary registry file behind"
        );
    }

    #[test]
    fn load_missing_file_returns_not_found() {
        use crate::error::StoreError;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let err = Registry::load(&path).unwrap_err();
        assert!(
            matches!(err, StoreError::NotFound { .. }),
            "expected StoreError::NotFound, got {err:?}"
        );
    }

    #[test]
    fn load_malformed_file_returns_parse_error() {
        use crate::error::StoreError;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        std::fs::write(&path, "{ not valid json }").unwrap();
        let err = Registry::load(&path).unwrap_err();
        assert!(
            matches!(err, StoreError::Parse { .. }),
            "expected StoreError::Parse, got {err:?}"
        );
    }

    #[test]
    fn find_project_returns_some_for_known_name() {
        let registry: Registry = serde_json::from_str(FULL_REGISTRY_JSON).unwrap();
        let found = registry.find_project("my-project");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "my-project");
    }

    #[test]
    fn find_project_returns_none_for_unknown_name() {
        let registry: Registry = serde_json::from_str(FULL_REGISTRY_JSON).unwrap();
        assert!(registry.find_project("nonexistent").is_none());
    }

    #[test]
    fn installs_skill_bool_true_deserializes() {
        let json = r#"{
            "version": 2,
            "projects": [{
                "name": "p", "path": "/p", "stack": "rust",
                "agent": "claude", "repo": "o/p", "branch": "main",
                "installs_skill": true
            }]
        }"#;
        let registry: Registry = serde_json::from_str(json).unwrap();
        assert!(matches!(
            registry.projects[0].installs_skill,
            Some(InstallsSkill::Default(true))
        ));
    }

    #[test]
    fn installs_skill_bool_false_deserializes() {
        let json = r#"{
            "version": 2,
            "projects": [{
                "name": "p", "path": "/p", "stack": "rust",
                "agent": "claude", "repo": "o/p", "branch": "main",
                "installs_skill": false
            }]
        }"#;
        let registry: Registry = serde_json::from_str(json).unwrap();
        assert!(matches!(
            registry.projects[0].installs_skill,
            Some(InstallsSkill::Default(false))
        ));
    }

    #[test]
    fn installs_skill_object_form_deserializes() {
        let json = r#"{
            "version": 2,
            "projects": [{
                "name": "p", "path": "/p", "stack": "rust",
                "agent": "claude", "repo": "o/p", "branch": "main",
                "installs_skill": { "command": "gilt skill-init --global --force" }
            }]
        }"#;
        let registry: Registry = serde_json::from_str(json).unwrap();
        assert!(matches!(
            &registry.projects[0].installs_skill,
            Some(InstallsSkill::Custom { command }) if command == "gilt skill-init --global --force"
        ));
    }

    #[test]
    fn installs_skill_absent_deserializes_as_none() {
        let json = r#"{
            "version": 2,
            "projects": [{
                "name": "p", "path": "/p", "stack": "rust",
                "agent": "claude", "repo": "o/p", "branch": "main"
            }]
        }"#;
        let registry: Registry = serde_json::from_str(json).unwrap();
        assert!(registry.projects[0].installs_skill.is_none());
    }

    #[test]
    fn installs_skill_round_trips_bool_true() {
        let json = r#"{
            "version": 2,
            "projects": [{
                "name": "p", "path": "/p", "stack": "rust",
                "agent": "claude", "repo": "o/p", "branch": "main",
                "installs_skill": true
            }]
        }"#;
        let registry: Registry = serde_json::from_str(json).unwrap();
        let serialized = serde_json::to_string(&registry).unwrap();
        let restored: Registry = serde_json::from_str(&serialized).unwrap();
        assert!(matches!(
            restored.projects[0].installs_skill,
            Some(InstallsSkill::Default(true))
        ));
    }

    #[test]
    fn installs_skill_round_trips_object_form() {
        let json = r#"{
            "version": 2,
            "projects": [{
                "name": "p", "path": "/p", "stack": "rust",
                "agent": "claude", "repo": "o/p", "branch": "main",
                "installs_skill": { "command": "mytool init" }
            }]
        }"#;
        let registry: Registry = serde_json::from_str(json).unwrap();
        let serialized = serde_json::to_string(&registry).unwrap();
        let restored: Registry = serde_json::from_str(&serialized).unwrap();
        assert!(matches!(
            &restored.projects[0].installs_skill,
            Some(InstallsSkill::Custom { command }) if command == "mytool init"
        ));
    }

    #[test]
    fn derive_default_skill_install_command_brew_formula() {
        let cmd = derive_default_skill_install_command(
            Some(&InstallConfig::Brew("gilt".to_string())),
            "my-project",
        );
        assert_eq!(cmd, "gilt init --global --force");
    }

    #[test]
    fn derive_default_skill_install_command_brew_empty_formula_falls_back_to_project_name() {
        let cmd = derive_default_skill_install_command(
            Some(&InstallConfig::Brew(String::new())),
            "my-project",
        );
        assert_eq!(cmd, "my-project init --global --force");
    }

    #[test]
    fn derive_default_skill_install_command_non_brew_install_uses_project_name() {
        let cmd = derive_default_skill_install_command(
            Some(&InstallConfig::Command("cargo install --path .".to_string())),
            "my-project",
        );
        assert_eq!(cmd, "my-project init --global --force");
    }

    #[test]
    fn derive_default_skill_install_command_no_install_uses_project_name() {
        let cmd = derive_default_skill_install_command(None, "my-project");
        assert_eq!(cmd, "my-project init --global --force");
    }

    #[test]
    fn all_stack_variants_deserialize() {
        for (json, expected) in [
            ("rust", Stack::Rust),
            ("python", Stack::Python),
            ("typescript", Stack::TypeScript),
            ("elixir", Stack::Elixir),
            ("cpp", Stack::Cpp),
        ] {
            let stack: Stack = serde_json::from_str(&format!(r#""{json}""#)).unwrap();
            assert_eq!(stack, expected);
        }
    }

    // --- parse_stack ---

    #[test]
    fn parse_stack_valid_values() {
        assert_eq!(parse_stack("rust").unwrap(), Stack::Rust);
        assert_eq!(parse_stack("python").unwrap(), Stack::Python);
        assert_eq!(parse_stack("typescript").unwrap(), Stack::TypeScript);
        assert_eq!(parse_stack("elixir").unwrap(), Stack::Elixir);
        assert_eq!(parse_stack("cpp").unwrap(), Stack::Cpp);
    }

    #[test]
    fn parse_stack_invalid_value_returns_error() {
        assert!(matches!(
            parse_stack("cobol"),
            Err(RegistryMutationError::InvalidStack(s)) if s == "cobol"
        ));
    }

    // --- Registry::add_project ---

    fn empty_registry() -> Registry {
        Registry {
            version: 2,
            projects: vec![],
        }
    }

    fn minimal_spec(name: &str) -> ProjectSpec {
        ProjectSpec {
            name: name.to_string(),
            path: format!("/projects/{name}"),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: format!("owner/{name}"),
            branch: "main".to_string(),
            iterate: false,
            maintain: false,
            push: false,
            audit: false,
            release: false,
            install_command: None,
            install_brew: None,
            notes: None,
            timeout_secs: None,
        }
    }

    #[test]
    fn add_project_succeeds_on_empty_registry() {
        let mut registry = empty_registry();
        let result = registry.add_project(minimal_spec("alpha"));
        assert!(result.is_ok());
        assert_eq!(registry.projects.len(), 1);
        assert_eq!(registry.projects[0].name, "alpha");
    }

    #[test]
    fn add_project_returns_reference_to_new_entry() {
        let mut registry = empty_registry();
        let entry = registry.add_project(minimal_spec("alpha")).unwrap();
        assert_eq!(entry.name, "alpha");
        assert_eq!(entry.stack, Stack::Rust);
    }

    #[test]
    fn add_project_duplicate_name_returns_error() {
        let mut registry = empty_registry();
        registry.add_project(minimal_spec("alpha")).unwrap();
        let err = registry.add_project(minimal_spec("alpha")).unwrap_err();
        assert!(matches!(err, RegistryMutationError::DuplicateName(n) if n == "alpha"));
    }

    #[test]
    fn add_project_conflicting_install_returns_error() {
        let mut registry = empty_registry();
        let spec = ProjectSpec {
            install_command: Some("cargo install --path .".to_string()),
            install_brew: Some("my-formula".to_string()),
            ..minimal_spec("alpha")
        };
        let err = registry.add_project(spec).unwrap_err();
        assert_eq!(err, RegistryMutationError::ConflictingInstall);
    }

    #[test]
    fn add_project_install_command_is_wired() {
        let mut registry = empty_registry();
        let spec = ProjectSpec {
            install_command: Some("cargo install --path .".to_string()),
            ..minimal_spec("alpha")
        };
        registry.add_project(spec).unwrap();
        assert!(
            matches!(&registry.projects[0].install, Some(InstallConfig::Command(c)) if c == "cargo install --path .")
        );
    }

    #[test]
    fn add_project_install_brew_is_wired() {
        let mut registry = empty_registry();
        let spec = ProjectSpec {
            install_brew: Some("my-formula".to_string()),
            ..minimal_spec("alpha")
        };
        registry.add_project(spec).unwrap();
        assert!(
            matches!(&registry.projects[0].install, Some(InstallConfig::Brew(f)) if f == "my-formula")
        );
    }

    // --- Registry::remove_project ---

    #[test]
    fn remove_project_removes_existing_entry() {
        let mut registry = empty_registry();
        registry.add_project(minimal_spec("alpha")).unwrap();
        registry.remove_project("alpha").unwrap();
        assert!(registry.projects.is_empty());
    }

    #[test]
    fn remove_project_not_found_returns_error() {
        let mut registry = empty_registry();
        let err = registry.remove_project("nonexistent").unwrap_err();
        assert!(matches!(err, RegistryMutationError::NotFound(n) if n == "nonexistent"));
    }

    // --- Registry::edit_project ---

    #[test]
    fn edit_project_updates_path() {
        let mut registry = empty_registry();
        registry.add_project(minimal_spec("alpha")).unwrap();
        registry
            .edit_project(
                "alpha",
                ProjectEdits {
                    path: Some("/new/path".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(registry.projects[0].path, "/new/path");
    }

    #[test]
    fn edit_project_updates_stack() {
        let mut registry = empty_registry();
        registry.add_project(minimal_spec("alpha")).unwrap();
        registry
            .edit_project(
                "alpha",
                ProjectEdits {
                    stack: Some(Stack::Python),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(registry.projects[0].stack, Stack::Python);
    }

    #[test]
    fn edit_project_sets_skip_reason() {
        let mut registry = empty_registry();
        registry.add_project(minimal_spec("alpha")).unwrap();
        registry
            .edit_project(
                "alpha",
                ProjectEdits {
                    skip: Some(Some("CI broken".to_string())),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(registry.projects[0].skip.as_deref(), Some("CI broken"));
    }

    #[test]
    fn edit_project_clears_skip() {
        let mut registry = empty_registry();
        let mut spec = minimal_spec("alpha");
        spec.notes = None;
        registry.add_project(spec).unwrap();
        registry.projects[0].skip = Some("old reason".to_string());
        registry
            .edit_project(
                "alpha",
                ProjectEdits {
                    skip: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(registry.projects[0].skip.is_none());
    }

    #[test]
    fn edit_project_not_found_returns_error() {
        let mut registry = empty_registry();
        let err = registry.edit_project("nonexistent", ProjectEdits::default()).unwrap_err();
        assert!(matches!(err, RegistryMutationError::NotFound(n) if n == "nonexistent"));
    }

    #[test]
    fn edit_project_conflicting_install_returns_error() {
        let mut registry = empty_registry();
        registry.add_project(minimal_spec("alpha")).unwrap();
        let err = registry
            .edit_project(
                "alpha",
                ProjectEdits {
                    install_command: Some("cmd".to_string()),
                    install_brew: Some("formula".to_string()),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert_eq!(err, RegistryMutationError::ConflictingInstall);
    }

    #[test]
    fn edit_project_updates_action_flags() {
        let mut registry = empty_registry();
        registry.add_project(minimal_spec("alpha")).unwrap();
        registry
            .edit_project(
                "alpha",
                ProjectEdits {
                    iterate: Some(true),
                    maintain: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(registry.projects[0].actions.iterate);
        assert!(registry.projects[0].actions.maintain);
    }

    #[test]
    fn edit_project_returns_reference_to_updated_entry() {
        let mut registry = empty_registry();
        registry.add_project(minimal_spec("alpha")).unwrap();
        let entry = registry
            .edit_project(
                "alpha",
                ProjectEdits {
                    agent: Some("gemini".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(entry.agent, "gemini");
    }

    // --- RegistryMutationError display ---

    #[test]
    fn mutation_error_display_duplicate_name() {
        let e = RegistryMutationError::DuplicateName("proj".to_string());
        assert!(e.to_string().contains("proj"));
        assert!(e.to_string().contains("already exists"));
    }

    #[test]
    fn mutation_error_display_not_found() {
        let e = RegistryMutationError::NotFound("proj".to_string());
        assert!(e.to_string().contains("proj"));
        assert!(e.to_string().contains("not found"));
    }

    #[test]
    fn mutation_error_display_invalid_stack() {
        let e = RegistryMutationError::InvalidStack("cobol".to_string());
        assert!(e.to_string().contains("cobol"));
    }

    #[test]
    fn mutation_error_display_conflicting_install() {
        let e = RegistryMutationError::ConflictingInstall;
        assert!(!e.to_string().is_empty());
    }
}
