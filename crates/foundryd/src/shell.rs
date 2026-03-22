// This module provides a public async shell-runner API consumed by block
// implementations.  Dead-code warnings are suppressed here because callers are
// added incrementally in subsequent tasks.
#![allow(dead_code)]

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;

/// The default command timeout: 5 minutes.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// The result of running an external shell command.
#[derive(Debug, Clone)]
pub struct CommandResult {
    /// Captured standard output from the process.
    pub stdout: String,
    /// Captured standard error from the process.
    pub stderr: String,
    /// The process exit code. Defaults to `-1` if the process was killed or the
    /// exit status was unavailable.
    pub exit_code: i32,
    /// `true` when the process exited with code `0`.
    pub success: bool,
}

/// Run an external command asynchronously.
///
/// # Arguments
///
/// * `working_dir` — directory in which the command is executed.
/// * `command` — program name or path.
/// * `args` — arguments passed to the program.
/// * `env` — optional list of `(key, value)` environment-variable overrides
///   that are *added to* (not replacing) the inherited environment.
/// * `timeout` — optional maximum wall-clock time allowed; defaults to 5 minutes.
///
/// # Errors
///
/// Returns an error when the timeout elapses before the command completes, or
/// when the OS fails to spawn/wait on the child process.
pub async fn run(
    working_dir: &Path,
    command: &str,
    args: &[&str],
    env: Option<&[(String, String)]>,
    timeout: Option<Duration>,
) -> Result<CommandResult> {
    let timeout = timeout.unwrap_or(DEFAULT_TIMEOUT);

    tracing::info!(
        command,
        args = ?args,
        working_dir = %working_dir.display(),
        "running shell command",
    );

    let mut cmd = Command::new(command);
    cmd.current_dir(working_dir)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Ensure the child is killed when the `Child` handle is dropped (e.g.
        // on timeout).
        .kill_on_drop(true);

    if let Some(pairs) = env {
        for (key, value) in pairs {
            cmd.env(key, value);
        }
    }

    let child = cmd.spawn().with_context(|| format!("failed to spawn command: {command}"))?;

    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .with_context(|| {
            format!("command timed out after {:.1}s: {command}", timeout.as_secs_f64())
        })?
        .with_context(|| format!("failed to wait on command: {command}"))?;

    let exit_code = output.status.code().unwrap_or(-1);
    let success = output.status.success();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    tracing::info!(
        command,
        exit_code,
        success,
        stdout_bytes = output.stdout.len(),
        stderr_bytes = output.stderr.len(),
        "shell command finished",
    );

    Ok(CommandResult {
        stdout,
        stderr,
        exit_code,
        success,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    // --- unit / integration tests -------------------------------------------

    #[tokio::test]
    async fn successful_command_returns_stdout_and_exit_zero() {
        let result = run(&tmp(), "echo", &["hello world"], None, None)
            .await
            .expect("echo should succeed");

        assert!(result.success);
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello world"), "stdout: {}", result.stdout);
        assert!(result.stderr.is_empty(), "unexpected stderr: {}", result.stderr);
    }

    #[tokio::test]
    async fn failed_command_returns_non_zero_exit_code() {
        // `false` is a POSIX utility that always exits with status 1.
        let result = run(&tmp(), "false", &[], None, None).await.expect("false should be runnable");

        assert!(!result.success);
        assert_ne!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn command_stderr_is_captured() {
        // Write a message to stderr via the shell.
        let result = run(&tmp(), "sh", &["-c", "echo error_output >&2"], None, None)
            .await
            .expect("sh should succeed");

        assert!(result.success);
        assert!(result.stderr.contains("error_output"), "stderr: {}", result.stderr);
    }

    #[tokio::test]
    async fn command_with_both_stdout_and_stderr() {
        let result = run(&tmp(), "sh", &["-c", "echo out_msg; echo err_msg >&2"], None, None)
            .await
            .expect("sh should succeed");

        assert!(result.success);
        assert!(result.stdout.contains("out_msg"), "stdout: {}", result.stdout);
        assert!(result.stderr.contains("err_msg"), "stderr: {}", result.stderr);
    }

    #[tokio::test]
    async fn timeout_kills_process_and_returns_error() {
        // `sleep 60` will not finish within a 50 ms timeout.
        let err = run(&tmp(), "sleep", &["60"], None, Some(Duration::from_millis(50)))
            .await
            .expect_err("should have timed out");

        let msg = format!("{err:#}");
        assert!(msg.contains("timed out"), "expected timeout error, got: {msg}");
    }

    #[tokio::test]
    async fn command_not_found_returns_error() {
        let err = run(&tmp(), "this_command_does_not_exist_foundry", &[], None, None)
            .await
            .expect_err("should fail to spawn");

        let msg = format!("{err:#}");
        assert!(msg.contains("failed to spawn"), "expected spawn error, got: {msg}");
    }

    #[tokio::test]
    async fn env_vars_are_passed_to_child() {
        let env = vec![("FOUNDRY_TEST_VAR".to_string(), "hello_env".to_string())];
        let result = run(&tmp(), "sh", &["-c", "echo $FOUNDRY_TEST_VAR"], Some(&env), None)
            .await
            .expect("sh should succeed");

        assert!(result.success);
        assert!(result.stdout.contains("hello_env"), "stdout: {}", result.stdout);
    }

    #[tokio::test]
    async fn empty_output_produces_empty_strings() {
        // `true` produces no output.
        let result = run(&tmp(), "true", &[], None, None).await.expect("true should succeed");

        assert!(result.success);
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
    }
}
