//! Streaming runner for agent invocations whose stdout must be captured
//! line-by-line and tee'd to a session log file as the process runs.

use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// A single line produced by an agent's streaming stdout, after writing to disk.
#[derive(Debug, Clone)]
pub struct StreamedLine {
    pub raw: String,
}

/// Outcome of a streaming agent run.
#[derive(Debug, Clone)]
pub struct AgentStreamOutcome {
    pub exit_code: i32,
    pub success: bool,
    pub stderr: String,
    pub bytes_written: u64,
    /// Lines collected during the run (parsed by the caller).
    pub lines: Vec<StreamedLine>,
}

pub trait AgentStreamRunner: Send + Sync {
    /// Spawn `command` with `args` in `working_dir`, read stdout line by line,
    /// append every line (with trailing `\n`) to `log_path`, and return the
    /// collected lines + exit info.
    fn run<'a>(
        &'a self,
        working_dir: &'a Path,
        command: &'a str,
        args: &'a [&'a str],
        env: Option<&'a [(String, String)]>,
        timeout: Option<Duration>,
        log_path: &'a Path,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentStreamOutcome>> + Send + 'a>>;
}

/// Production implementation: spawns a child process, reads stdout via
/// `tokio::io::BufReader::lines`, and tees each line to the supplied file.
pub struct ProcessAgentStreamRunner;

impl AgentStreamRunner for ProcessAgentStreamRunner {
    fn run<'a>(
        &'a self,
        working_dir: &'a Path,
        command: &'a str,
        args: &'a [&'a str],
        env: Option<&'a [(String, String)]>,
        timeout: Option<Duration>,
        log_path: &'a Path,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentStreamOutcome>> + Send + 'a>> {
        Box::pin(async move {
            let timeout = timeout.unwrap_or(Duration::from_secs(300));

            if let Some(parent) = log_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("failed to create log dir {}", parent.display()))?;
            }

            let mut log_file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
                .await
                .with_context(|| format!("failed to open log file {}", log_path.display()))?;

            let mut cmd = Command::new(command);
            cmd.current_dir(working_dir)
                .args(args)
                // Close stdin. Agentic CLIs run non-interactively here, but some
                // (notably `opencode run`) block forever after bootstrap waiting on
                // an inherited stdin that never closes. The `claude` CLI in
                // `--print -p` mode never reads stdin, so closing it is safe for both.
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);

            // Propagate the active block's span context to the child process as
            // a W3C `TRACEPARENT` env var. No-op when no span context is in
            // scope. Caller-provided env (below) wins if it sets TRACEPARENT.
            foundry_core::span_context::inject_traceparent(&mut cmd);

            if let Some(pairs) = env {
                for (k, v) in pairs {
                    cmd.env(k, v);
                }
            }

            let mut child = cmd.spawn().with_context(|| format!("failed to spawn {command}"))?;

            let stdout = child.stdout.take().context("missing stdout pipe")?;
            let stderr = child.stderr.take().context("missing stderr pipe")?;

            let mut reader = BufReader::new(stdout).lines();
            let mut lines: Vec<StreamedLine> = Vec::new();
            let mut bytes_written: u64 = 0;

            let read_loop = async {
                while let Some(line) = reader.next_line().await? {
                    let with_newline = format!("{line}\n");
                    log_file.write_all(with_newline.as_bytes()).await?;
                    log_file.flush().await?;
                    bytes_written = bytes_written.saturating_add(with_newline.len() as u64);
                    lines.push(StreamedLine { raw: line });
                }
                Ok::<(), anyhow::Error>(())
            };

            let stderr_collect = async {
                let mut stderr_reader = BufReader::new(stderr);
                let mut buf = String::new();
                stderr_reader.read_to_string(&mut buf).await?;
                Ok::<String, anyhow::Error>(buf)
            };

            let combined = async {
                let (read_res, stderr_res) = tokio::join!(read_loop, stderr_collect);
                read_res?;
                let stderr_text = stderr_res?;
                let exit = child.wait().await?;
                Ok::<(std::process::ExitStatus, String), anyhow::Error>((exit, stderr_text))
            };

            let (exit_status, stderr_text) =
                tokio::time::timeout(timeout, combined).await.with_context(|| {
                    format!("agent stream timed out after {:.1}s", timeout.as_secs_f64())
                })??;

            let exit_code = exit_status.code().unwrap_or(-1);
            let success = exit_status.success();

            Ok(AgentStreamOutcome {
                exit_code,
                success,
                stderr: stderr_text,
                bytes_written,
                lines,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_log() -> PathBuf {
        std::env::temp_dir().join(format!("agent-stream-test-{}.jsonl", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn streams_stdout_lines_and_writes_them_to_log() {
        let log = tmp_log();
        let runner = ProcessAgentStreamRunner;
        let outcome = runner
            .run(
                std::env::temp_dir().as_path(),
                "sh",
                &["-c", "printf 'line one\\nline two\\nline three\\n'"],
                None,
                None,
                log.as_path(),
            )
            .await
            .expect("run should succeed");

        assert!(outcome.success);
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.lines.len(), 3);
        assert_eq!(outcome.lines[0].raw, "line one");
        assert_eq!(outcome.lines[2].raw, "line three");

        let written = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(written, "line one\nline two\nline three\n");

        let _ = tokio::fs::remove_file(&log).await;
    }

    // --- TRACEPARENT propagation tests --------------------------------------

    #[tokio::test]
    async fn run_injects_traceparent_when_span_context_set() {
        use foundry_core::span_context::{SPAN_CONTEXT, SpanContext};

        let ctx = SpanContext {
            trace_id: "0123456789abcdef0123456789abcdef".to_string(),
            span_id: "fedcba9876543210".to_string(),
        };
        let expected = ctx.traceparent();

        let log = tmp_log();
        let runner = ProcessAgentStreamRunner;
        let outcome = SPAN_CONTEXT
            .scope(ctx, async {
                runner
                    .run(
                        std::env::temp_dir().as_path(),
                        "sh",
                        &["-c", "printenv TRACEPARENT"],
                        None,
                        None,
                        log.as_path(),
                    )
                    .await
            })
            .await
            .expect("run should succeed");

        assert!(outcome.success, "printenv should find TRACEPARENT; outcome: {outcome:?}");
        assert_eq!(outcome.lines.len(), 1);
        assert_eq!(outcome.lines[0].raw, expected);

        let _ = tokio::fs::remove_file(&log).await;
    }

    #[tokio::test]
    async fn run_does_not_set_traceparent_when_context_absent() {
        let log = tmp_log();
        let runner = ProcessAgentStreamRunner;
        let outcome = runner
            .run(
                std::env::temp_dir().as_path(),
                "sh",
                &["-c", "printenv TRACEPARENT || true"],
                None,
                None,
                log.as_path(),
            )
            .await
            .expect("run should succeed");

        // When no span context is active, no TRACEPARENT should be set.
        // `printenv` produces no output for an unset var; `|| true` keeps
        // success. The line reader skips empty trailing lines, so we just
        // assert nothing resembling a W3C traceparent shows up.
        assert!(
            outcome.lines.iter().all(|l| !l.raw.starts_with("00-")),
            "no TRACEPARENT should leak when context is unset; got: {:?}",
            outcome.lines
        );

        let _ = tokio::fs::remove_file(&log).await;
    }
}
