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

/// One streaming agent run: the child to spawn, what to feed it, and where to
/// tee its stdout.
///
/// A struct rather than a parameter list because the run now carries an
/// optional stdin payload as well — see [`StreamRun::stdin`].
#[derive(Clone, Copy)]
pub struct StreamRun<'a> {
    pub working_dir: &'a Path,
    pub command: &'a str,
    pub args: &'a [&'a str],
    pub env: Option<&'a [(String, String)]>,
    /// Bytes to write to the child's stdin, which is then closed.
    ///
    /// `None` leaves stdin closed outright (`Stdio::null()`). That is the
    /// default because some agentic CLIs (notably `opencode run`) block forever
    /// after bootstrap on an inherited stdin that never closes.
    ///
    /// Supply `Some(..)` to hand the child a payload too large for a single
    /// argv element: Linux caps one argument at `MAX_ARG_STRLEN` (131072
    /// bytes), and exceeding it fails the spawn with `E2BIG`.
    pub stdin: Option<&'a [u8]>,
    pub timeout: Option<Duration>,
    pub log_path: &'a Path,
}

pub trait AgentStreamRunner: Send + Sync {
    /// Spawn `run.command` with `run.args` in `run.working_dir`, read stdout
    /// line by line, append every line (with trailing `\n`) to `run.log_path`,
    /// and return the collected lines + exit info.
    fn run<'a>(
        &'a self,
        run: StreamRun<'a>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentStreamOutcome>> + Send + 'a>>;
}

/// Production implementation: spawns a child process, reads stdout via
/// `tokio::io::BufReader::lines`, and tees each line to the supplied file.
pub struct ProcessAgentStreamRunner;

/// Write `payload` to the child's stdin on its own task, then close the pipe.
///
/// Its own task because a payload larger than the pipe buffer (64KB on Linux)
/// blocks until the child drains it, so writing inline would stall before
/// stdout is being read.
///
/// The write is best-effort: a child that exits before draining the prompt
/// breaks the pipe (`EPIPE`), and that is the child’s exit, not a runner
/// fault. Failing the run here would hide the real outcome behind
/// "failed to write prompt to child stdin" — no exit code, no stderr, no
/// interpretation. The caller’s `child.wait()` and drained stderr report the
/// outcome as they do for any other exit.
fn spawn_stdin_writer(
    mut pipe: tokio::process::ChildStdin,
    payload: Vec<u8>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let write = async {
            pipe.write_all(&payload).await?;
            pipe.shutdown().await
        };
        if let Err(e) = write.await {
            // Best-effort: see this function’s docs — the child’s own exit code
            // and stderr are the outcome worth reporting, not this `EPIPE`.
            tracing::warn!(
                error = %e,
                "failed to write prompt to child stdin; reporting the child’s own exit instead"
            );
        }
        // Drop closes the pipe, so the child sees EOF.
        drop(pipe);
    })
}

impl AgentStreamRunner for ProcessAgentStreamRunner {
    fn run<'a>(
        &'a self,
        run: StreamRun<'a>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentStreamOutcome>> + Send + 'a>> {
        Box::pin(async move {
            let StreamRun {
                working_dir,
                command,
                args,
                env,
                stdin,
                timeout,
                log_path,
            } = run;
            let timeout = timeout.unwrap_or(Duration::from_secs(300));

            if let Some(parent) = log_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("failed to create log dir {}", parent.display()))?;
            }

            let log_file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
                .await
                .with_context(|| format!("failed to open log file {}", log_path.display()))?;

            let mut cmd = Command::new(command);
            cmd.current_dir(working_dir)
                .args(args)
                // With no stdin payload, close stdin. Agentic CLIs run
                // non-interactively here, but some (notably `opencode run`) block
                // forever after bootstrap waiting on an inherited stdin that never
                // closes. With a payload, pipe it in and close the pipe right after
                // writing, which leaves the child seeing a normal EOF.
                .stdin(if stdin.is_some() {
                    std::process::Stdio::piped()
                } else {
                    std::process::Stdio::null()
                })
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);

            // Propagate the active block's span context to the child process as
            // a W3C `TRACEPARENT` env var. No-op when no span context is in
            // scope. Caller-provided env (below) wins if it sets TRACEPARENT.
            foundry_sdk::span_context::inject_traceparent(&mut cmd);

            if let Some(pairs) = env {
                for (k, v) in pairs {
                    cmd.env(k, v);
                }
            }

            // Carry the OS error text into the message. A prompt that exceeds
            // `MAX_ARG_STRLEN` fails here with `Argument list too long (os error 7)`,
            // and without the cause the record reads as an unexplained
            // "failed to spawn claude".
            let mut child =
                cmd.spawn().map_err(|e| anyhow::anyhow!("failed to spawn {command}: {e}"))?;

            let stdout = child.stdout.take().context("missing stdout pipe")?;
            let stderr = child.stderr.take().context("missing stderr pipe")?;

            // Feed stdin from its own task. A payload larger than the pipe buffer
            // (64KB on Linux) blocks until the child drains it, so writing inline
            // would stall before stdout is being read.
            let stdin_handle = match stdin {
                Some(bytes) => Some(spawn_stdin_writer(
                    child.stdin.take().context("missing stdin pipe")?,
                    bytes.to_vec(),
                )),
                None => None,
            };

            let mut reader = BufReader::new(stdout).lines();
            let mut lines: Vec<StreamedLine> = Vec::new();
            let mut bytes_written: u64 = 0;

            // Drain stderr on its OWN task. If stdout and stderr are merely
            // `tokio::join!`-ed on one task, a disk write on the stdout path
            // suspends the task and stops draining stderr until it resumes — so
            // a child that bursts to one pipe while the parent is busy on the
            // other can fill a 64KB pipe buffer and block mid-run. Independent
            // tasks drain both pipes continuously.
            let stderr_handle = tokio::spawn(async move {
                let mut stderr_reader = BufReader::new(stderr);
                let mut buf = String::new();
                stderr_reader.read_to_string(&mut buf).await?;
                Ok::<String, anyhow::Error>(buf)
            });

            // Buffer the log writes. Flushing to disk on every single line
            // throttled how fast we drained opencode's stdout pipe; the pipe
            // filled during output bursts and opencode blocked on the write,
            // hanging the whole run. BufWriter batches syscalls so the pipe is
            // drained promptly; we flush once at the end.
            let mut log_writer = tokio::io::BufWriter::new(log_file);

            let combined = async {
                while let Some(line) = reader.next_line().await? {
                    let with_newline = format!("{line}\n");
                    log_writer.write_all(with_newline.as_bytes()).await?;
                    bytes_written = bytes_written.saturating_add(with_newline.len() as u64);
                    lines.push(StreamedLine { raw: line });
                }
                log_writer.flush().await?;
                if let Some(handle) = stdin_handle {
                    handle.await.context("stdin writer task failed to join")?;
                }
                let exit = child.wait().await?;
                let stderr_text = stderr_handle.await??;
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

    /// A `sh -c` run with no env, no stdin and the default timeout.
    fn sh_run<'a>(working_dir: &'a Path, args: &'a [&'a str], log_path: &'a Path) -> StreamRun<'a> {
        StreamRun {
            working_dir,
            command: "sh",
            args,
            env: None,
            stdin: None,
            timeout: None,
            log_path,
        }
    }

    #[tokio::test]
    async fn streams_stdout_lines_and_writes_them_to_log() {
        let log = tmp_log();
        let dir = std::env::temp_dir();
        let runner = ProcessAgentStreamRunner;
        let outcome = runner
            .run(sh_run(
                dir.as_path(),
                &["-c", "printf 'line one\\nline two\\nline three\\n'"],
                log.as_path(),
            ))
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

    // --- stdin delivery -----------------------------------------------------

    /// A prompt larger than Linux's `MAX_ARG_STRLEN` (131072 bytes) cannot ride
    /// in a single argv element, but must reach the child intact over stdin.
    #[tokio::test]
    async fn delivers_a_stdin_payload_larger_than_max_arg_strlen_intact() {
        const MAX_ARG_STRLEN: usize = 131_072;
        let payload = "x".repeat(MAX_ARG_STRLEN + 4096);

        let log = tmp_log();
        let dir = std::env::temp_dir();
        let runner = ProcessAgentStreamRunner;
        let mut run = sh_run(dir.as_path(), &["-c", "wc -c"], log.as_path());
        run.stdin = Some(payload.as_bytes());
        let outcome = runner.run(run).await.expect("run should succeed");

        assert!(outcome.success, "outcome: {outcome:?}");
        let counted: usize = outcome
            .lines
            .first()
            .expect("wc should report a byte count")
            .raw
            .trim()
            .parse()
            .expect("wc output should be a number");
        assert_eq!(counted, payload.len());

        let _ = tokio::fs::remove_file(&log).await;
    }

    /// A child that exits without reading a prompt too large for the pipe buffer
    /// breaks the stdin pipe. That `EPIPE` must not become the run's error — the
    /// child's own exit code and stderr are the outcome worth reporting.
    #[tokio::test]
    async fn reports_the_child_exit_when_it_dies_before_draining_stdin() {
        const MAX_ARG_STRLEN: usize = 131_072;
        let payload = "x".repeat(MAX_ARG_STRLEN + 4096);

        let log = tmp_log();
        let dir = std::env::temp_dir();
        let runner = ProcessAgentStreamRunner;
        let mut run = sh_run(dir.as_path(), &["-c", "echo 'boom' >&2; exit 17"], log.as_path());
        run.stdin = Some(payload.as_bytes());
        let outcome = runner.run(run).await.expect("broken stdin pipe must not fail the run");

        assert!(!outcome.success, "outcome: {outcome:?}");
        assert_eq!(outcome.exit_code, 17);
        assert!(outcome.stderr.contains("boom"), "stderr: {}", outcome.stderr);

        let _ = tokio::fs::remove_file(&log).await;
    }

    #[tokio::test]
    async fn closes_stdin_when_no_payload_is_supplied() {
        let log = tmp_log();
        let dir = std::env::temp_dir();
        let runner = ProcessAgentStreamRunner;
        let outcome = runner
            .run(sh_run(dir.as_path(), &["-c", "wc -c"], log.as_path()))
            .await
            .expect("run should succeed");

        assert!(outcome.success);
        let counted: usize = outcome
            .lines
            .first()
            .expect("wc should report a byte count")
            .raw
            .trim()
            .parse()
            .expect("wc output should be a number");
        assert_eq!(counted, 0, "stdin should be closed and empty");

        let _ = tokio::fs::remove_file(&log).await;
    }

    #[tokio::test]
    async fn spawn_failure_message_carries_the_os_error() {
        let log = tmp_log();
        let dir = std::env::temp_dir();
        let runner = ProcessAgentStreamRunner;
        let err = runner
            .run(StreamRun {
                working_dir: dir.as_path(),
                command: "foundry-no-such-binary-6f2a",
                args: &[],
                env: None,
                stdin: None,
                timeout: None,
                log_path: log.as_path(),
            })
            .await
            .expect_err("spawning a missing binary should fail");

        let text = format!("{err}");
        assert!(text.starts_with("failed to spawn foundry-no-such-binary-6f2a: "), "got: {text}");
        assert!(text.contains("os error"), "spawn error should carry the OS cause; got: {text}");

        let _ = tokio::fs::remove_file(&log).await;
    }

    // --- TRACEPARENT propagation tests --------------------------------------

    #[tokio::test]
    async fn run_injects_traceparent_when_span_context_set() {
        use foundry_sdk::span_context::{SPAN_CONTEXT, SpanContext};

        let ctx = SpanContext {
            trace_id: "0123456789abcdef0123456789abcdef".to_string(),
            span_id: "fedcba9876543210".to_string(),
        };
        let expected = ctx.traceparent();

        let log = tmp_log();
        let dir = std::env::temp_dir();
        let runner = ProcessAgentStreamRunner;
        let outcome = SPAN_CONTEXT
            .scope(ctx, async {
                runner
                    .run(sh_run(dir.as_path(), &["-c", "printenv TRACEPARENT"], log.as_path()))
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
        let dir = std::env::temp_dir();
        let runner = ProcessAgentStreamRunner;
        let outcome = runner
            .run(sh_run(dir.as_path(), &["-c", "printenv TRACEPARENT || true"], log.as_path()))
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
