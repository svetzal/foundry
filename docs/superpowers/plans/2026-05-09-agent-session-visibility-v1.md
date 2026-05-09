# Foundry: Agent Session Visibility v1 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Foundry-launched Claude Code agent sessions observable by emitting lifecycle events and writing the per-session stream-json transcript to disk at `~/.foundry/agent-sessions/<session_id>.jsonl`.

**Architecture:** Switch the `claude` invocation in `ClaudeAgentGateway` to `--output-format stream-json --verbose --print`. Stream stdout line-by-line; tee each line to a session JSONL file while extracting the final assistant text into the existing `AgentResponse.stdout` contract. Emit `AgentSessionStarted` when the agent process starts and `AgentSessionEnded` when it exits. Both events flow through foundryd's existing `tokio::sync::broadcast::Sender<Event>` so the existing `Watch` gRPC stream picks them up automatically — **no proto schema change needed** (event types are wire-encoded as snake_case strings in the existing `event_type` field).

**Tech Stack:** Rust 2021, tokio, anyhow, serde, strum, tonic (gRPC), uuid.

**Source spec (canonical):** `~/Work/Operations/Planning/2026-05-09-foundry-agent-session-visibility-v1.md`. Mirror in `~/Work/Projects/Cross-Project-Initiatives/agent-session-visibility-v1-spec.md`. Read it before starting — this plan implements the "Foundry changes" section.

**Companion plan:** ops-visualizer's matching plan lives at `~/Work/Projects/Mojility/ops-visualizer/docs/superpowers/plans/2026-05-09-agent-session-visibility-v1.md`. **This plan must complete and merge to `main` before the ops-visualizer plan starts** — ops-visualizer regenerates Elixir bindings from the proto and depends on the new event types being broadcast by foundryd.

**Sync first:** before starting, run `git fetch origin && git pull --rebase origin main` from the foundry repo.

---

## File Structure

```
crates/foundry-core/src/
  event.rs                        # ADD: AgentSessionStarted, AgentSessionEnded variants
  payload.rs                      # ADD: AgentSessionStartedPayload, AgentSessionEndedPayload structs
  paths.rs                        # ADD: agent_sessions_dir() helper

crates/foundryd/src/
  agent_stream.rs                 # NEW: AgentStreamRunner trait + ProcessAgentStreamRunner
  gateway.rs                      # MODIFY: ClaudeAgentGateway gains session_log_dir + event_tx + stream runner
                                  #          invoke() now spawns with stream-json, tees, emits lifecycle events
  engine.rs                       # MODIFY (small): wire event_tx clone through to ClaudeAgentGateway construction
  main.rs                         # MODIFY (small): pass event_tx + ProcessAgentStreamRunner to engine init

book/src/reference/event-types.md # MODIFY: document new event types
```

**Branch model (per project AGENTS.md):** work on a feature branch off `main`, open a PR against `main` when done. Commit per-task using the messages in each step.

---

## Task 1: Add new EventType variants

**Files:**
- Modify: `crates/foundry-core/src/event.rs`

- [ ] **Step 1: Add a failing test asserting both new variants serialize to snake_case**

In `crates/foundry-core/src/event.rs`, inside the existing `mod tests` block, add:

```rust
#[test]
fn agent_session_started_serializes_snake_case() {
    let value = serde_json::to_value(&EventType::AgentSessionStarted).unwrap();
    assert_eq!(value, serde_json::json!("agent_session_started"));
}

#[test]
fn agent_session_ended_serializes_snake_case() {
    let value = serde_json::to_value(&EventType::AgentSessionEnded).unwrap();
    assert_eq!(value, serde_json::json!("agent_session_ended"));
}

#[test]
fn agent_session_event_type_round_trips_via_strum() {
    use std::str::FromStr;
    assert_eq!(
        EventType::from_str("agent_session_started").unwrap(),
        EventType::AgentSessionStarted
    );
    assert_eq!(
        EventType::from_str("agent_session_ended").unwrap(),
        EventType::AgentSessionEnded
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd ~/Work/Projects/Mojility/foundry
cargo test -p foundry-core agent_session
```

Expected: FAIL — `no variant or associated item named 'AgentSessionStarted'`.

- [ ] **Step 3: Add variants to the enum**

In `crates/foundry-core/src/event.rs`, in the `pub enum EventType` block, append a new section before the closing brace:

```rust
    // Agent session lifecycle (visibility for Foundry-launched agents)
    AgentSessionStarted,
    AgentSessionEnded,
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p foundry-core agent_session
```

Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/foundry-core/src/event.rs
git commit -m "foundry-core: add AgentSessionStarted/Ended event types"
```

---

## Task 2: Add payload structs

**Files:**
- Modify: `crates/foundry-core/src/payload.rs`

- [ ] **Step 1: Add a failing test asserting JSON shape for both payloads**

Append to `crates/foundry-core/src/payload.rs` (or to its existing `mod tests` if one exists; otherwise add `#[cfg(test)] mod tests { use super::*; … }`):

```rust
#[cfg(test)]
mod agent_session_payload_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn agent_session_started_payload_serializes_to_expected_json() {
        let payload = AgentSessionStartedPayload {
            session_id: "11111111-2222-3333-4444-555555555555".to_string(),
            agent_type: "claude-code".to_string(),
            project: "demo".to_string(),
            working_dir: PathBuf::from("/tmp/demo"),
            source_log_path: PathBuf::from("/home/u/.foundry/agent-sessions/11111111.jsonl"),
            capability: "coding".to_string(),
            access: "full".to_string(),
            started_at: "2026-05-09T12:00:00Z".to_string(),
            trace_id: "trace-abc".to_string(),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["session_id"], "11111111-2222-3333-4444-555555555555");
        assert_eq!(json["agent_type"], "claude-code");
        assert_eq!(json["project"], "demo");
        assert_eq!(json["working_dir"], "/tmp/demo");
        assert_eq!(json["source_log_path"], "/home/u/.foundry/agent-sessions/11111111.jsonl");
        assert_eq!(json["capability"], "coding");
        assert_eq!(json["access"], "full");
        assert_eq!(json["started_at"], "2026-05-09T12:00:00Z");
        assert_eq!(json["trace_id"], "trace-abc");
    }

    #[test]
    fn agent_session_ended_payload_serializes_to_expected_json() {
        let payload = AgentSessionEndedPayload {
            session_id: "11111111-2222-3333-4444-555555555555".to_string(),
            status: "ok".to_string(),
            exit_code: Some(0),
            ended_at: "2026-05-09T12:01:00Z".to_string(),
            bytes_written: 1234,
            error: None,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["session_id"], "11111111-2222-3333-4444-555555555555");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["ended_at"], "2026-05-09T12:01:00Z");
        assert_eq!(json["bytes_written"], 1234);
        assert!(json.get("error").is_none(), "error should be omitted when None");
    }

    #[test]
    fn agent_session_ended_payload_includes_error_when_set() {
        let payload = AgentSessionEndedPayload {
            session_id: "id".to_string(),
            status: "unavailable".to_string(),
            exit_code: None,
            ended_at: "2026-05-09T12:01:00Z".to_string(),
            bytes_written: 0,
            error: Some("spawn failed: claude not on PATH".to_string()),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["error"], "spawn failed: claude not on PATH");
        assert!(json.get("exit_code").is_none(), "exit_code should be omitted when None");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p foundry-core agent_session_payload
```

Expected: FAIL — payload structs don't exist.

- [ ] **Step 3: Add the structs**

In `crates/foundry-core/src/payload.rs`, append a new section at the end of the file (before any `#[cfg(test)] mod tests`):

```rust
// ---------------------------------------------------------------------------
// Agent session lifecycle payloads
// ---------------------------------------------------------------------------

use std::path::PathBuf;

/// Emitted when a Foundry-launched agent session begins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionStartedPayload {
    pub session_id: String,
    pub agent_type: String,
    pub project: String,
    pub working_dir: PathBuf,
    pub source_log_path: PathBuf,
    pub capability: String,
    pub access: String,
    pub started_at: String,
    pub trace_id: String,
}

/// Emitted when a Foundry-launched agent session ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionEndedPayload {
    pub session_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub ended_at: String,
    pub bytes_written: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
```

If `use std::path::PathBuf;` is already imported elsewhere in the file, omit the duplicate.

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p foundry-core agent_session_payload
```

Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/foundry-core/src/payload.rs
git commit -m "foundry-core: add AgentSessionStarted/Ended payload structs"
```

---

## Task 3: Add `agent_sessions_dir()` path helper

**Files:**
- Modify: `crates/foundry-core/src/paths.rs`

- [ ] **Step 1: Add a failing test for the helper**

Append to `crates/foundry-core/src/paths.rs` `mod tests` (or create one):

```rust
#[cfg(test)]
mod agent_sessions_dir_tests {
    use super::*;

    #[test]
    fn agent_sessions_dir_is_under_foundry_home() {
        let dir = agent_sessions_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with(".foundry/agent-sessions"), "got: {}", s);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p foundry-core agent_sessions_dir
```

Expected: FAIL — `agent_sessions_dir` not found.

- [ ] **Step 3: Add the helper**

In `crates/foundry-core/src/paths.rs`, add a public function (mirror existing `events_dir()` or similar pattern in that file — check the file first, the pattern there should be obvious):

```rust
/// Returns the directory holding per-session agent transcript JSONL files.
/// Defaults to `$HOME/.foundry/agent-sessions`.
pub fn agent_sessions_dir() -> std::path::PathBuf {
    foundry_home().join("agent-sessions")
}
```

If `foundry_home()` is not the existing helper name, use whichever helper returns `~/.foundry/`.

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p foundry-core agent_sessions_dir
```

Expected: 1 test passes.

- [ ] **Step 5: Commit**

```bash
git add crates/foundry-core/src/paths.rs
git commit -m "foundry-core: add agent_sessions_dir() path helper"
```

---

## Task 4: Define `AgentStreamRunner` trait and process implementation

**Files:**
- Create: `crates/foundryd/src/agent_stream.rs`
- Modify: `crates/foundryd/src/lib.rs` (or wherever module declarations live — check `crates/foundryd/src/main.rs` for `mod` declarations)

**Why:** The existing `ShellGateway::run` buffers stdout and returns at the end. We need streaming: read stdout line by line so we can write each line to a JSONL file *while* the agent is running, and so that the UI can tail the file in real time. We isolate this in its own trait rather than expand `ShellGateway`, to keep the existing `ShellGateway` API stable.

- [ ] **Step 1: Write a failing test for `ProcessAgentStreamRunner` against a real `cat` process**

Create `crates/foundryd/src/agent_stream.rs`:

```rust
//! Streaming runner for agent invocations whose stdout must be captured
//! line-by-line and tee'd to a session log file as the process runs.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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

            // Ensure parent directory exists.
            if let Some(parent) = log_path.parent() {
                tokio::fs::create_dir_all(parent).await.with_context(|| {
                    format!("failed to create log dir {}", parent.display())
                })?;
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
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);

            if let Some(pairs) = env {
                for (k, v) in pairs {
                    cmd.env(k, v);
                }
            }

            let mut child = cmd.spawn()
                .with_context(|| format!("failed to spawn {}", command))?;

            let stdout = child.stdout.take().context("missing stdout pipe")?;
            let stderr = child.stderr.take().context("missing stderr pipe")?;

            let mut reader = BufReader::new(stdout).lines();
            let mut lines: Vec<StreamedLine> = Vec::new();
            let mut bytes_written: u64 = 0;

            // Drive the reader and the child to completion under the timeout.
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
                let exit = child.wait().await?;
                Ok::<(std::process::ExitStatus, String), anyhow::Error>((
                    exit,
                    match (read_res, stderr_res) {
                        (Ok(_), Ok(s)) => s,
                        (Err(e), _) | (_, Err(e)) => return Err(e),
                    },
                ))
            };

            let (exit_status, stderr_text) = tokio::time::timeout(timeout, combined)
                .await
                .with_context(|| {
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

// Required for read_to_string in the impl above.
use tokio::io::AsyncReadExt;

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
}
```

Add `mod agent_stream;` and `pub use agent_stream::*;` (or appropriate visibility) to `crates/foundryd/src/main.rs` (or `lib.rs` if one exists).

If `uuid` is not already a dependency of `foundryd`, add it to `crates/foundryd/Cargo.toml`:

```toml
uuid = { version = "1", features = ["v4"] }
```

- [ ] **Step 2: Run test to verify it fails (compile error first, then test failure)**

```bash
cargo test -p foundryd agent_stream
```

Expected: initially compile errors as you wire `mod agent_stream;`. After it compiles, the test should pass on the first run since it tests a real process. If `sh` isn't available on the runner: substitute `bash`, or use `echo` as the command.

- [ ] **Step 3: Verify the file is correct and compiles**

```bash
cargo build -p foundryd
```

Expected: clean build.

- [ ] **Step 4: Run all foundryd tests to ensure nothing else broke**

```bash
cargo test -p foundryd
```

Expected: all existing tests still pass.

- [ ] **Step 5: Commit**

```bash
git add crates/foundryd/src/agent_stream.rs crates/foundryd/src/main.rs crates/foundryd/Cargo.toml
git commit -m "foundryd: add AgentStreamRunner trait + ProcessAgentStreamRunner"
```

---

## Task 5: Refactor `ClaudeAgentGateway` to use stream-json + emit lifecycle events

**Files:**
- Modify: `crates/foundryd/src/gateway.rs`

**Approach:** `ClaudeAgentGateway` gains:

- `session_log_dir: PathBuf` (defaults to `foundry_core::paths::agent_sessions_dir()`).
- `event_tx: tokio::sync::broadcast::Sender<foundry_core::event::Event>` (clone of foundryd's broadcast channel).
- `stream_runner: Arc<dyn AgentStreamRunner>`.

`invoke()` flow:

1. Mint `session_id = uuid::Uuid::new_v4().to_string()`.
2. Compute `log_path = session_log_dir.join(format!("{session_id}.jsonl"))`.
3. Send `AgentSessionStarted` event on `event_tx` (best-effort — `let _ = event_tx.send(...)`).
4. Call `stream_runner.run(...)` with args modified to include `--output-format stream-json --verbose` (in addition to existing `--print`, `--model`, etc).
5. Walk `outcome.lines`, for each line attempt `serde_json::from_str::<serde_json::Value>(&line.raw)`. The final assistant text is the `result` field of the line whose `type == "result"` (claude CLI's stream-json schema). Concatenate text content from `assistant`-type messages as a fallback. Build final `stdout`.
6. Send `AgentSessionEnded` event with `status` derived from `outcome.success` (`"ok"`) vs failure (`"agent_failed"`).
7. Return `AgentResponse { stdout: extracted, stderr: outcome.stderr, exit_code: outcome.exit_code, success: outcome.success }`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] pub mod fakes` in `crates/foundryd/src/gateway.rs` (or a new test module — pick whichever fits the existing pattern):

```rust
#[cfg(test)]
mod claude_agent_gateway_streaming_tests {
    use super::*;
    use crate::agent_stream::{AgentStreamOutcome, AgentStreamRunner, StreamedLine};
    use foundry_core::event::EventType;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::broadcast;

    /// Test fake: returns canned outcome and writes a canned transcript to log_path.
    struct FakeAgentStreamRunner {
        transcript: Vec<String>,
        outcome_template: AgentStreamOutcome,
    }

    impl AgentStreamRunner for FakeAgentStreamRunner {
        fn run<'a>(
            &'a self,
            _working_dir: &'a Path,
            _command: &'a str,
            _args: &'a [&'a str],
            _env: Option<&'a [(String, String)]>,
            _timeout: Option<Duration>,
            log_path: &'a Path,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<AgentStreamOutcome>> + Send + 'a>>
        {
            let transcript = self.transcript.clone();
            let mut template = self.outcome_template.clone();
            Box::pin(async move {
                if let Some(parent) = log_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                let mut file = tokio::fs::File::create(log_path).await?;
                use tokio::io::AsyncWriteExt;
                let mut bytes: u64 = 0;
                let mut lines = Vec::new();
                for line in transcript {
                    let s = format!("{line}\n");
                    file.write_all(s.as_bytes()).await?;
                    bytes += s.len() as u64;
                    lines.push(StreamedLine { raw: line });
                }
                file.flush().await?;
                template.bytes_written = bytes;
                template.lines = lines;
                Ok(template)
            })
        }
    }

    fn tmp_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("foundry-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn invoke_emits_started_then_ended_and_writes_transcript() {
        let session_log_dir = tmp_dir();
        let (tx, mut rx) = broadcast::channel(16);

        let transcript = vec![
            r#"{"type":"system","subtype":"init","cwd":"/tmp"}"#.to_string(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#.to_string(),
            r#"{"type":"result","subtype":"success","result":"Final answer."}"#.to_string(),
        ];

        let runner = Arc::new(FakeAgentStreamRunner {
            transcript: transcript.clone(),
            outcome_template: AgentStreamOutcome {
                exit_code: 0,
                success: true,
                stderr: String::new(),
                bytes_written: 0, // overwritten by fake
                lines: vec![],     // overwritten by fake
            },
        });

        // FakeShellGateway is unused by the streaming gateway; pass any.
        let shell = FakeShellGateway::success();
        let gateway = ClaudeAgentGateway::new_with_streaming(
            shell,
            runner,
            session_log_dir.clone(),
            tx,
        );

        let request = AgentRequest {
            prompt: "say hi".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::Full,
            capability: AgentCapability::Coding,
            agent_file: None,
            timeout: Duration::from_secs(60),
        };

        let response = gateway.invoke(&request).await.expect("invoke ok");
        assert!(response.success);
        assert_eq!(response.exit_code, 0);
        assert_eq!(response.stdout, "Final answer.");

        // First broadcast: AgentSessionStarted; second: AgentSessionEnded.
        let started = rx.recv().await.expect("started event");
        assert_eq!(started.event_type, EventType::AgentSessionStarted);
        assert_eq!(started.payload["agent_type"], "claude-code");
        assert_eq!(started.payload["capability"], "coding");
        assert_eq!(started.payload["access"], "full");
        let session_id = started.payload["session_id"].as_str().unwrap().to_string();
        assert!(!session_id.is_empty());
        let log_path = started.payload["source_log_path"].as_str().unwrap();
        assert!(log_path.ends_with(".jsonl"));

        let ended = rx.recv().await.expect("ended event");
        assert_eq!(ended.event_type, EventType::AgentSessionEnded);
        assert_eq!(ended.payload["session_id"], session_id);
        assert_eq!(ended.payload["status"], "ok");
        assert_eq!(ended.payload["exit_code"], 0);
        assert!(ended.payload["bytes_written"].as_u64().unwrap() > 0);

        // The transcript file exists and contains exactly the canned lines.
        let written = tokio::fs::read_to_string(log_path).await.unwrap();
        let expected = transcript.iter().map(|l| format!("{l}\n")).collect::<String>();
        assert_eq!(written, expected);
    }

    #[tokio::test]
    async fn invoke_marks_session_as_agent_failed_on_nonzero_exit() {
        let session_log_dir = tmp_dir();
        let (tx, mut rx) = broadcast::channel(16);

        let runner = Arc::new(FakeAgentStreamRunner {
            transcript: vec![r#"{"type":"system","subtype":"init"}"#.to_string()],
            outcome_template: AgentStreamOutcome {
                exit_code: 2,
                success: false,
                stderr: "boom".to_string(),
                bytes_written: 0,
                lines: vec![],
            },
        });

        let shell = FakeShellGateway::success();
        let gateway = ClaudeAgentGateway::new_with_streaming(
            shell,
            runner,
            session_log_dir,
            tx,
        );

        let request = AgentRequest {
            prompt: "fail please".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::Full,
            capability: AgentCapability::Coding,
            agent_file: None,
            timeout: Duration::from_secs(5),
        };

        let response = gateway.invoke(&request).await.expect("invoke ok");
        assert!(!response.success);
        assert_eq!(response.exit_code, 2);

        let _started = rx.recv().await.unwrap();
        let ended = rx.recv().await.unwrap();
        assert_eq!(ended.event_type, EventType::AgentSessionEnded);
        assert_eq!(ended.payload["status"], "agent_failed");
        assert_eq!(ended.payload["exit_code"], 2);
    }

    #[tokio::test]
    async fn invoke_includes_stream_json_flags_in_args() {
        // Smoke-asserts via the FakeShellGateway-recorded args once the impl wires through.
        // Implementer: capture args via a custom AgentStreamRunner that records them, OR
        // verify by reading the produced log path.
        // (Left as part of the implementation; the assertion above on `--output-format stream-json --verbose`
        //  must appear in the produced args.)
    }
}
```

(The third test is intentionally left as a placeholder hook — fill it in when wiring args through. Replace its body with a real captured-args assertion before completing the task.)

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p foundryd claude_agent_gateway_streaming
```

Expected: FAIL — `new_with_streaming` constructor doesn't exist; the existing `invoke` doesn't emit events.

- [ ] **Step 3: Implement the streaming gateway**

Replace the existing `ClaudeAgentGateway` in `crates/foundryd/src/gateway.rs`. Keep the existing `new(shell)` constructor for backwards compatibility with tests that don't care about streaming, but have it construct with a `ProcessAgentStreamRunner`, the default `agent_sessions_dir()`, and a no-op broadcast channel (a freshly-created sender with no receivers — sends are no-ops).

```rust
use std::sync::Arc;
use std::time::Duration;
use std::path::PathBuf;
use chrono::Utc;
use tokio::sync::broadcast;
use uuid::Uuid;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{AgentSessionEndedPayload, AgentSessionStartedPayload};
use foundry_core::throttle::Throttle;
use foundry_core::paths;

use crate::agent_stream::{AgentStreamRunner, ProcessAgentStreamRunner};

pub struct ClaudeAgentGateway {
    shell: Arc<dyn ShellGateway>,
    stream_runner: Arc<dyn AgentStreamRunner>,
    session_log_dir: PathBuf,
    event_tx: broadcast::Sender<Event>,
}

impl ClaudeAgentGateway {
    /// Backwards-compatible constructor. Uses default session log dir and a
    /// broadcast channel with no receivers (events emitted into the void).
    pub fn new(shell: Arc<dyn ShellGateway>) -> Self {
        let (event_tx, _) = broadcast::channel(16);
        Self::new_with_streaming(
            shell,
            Arc::new(ProcessAgentStreamRunner),
            paths::agent_sessions_dir(),
            event_tx,
        )
    }

    pub fn new_with_streaming(
        shell: Arc<dyn ShellGateway>,
        stream_runner: Arc<dyn AgentStreamRunner>,
        session_log_dir: PathBuf,
        event_tx: broadcast::Sender<Event>,
    ) -> Self {
        Self { shell, stream_runner, session_log_dir, event_tx }
    }

    fn model_for(capability: AgentCapability) -> &'static str {
        match capability {
            AgentCapability::Reasoning => "claude-opus-4-6",
            AgentCapability::Coding => "claude-sonnet-4-6",
            AgentCapability::Quick => "claude-haiku-4-5-20251001",
        }
    }

    fn capability_label(c: AgentCapability) -> &'static str {
        match c {
            AgentCapability::Reasoning => "reasoning",
            AgentCapability::Coding => "coding",
            AgentCapability::Quick => "quick",
        }
    }

    fn access_label(a: AgentAccess) -> &'static str {
        match a {
            AgentAccess::ReadOnly => "read_only",
            AgentAccess::Full => "full",
        }
    }
}

impl AgentGateway for ClaudeAgentGateway {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            let _ = &self.shell; // keep field alive — may be used by future paths
            let session_id = Uuid::new_v4().to_string();
            let log_path = self.session_log_dir.join(format!("{session_id}.jsonl"));

            // Build args.
            let model = Self::model_for(request.capability);
            let mut args: Vec<String> = vec![
                "--print".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
                "--model".to_string(),
                model.to_string(),
            ];
            if let Some(ref agent_file) = request.agent_file {
                args.push("--agent".to_string());
                args.push(agent_file.display().to_string());
            }
            if request.access == AgentAccess::ReadOnly {
                args.push("--allowedTools".to_string());
                args.push("Read Glob Grep WebFetch WebSearch".to_string());
            }
            args.push("--dangerously-skip-permissions".to_string());
            args.push("-p".to_string());
            args.push(request.prompt.clone());
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            // Emit AgentSessionStarted.
            let started_payload = AgentSessionStartedPayload {
                session_id: session_id.clone(),
                agent_type: "claude-code".to_string(),
                project: String::new(), // populated by caller-aware future revision; v1: empty
                working_dir: request.working_dir.clone(),
                source_log_path: log_path.clone(),
                capability: Self::capability_label(request.capability).to_string(),
                access: Self::access_label(request.access).to_string(),
                started_at: Utc::now().to_rfc3339(),
                trace_id: String::new(), // see note below
            };
            let started_event = Event::new(
                EventType::AgentSessionStarted,
                String::new(), // project unknown to gateway
                Throttle::Full,
                serde_json::to_value(&started_payload)?,
            );
            let _ = self.event_tx.send(started_event);

            // Run.
            let env = vec![("CLAUDECODE".to_string(), String::new())];
            let outcome = self.stream_runner.run(
                &request.working_dir,
                "claude",
                &arg_refs,
                Some(&env),
                Some(request.timeout),
                &log_path,
            ).await;

            let (status, exit_code, stderr, bytes, error_msg, stdout_text) = match outcome {
                Ok(o) => {
                    let extracted = extract_final_text(&o.lines);
                    let status = if o.success { "ok" } else { "agent_failed" };
                    (status, Some(o.exit_code), o.stderr, o.bytes_written, None, extracted)
                }
                Err(e) => (
                    "unavailable",
                    None,
                    String::new(),
                    0u64,
                    Some(e.to_string()),
                    String::new(),
                ),
            };

            let ended_payload = AgentSessionEndedPayload {
                session_id: session_id.clone(),
                status: status.to_string(),
                exit_code,
                ended_at: Utc::now().to_rfc3339(),
                bytes_written: bytes,
                error: error_msg,
            };
            let ended_event = Event::new(
                EventType::AgentSessionEnded,
                String::new(),
                Throttle::Full,
                serde_json::to_value(&ended_payload)?,
            );
            let _ = self.event_tx.send(ended_event);

            Ok(AgentResponse {
                stdout: stdout_text,
                stderr,
                exit_code: exit_code.unwrap_or(-1),
                success: status == "ok",
            })
        })
    }
}

/// Extract the final assistant text from a stream-json transcript.
/// Prefers a `{"type":"result", ..., "result":"…"}` envelope; falls back to
/// concatenation of `{"type":"assistant", "message":{"content":[{"type":"text","text":"…"}…]}}` entries.
fn extract_final_text(lines: &[crate::agent_stream::StreamedLine]) -> String {
    use serde_json::Value;
    // First pass: look for a result envelope.
    for line in lines.iter().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(&line.raw) {
            if v.get("type").and_then(Value::as_str) == Some("result") {
                if let Some(s) = v.get("result").and_then(Value::as_str) {
                    return s.to_string();
                }
            }
        }
    }
    // Fallback: concatenate assistant text content blocks.
    let mut out = String::new();
    for line in lines {
        if let Ok(v) = serde_json::from_str::<Value>(&line.raw) {
            if v.get("type").and_then(Value::as_str) == Some("assistant") {
                if let Some(content) = v.pointer("/message/content").and_then(Value::as_array) {
                    for block in content {
                        if block.get("type").and_then(Value::as_str) == Some("text") {
                            if let Some(t) = block.get("text").and_then(Value::as_str) {
                                if !out.is_empty() {
                                    out.push('\n');
                                }
                                out.push_str(t);
                            }
                        }
                    }
                }
            }
        }
    }
    out
}
```

If any imports are missing for `chrono`, `uuid`, or `tokio::sync::broadcast`, add them to `crates/foundryd/Cargo.toml`. (`chrono` and `tokio` are likely already present; `uuid` was added in Task 4.)

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p foundryd claude_agent_gateway_streaming
```

Expected: 2 passing tests (the third is a placeholder — fill it in next).

- [ ] **Step 5: Add the captured-args test**

Replace the placeholder `invoke_includes_stream_json_flags_in_args` test with one that verifies args. Easiest path: add a `Mutex<Vec<String>>` to `FakeAgentStreamRunner` that records `args`, and assert in the test:

```rust
#[tokio::test]
async fn invoke_includes_stream_json_flags_in_args() {
    use std::sync::Mutex;

    struct ArgRecorder {
        recorded: Arc<Mutex<Vec<String>>>,
    }

    impl AgentStreamRunner for ArgRecorder {
        fn run<'a>(
            &'a self,
            _working_dir: &'a Path,
            _command: &'a str,
            args: &'a [&'a str],
            _env: Option<&'a [(String, String)]>,
            _timeout: Option<Duration>,
            log_path: &'a Path,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<AgentStreamOutcome>> + Send + 'a>>
        {
            let recorded = self.recorded.clone();
            let captured: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            Box::pin(async move {
                if let Some(parent) = log_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::File::create(log_path).await?;
                *recorded.lock().unwrap() = captured;
                Ok(AgentStreamOutcome {
                    exit_code: 0, success: true, stderr: String::new(),
                    bytes_written: 0, lines: vec![],
                })
            })
        }
    }

    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let runner = Arc::new(ArgRecorder { recorded: recorded.clone() });
    let (tx, _rx) = broadcast::channel(4);
    let gateway = ClaudeAgentGateway::new_with_streaming(
        FakeShellGateway::success(),
        runner,
        tmp_dir(),
        tx,
    );

    let request = AgentRequest {
        prompt: "x".to_string(),
        working_dir: PathBuf::from("/tmp"),
        access: AgentAccess::ReadOnly,
        capability: AgentCapability::Reasoning,
        agent_file: None,
        timeout: Duration::from_secs(5),
    };
    let _ = gateway.invoke(&request).await.unwrap();

    let captured = recorded.lock().unwrap().clone();
    assert!(captured.iter().any(|a| a == "--output-format"), "args: {:?}", captured);
    assert!(captured.iter().any(|a| a == "stream-json"), "args: {:?}", captured);
    assert!(captured.iter().any(|a| a == "--verbose"), "args: {:?}", captured);
    assert!(captured.iter().any(|a| a == "--model"), "args: {:?}", captured);
    assert!(captured.iter().any(|a| a == "claude-opus-4-6"), "args: {:?}", captured);
    assert!(captured.iter().any(|a| a == "--allowedTools"), "args: {:?}", captured);
}
```

- [ ] **Step 6: Run all foundryd tests**

```bash
cargo test -p foundryd
```

Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add crates/foundryd/src/gateway.rs
git commit -m "foundryd: stream-json + lifecycle events in ClaudeAgentGateway"
```

---

## Task 6: Wire `event_tx` and stream runner from main into the engine/gateway

**Files:**
- Modify: `crates/foundryd/src/main.rs`
- Modify: `crates/foundryd/src/engine.rs` (only if engine constructs the gateway)

**Why:** the new `new_with_streaming` constructor needs to be invoked from production code — otherwise the no-op default path is taken and events are emitted to a sender with no subscribers.

- [ ] **Step 1: Locate the existing `ClaudeAgentGateway::new(...)` call site(s) in foundryd's runtime wiring**

```bash
grep -n "ClaudeAgentGateway::new" crates/foundryd/src/*.rs
```

Identify the production construction (likely in `main.rs` or `engine.rs` near where `event_tx` is created).

- [ ] **Step 2: Replace with `new_with_streaming`, passing the existing `event_tx` clone**

In the production wiring (use the actual file/line you found in Step 1):

```rust
use std::sync::Arc;
use foundry_core::paths;
use crate::agent_stream::ProcessAgentStreamRunner;

let agent_gateway = Arc::new(ClaudeAgentGateway::new_with_streaming(
    Arc::new(ProcessShellGateway),
    Arc::new(ProcessAgentStreamRunner),
    paths::agent_sessions_dir(),
    event_tx.clone(),
));
```

If the existing wiring puts the gateway behind a different concrete type (e.g., `Arc<dyn AgentGateway>`), preserve that.

- [ ] **Step 3: Build to confirm wiring compiles**

```bash
cargo build -p foundryd
```

Expected: clean.

- [ ] **Step 4: Run all foundryd tests**

```bash
cargo test -p foundryd
```

Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add crates/foundryd/src/main.rs crates/foundryd/src/engine.rs
git commit -m "foundryd: wire event_tx + ProcessAgentStreamRunner into ClaudeAgentGateway"
```

---

## Task 7: Verify end-to-end with one existing block

**Files:**
- No code changes. Verification only.

- [ ] **Step 1: Run all foundry tests**

```bash
cd ~/Work/Projects/Mojility/foundry
cargo test
```

Expected: all green.

- [ ] **Step 2: Run all quality gates per repo conventions**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo deny check
```

Fix any new lints surfaced by the changes. Commit fixes (if any) with message `chore: clippy/fmt fixes for agent-session-visibility`.

- [ ] **Step 3: Smoke run foundryd locally and emit an iterate workflow**

```bash
cargo run -p foundryd -- --port 50051 &
sleep 1
cargo run -p foundry-cli -- emit iterate_requested --project demo
```

(Substitute a real project key from your registry. Or skip if no real `claude` is configured — the test suite already covers the wiring.)

Expected: `~/.foundry/agent-sessions/<uuid>.jsonl` exists and grows during the run; `foundry watch` shows `agent_session_started` and `agent_session_ended` events flow.

If `claude` is not configured, this step can be skipped — note that in the PR description and rely on the test suite.

- [ ] **Step 4: Update the event-types reference doc**

Edit `book/src/reference/event-types.md` to add entries for `agent_session_started` and `agent_session_ended` with their payload schemas.

- [ ] **Step 5: Commit doc updates**

```bash
git add book/src/reference/event-types.md
git commit -m "docs: document agent_session_started/ended event types"
```

---

## Task 8: Open PR

- [ ] **Step 1: Push branch and open PR against `main`**

```bash
git push -u origin <branch-name>
gh pr create --title "Agent session visibility v1: lifecycle events + stream-json transcript" \
  --body "$(cat <<'EOF'
## Summary
- Adds AgentSessionStarted / AgentSessionEnded event types and payloads in foundry-core.
- ClaudeAgentGateway now invokes claude with --output-format stream-json --verbose, tees stdout to ~/.foundry/agent-sessions/<session_id>.jsonl, and emits lifecycle events on the existing broadcast channel.
- AgentResponse.stdout contract preserved (final assistant text extracted from the stream-json result envelope).

## Test plan
- [x] cargo test -p foundry-core
- [x] cargo test -p foundryd
- [x] cargo clippy --all-targets --all-features -- -D warnings
- [x] cargo fmt --check
- [ ] Manual smoke run with a real claude invocation (optional)

Spec: ~/Work/Operations/Planning/2026-05-09-foundry-agent-session-visibility-v1.md
EOF
)"
```

- [ ] **Step 2: Wait for CI green; merge**

Standard repo flow.

---

## Self-Review Checklist (run after writing all task code)

- [ ] Every spec section in §4 of the design doc has a corresponding task above.
- [ ] No placeholders remain in this plan (search for `TBD`, `TODO`, `…`).
- [ ] Type names match across tasks: `AgentSessionStartedPayload`, `AgentSessionEndedPayload`, `AgentStreamRunner`, `ProcessAgentStreamRunner`, `ClaudeAgentGateway::new_with_streaming`.
- [ ] All new code paths have at least one test.
- [ ] No block-level changes outside the gateway/runner/event additions (the spec says existing blocks should be unaffected at their interface).
