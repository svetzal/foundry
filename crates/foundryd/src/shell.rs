use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use tokio::process::Command;

/// Default timeout for shell commands.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// The outcome of running a shell command.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub stdout: String,
    /// Captured standard error output. Available for diagnostics and future blocks.
    #[allow(dead_code)]
    pub stderr: String,
    pub exit_code: i32,
    pub success: bool,
}

/// Run `command` with `args` in `working_dir`, optionally with extra environment
/// variables and a custom timeout.
///
/// Returns `Ok(CommandResult)` on clean execution (even if the command exits
/// non-zero). Returns `Err` only on I/O or spawn failure.
pub async fn run(
    working_dir: &Path,
    command: &str,
    args: &[&str],
    env: Option<HashMap<String, String>>,
    timeout: Option<Duration>,
) -> anyhow::Result<CommandResult> {
    let timeout = timeout.unwrap_or(DEFAULT_TIMEOUT);

    tracing::info!(
        dir = %working_dir.display(),
        cmd = command,
        args = ?args,
        timeout_secs = timeout.as_secs(),
        "running shell command",
    );

    let mut cmd = Command::new(command);
    cmd.args(args)
        .current_dir(working_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Some(env_vars) = env {
        for (key, value) in env_vars {
            cmd.env(key, value);
        }
    }

    let child = cmd.spawn().with_context(|| format!("failed to spawn `{command}`"))?;

    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .with_context(|| format!("`{command}` timed out after {}s", timeout.as_secs()))?
        .with_context(|| format!("failed to wait for `{command}`"))?;

    let exit_code = output.status.code().unwrap_or(-1);
    let success = output.status.success();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    tracing::info!(exit_code, success, "shell command finished");

    Ok(CommandResult {
        stdout,
        stderr,
        exit_code,
        success,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir() -> PathBuf {
        std::env::temp_dir()
    }

    #[tokio::test]
    async fn successful_command_captures_stdout() {
        let result = run(&tmp_dir(), "echo", &["hello world"], None, None).await.unwrap();
        assert!(result.success);
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello world"));
    }

    #[tokio::test]
    async fn failed_command_returns_non_zero_exit() {
        let result = run(&tmp_dir(), "false", &[], None, None).await.unwrap();
        assert!(!result.success);
        assert_ne!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn timeout_returns_error() {
        let result =
            run(&tmp_dir(), "sleep", &["60"], None, Some(Duration::from_millis(100))).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("timed out"), "unexpected error: {msg}");
    }

    #[tokio::test]
    async fn env_vars_are_passed_to_command() {
        let mut env = HashMap::new();
        env.insert("FOUNDRY_TEST_VAR".to_string(), "hello_foundry".to_string());
        let result = run(&tmp_dir(), "sh", &["-c", "echo $FOUNDRY_TEST_VAR"], Some(env), None)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("hello_foundry"));
    }
}
