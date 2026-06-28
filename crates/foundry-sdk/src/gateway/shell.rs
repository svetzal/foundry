use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;

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

/// Abstracts over external process execution so that task blocks can be tested
/// without spawning real child processes.
pub trait ShellGateway: Send + Sync {
    fn run<'a>(
        &'a self,
        working_dir: &'a Path,
        command: &'a str,
        args: &'a [&'a str],
        env: Option<&'a [(String, String)]>,
        timeout: Option<Duration>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<CommandResult>> + Send + 'a>>;
}
