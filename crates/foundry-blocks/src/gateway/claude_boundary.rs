//! Per-invocation Claude policy. File tools receive only the exact repository
//! edit allowance; shell writes are constrained by the OS sandbox.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::engine::RunFiles;
use super::{AgentAccess, AgentRequest};

pub(super) struct Boundary {
    pub(super) files: RunFiles,
    pub(super) settings_path: PathBuf,
    pub(super) scratch: PathBuf,
}

/// Validate the declaration against the directory the CLI actually receives.
fn declared_root(request: &AgentRequest) -> Result<PathBuf> {
    let root = request
        .working_dir
        .canonicalize()
        .context("cannot resolve Claude working directory")?;
    if let Some((_, declared)) = request.env.iter().find(|(key, _)| key == "FOUNDRY_WRITABLE_ROOT")
    {
        let declared = Path::new(declared)
            .canonicalize()
            .context("cannot resolve declared writable root")?;
        anyhow::ensure!(
            declared == root,
            "declared writable root differs from Claude working directory"
        );
    }
    ensure_plain_pattern(&root, "Claude writable root")?;
    Ok(root)
}

/// A path that goes into a permission rule must be UTF-8 and hold no
/// permission-pattern characters.
fn ensure_plain_pattern(path: &Path, what: &str) -> Result<()> {
    let text = path.to_str().with_context(|| format!("{what} must be UTF-8"))?;
    anyhow::ensure!(
        !text.contains(['*', '?', '[', ']', '(', ')', '{', '}', '\\', '\n', '\r']),
        "{what} must not contain permission-pattern characters"
    );
    Ok(())
}

/// The run's scratch directory the session may also write
/// (`FOUNDRY_SCRATCH_DIR`), resolved. It must be outside the repository.
fn declared_run_scratch(request: &AgentRequest, root: &Path) -> Result<Option<PathBuf>> {
    let Some((_, declared)) =
        request.env.iter().find(|(key, _)| key == foundry_sdk::paths::SCRATCH_DIR_ENV)
    else {
        return Ok(None);
    };
    let scratch = Path::new(declared)
        .canonicalize()
        .context("cannot resolve the run's scratch directory")?;
    anyhow::ensure!(
        !scratch.starts_with(root),
        "the run's scratch directory must be outside the selected repository"
    );
    ensure_plain_pattern(&scratch, "the run's scratch directory")?;
    Ok(Some(scratch))
}

fn settings(
    root: &Path,
    access: AgentAccess,
    scratch: &Path,
    policy_directory: &Path,
    run_scratch: Option<&Path>,
) -> Value {
    let mut policy = json!({
        "permissions": { "allow": ["Read", "Glob", "Grep", "Bash", "WebFetch", "WebSearch"], "deny": ["mcp__*"] },
        "disableAllHooks": true,
        "sandbox": { "enabled": true, "allowUnsandboxedCommands": false, "failIfUnavailable": true, "filesystem": {"denyWrite": [policy_directory]} }
    });
    if access == AgentAccess::ReadOnly {
        policy["permissions"]["deny"] = json!(["Edit", "Write", "NotebookEdit", "mcp__*"]);
        policy["sandbox"]["filesystem"] =
            json!({"denyWrite": [root, policy_directory], "allowWrite": [scratch]});
    } else {
        // Absolute Edit rules have two leading slashes. This is the only edit
        // allowance; dontAsk refuses every file write outside this exact tree.
        policy["permissions"]["allow"] = json!([
            format!("Edit(/{}/**)", root.display()),
            "Read",
            "Glob",
            "Grep",
            "Bash",
            "WebFetch",
            "WebSearch"
        ]);
        // The run's scratch directory is the one other place this session
        // writes: proof, blocker and command logs, outside the repository.
        if let Some(run_scratch) = run_scratch {
            if let Some(allow) = policy["permissions"]["allow"].as_array_mut() {
                allow.insert(1, json!(format!("Edit(/{}/**)", run_scratch.display())));
            }
            policy["sandbox"]["filesystem"]["allowWrite"] = json!([run_scratch]);
        }
    }
    policy
}

pub(super) fn prepare(request: &AgentRequest, session_id: &str) -> Result<Option<Boundary>> {
    if request.access != AgentAccess::ReadOnly
        && !request.env.iter().any(|(key, _)| key == "FOUNDRY_WRITABLE_ROOT")
    {
        return Ok(None);
    }
    let root = declared_root(request)?;
    let run_scratch = declared_run_scratch(request, &root)?;
    let files = RunFiles::create(
        foundry_sdk::paths::foundry_home().join("agent-sandboxes").join(session_id),
    )
    .context("cannot create private Claude invocation files")?;
    anyhow::ensure!(
        !files
            .directory
            .canonicalize()
            .context("cannot resolve private Claude invocation directory")?
            .starts_with(&root),
        "Claude invocation files must be outside the selected repository"
    );
    let scratch = files.directory.join("scratch");
    std::fs::create_dir(&scratch).context("cannot create Claude scratch directory")?;
    let policy_directory = files.directory.join("policy");
    std::fs::create_dir(&policy_directory)
        .context("cannot create immutable Claude policy directory")?;
    let settings_path = policy_directory.join("settings.json");
    std::fs::write(
        &settings_path,
        serde_json::to_vec(&settings(
            &root,
            request.access,
            &scratch,
            &policy_directory,
            run_scratch.as_deref(),
        ))?,
    )
    .context("cannot write Claude boundary settings")?;
    Ok(Some(Boundary {
        files,
        settings_path,
        scratch,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{AgentAccess, AgentRequest};

    #[test]
    fn writable_policy_grants_exact_root_without_widening_shell_roots() {
        let policy = settings(
            std::path::Path::new("/home/stacey/.foundry/worktrees/task"),
            AgentAccess::Full,
            std::path::Path::new("/home/stacey/.foundry/agent-sandboxes/id/scratch"),
            std::path::Path::new("/home/stacey/.foundry/agent-sandboxes/id/policy"),
            None,
        );
        assert_eq!(
            policy["permissions"]["allow"][0],
            "Edit(//home/stacey/.foundry/worktrees/task/**)"
        );
        assert_eq!(policy["sandbox"]["enabled"], true);
        assert_eq!(policy["sandbox"]["allowUnsandboxedCommands"], false);
        assert_eq!(policy["sandbox"]["failIfUnavailable"], true);
        assert!(policy["sandbox"]["filesystem"]["allowWrite"].is_null());
        assert_eq!(policy["disableAllHooks"], true);
    }

    #[test]
    fn review_blocks_file_tools_and_shell_writes_to_the_repository() {
        let policy = settings(
            std::path::Path::new("/repo"),
            AgentAccess::ReadOnly,
            std::path::Path::new("/scratch"),
            std::path::Path::new("/policy"),
            None,
        );
        assert!(
            policy["permissions"]["deny"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("Edit"))
        );
        assert!(
            policy["permissions"]["deny"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("Write"))
        );
        assert_eq!(
            policy["sandbox"]["filesystem"]["denyWrite"],
            serde_json::json!(["/repo", "/policy"])
        );
        assert_eq!(policy["sandbox"]["filesystem"]["allowWrite"], serde_json::json!(["/scratch"]));
    }

    #[test]
    fn writable_policy_also_grants_the_runs_scratch_directory() {
        let policy = settings(
            std::path::Path::new("/srv/foundry/worktrees/task"),
            AgentAccess::Full,
            std::path::Path::new("/srv/foundry/agent-sandboxes/id/scratch"),
            std::path::Path::new("/srv/foundry/agent-sandboxes/id/policy"),
            Some(std::path::Path::new("/srv/foundry/evidence/trace/attempt")),
        );
        assert_eq!(
            policy["permissions"]["allow"].as_array().unwrap()[0..2],
            [
                serde_json::json!("Edit(//srv/foundry/worktrees/task/**)"),
                serde_json::json!("Edit(//srv/foundry/evidence/trace/attempt/**)")
            ]
        );
        assert_eq!(
            policy["sandbox"]["filesystem"]["allowWrite"],
            serde_json::json!(["/srv/foundry/evidence/trace/attempt"])
        );
    }

    fn writable_request(work: &Path, scratch: &Path) -> AgentRequest {
        AgentRequest {
            prompt: "test".into(),
            project: "test".into(),
            working_dir: work.into(),
            access: AgentAccess::Full,
            tier: crate::gateway::ModelTier::Balanced,
            effort: crate::gateway::ReasoningEffort::Medium,
            agent_file: None,
            provider: None,
            env: vec![
                ("FOUNDRY_WRITABLE_ROOT".into(), work.to_string_lossy().into()),
                ("FOUNDRY_SCRATCH_DIR".into(), scratch.to_string_lossy().into()),
            ],
            timeout: std::time::Duration::ZERO,
            trace_id: None,
            requires_json: false,
        }
    }

    #[test]
    fn the_runs_scratch_directory_must_be_outside_the_repository() {
        let work = tempfile::tempdir().unwrap();
        let inside = work.path().join(".foundry");
        std::fs::create_dir(&inside).unwrap();
        let root = declared_root(&writable_request(work.path(), &inside)).unwrap();
        assert!(declared_run_scratch(&writable_request(work.path(), &inside), &root).is_err());

        let outside = tempfile::tempdir().unwrap();
        let declared =
            declared_run_scratch(&writable_request(work.path(), outside.path()), &root).unwrap();
        assert_eq!(declared, Some(outside.path().canonicalize().unwrap()));
    }

    #[test]
    fn private_run_files_are_removed_on_error_and_never_reused() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("run");
        {
            let _files = RunFiles::create(path.clone()).unwrap();
            std::fs::write(path.join("settings.json"), "policy").unwrap();
            assert!(RunFiles::create(path.clone()).is_err());
            assert_eq!(std::fs::read_to_string(path.join("settings.json")).unwrap(), "policy");
        }
        assert!(!path.exists());
    }

    #[test]
    fn declared_root_must_match_the_actual_working_directory() {
        let work = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let request = AgentRequest {
            prompt: "test".into(),
            project: "test".into(),
            working_dir: work.path().into(),
            access: AgentAccess::Full,
            tier: crate::gateway::ModelTier::Balanced,
            effort: crate::gateway::ReasoningEffort::Medium,
            agent_file: None,
            provider: None,
            env: vec![("FOUNDRY_WRITABLE_ROOT".into(), other.path().to_string_lossy().into())],
            timeout: std::time::Duration::ZERO,
            trace_id: None,
            requires_json: false,
        };
        assert!(declared_root(&request).is_err());
    }
}

#[cfg(all(test, target_os = "macos"))]
#[path = "claude_boundary_live.rs"]
mod live;
