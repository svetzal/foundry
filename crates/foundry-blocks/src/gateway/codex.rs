//! `CodexAgentGateway` — drives the `codex` CLI (OpenAI-backed agentic runner)
//! behind the provider-neutral [`AgentGateway`] trait.
//!
//! A third agent backend alongside `claude` and `opencode`, selectable per
//! request (see [`crate::gateway::routing::RoutingAgentGateway`]) or as the
//! daemon default via `FOUNDRY_AGENT_PROVIDER=codex`.
//!
//! The invocation contract (validated against `codex-cli` 0.134.0, `OpenAI` auth):
//!
//! - `codex exec --json --skip-git-repo-check -m <model>
//!   -c model_reasoning_effort=<effort> -o <last_message_file>
//!   {-s read-only | --dangerously-bypass-approvals-and-sandbox} <prompt>`,
//!   spawned with **stdin closed** (the shared [`AgentStreamRunner`] does this;
//!   `codex exec` otherwise blocks reading additional input from a piped stdin).
//! - The prompt is passed positionally as the last argument.
//! - stdout is JSONL. The authoritative final answer is the agent's last
//!   message, which `codex` writes verbatim to the `-o <file>` path; we read
//!   that file after the run. A stream fallback scans for the last
//!   `{"type":"item.completed","item":{"type":"agent_message","text":…}}` event
//!   when the output file is missing or empty.
//! - `AgentAccess::ReadOnly` maps to `-s read-only`, which `codex` *enforces*
//!   (the model's shell commands are sandboxed read-only) — a real guarantee
//!   that `opencode`'s advisory `ReadOnly` lacks. `AgentAccess::Full` maps to
//!   `--dangerously-bypass-approvals-and-sandbox`, matching the unsandboxed,
//!   no-prompt posture the `claude` and `opencode` gateways use for mutating
//!   work; the iterate/maintain safety net (commit only on passing gates) is
//!   the actual guard.
//! - `codex` has no `--agent` flag. When an agent-definition file is supplied
//!   its body (frontmatter stripped) is prepended to the prompt as a persona
//!   preamble — the provider-neutral equivalent of a system prompt.
//!
//! Failure detection is primarily the process exit code (validated: `0` on
//! success). As a defensive measure an explicit `{"type":"error"}` or
//! `{"type":"turn.failed"}` stream event also marks the run failed; these event
//! shapes are best-effort, not part of the validated contract.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use foundry_core::event::{Event, EventType};
use foundry_core::payload::{AgentSessionEndedPayload, AgentSessionStartedPayload};
use foundry_core::throttle::Throttle;
use serde_json::Value;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::agent_stream::{AgentStreamRunner, ProcessAgentStreamRunner, StreamedLine};

use super::{
    AgentAccess, AgentCapability, AgentGateway, AgentRequest, AgentResponse, ShellGateway,
};

/// Production [`AgentGateway`] that invokes the `codex` CLI and emits
/// `AgentSessionStarted` / `AgentSessionEnded` lifecycle events.
pub struct CodexAgentGateway {
    /// Retained for constructor symmetry with the other gateways; `codex`
    /// reads its authoritative result from the `-o` output file, so no
    /// post-run shell call is needed.
    #[allow(dead_code)]
    shell: Arc<dyn ShellGateway>,
    stream_runner: Arc<dyn AgentStreamRunner>,
    session_log_dir: PathBuf,
    event_tx: broadcast::Sender<Event>,
}

impl CodexAgentGateway {
    /// Backwards-compatible constructor: default session log dir, default stream
    /// runner, and a broadcast channel with no external receivers.
    pub fn new(shell: Arc<dyn ShellGateway>) -> Self {
        let (event_tx, _) = broadcast::channel(16);
        Self::new_with_streaming(
            shell,
            Arc::new(ProcessAgentStreamRunner),
            foundry_core::paths::agent_sessions_dir(),
            event_tx,
        )
    }

    pub fn new_with_streaming(
        shell: Arc<dyn ShellGateway>,
        stream_runner: Arc<dyn AgentStreamRunner>,
        session_log_dir: PathBuf,
        event_tx: broadcast::Sender<Event>,
    ) -> Self {
        Self {
            shell,
            stream_runner,
            session_log_dir,
            event_tx,
        }
    }

    /// Capability → model id (bare `OpenAI` names; `codex` is `OpenAI`-native).
    fn model_for(capability: AgentCapability) -> &'static str {
        match capability {
            AgentCapability::Reasoning => "gpt-5.5",
            AgentCapability::Coding => "gpt-5.4",
            AgentCapability::Quick => "gpt-5.4-mini",
        }
    }

    /// Capability → `model_reasoning_effort`.
    fn effort_for(capability: AgentCapability) -> &'static str {
        match capability {
            AgentCapability::Reasoning => "high",
            AgentCapability::Coding => "medium",
            AgentCapability::Quick => "low",
        }
    }

    fn capability_label(capability: AgentCapability) -> &'static str {
        match capability {
            AgentCapability::Reasoning => "reasoning",
            AgentCapability::Coding => "coding",
            AgentCapability::Quick => "quick",
        }
    }

    fn access_label(access: AgentAccess) -> &'static str {
        match access {
            AgentAccess::ReadOnly => "read_only",
            AgentAccess::Full => "full",
        }
    }
}

impl AgentGateway for CodexAgentGateway {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            let session_id = Uuid::new_v4().to_string();
            let log_path = self.session_log_dir.join(format!("{session_id}.jsonl"));
            let last_message_path = self.session_log_dir.join(format!("{session_id}.last.txt"));

            let model = Self::model_for(request.capability);
            let effort = Self::effort_for(request.capability);

            // Prepend the agent persona (if any) to the prompt — codex has no
            // `--agent` flag.
            let prompt =
                build_prompt(request.agent_file.as_deref(), &request.prompt, &request.project);

            let args = build_codex_argv(model, effort, request.access, &last_message_path, &prompt);
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let started_payload = AgentSessionStartedPayload {
                session_id: session_id.clone(),
                agent_type: "codex".to_string(),
                project: request.project.clone(),
                working_dir: request.working_dir.clone(),
                source_log_path: log_path.clone(),
                capability: Self::capability_label(request.capability).to_string(),
                access: Self::access_label(request.access).to_string(),
                started_at: Utc::now().to_rfc3339(),
                trace_id: String::new(),
            };
            let started_event = Event::new(
                EventType::AgentSessionStarted,
                request.project.clone(),
                Throttle::Full,
                serde_json::to_value(&started_payload)?,
            );
            let _ = self.event_tx.send(started_event);

            let outcome = self
                .stream_runner
                .run(
                    &request.working_dir,
                    "codex",
                    &arg_refs,
                    None,
                    Some(request.timeout),
                    &log_path,
                )
                .await;

            let (status, exit_code, stderr, bytes_written, error_msg, stdout_text) = match outcome {
                Ok(o) => {
                    let failed_event = has_failure_event(&o.lines);
                    let success = o.success && !failed_event;

                    // Authoritative result: the `-o` last-message file. Fall back
                    // to the last `agent_message` event in the stream.
                    let stdout_text = read_last_message(&last_message_path)
                        .await
                        .unwrap_or_else(|| extract_agent_message(&o.lines));

                    let status = if success { "ok" } else { "agent_failed" };
                    let exit_code = if o.success && failed_event {
                        1
                    } else {
                        o.exit_code
                    };
                    (status, Some(exit_code), o.stderr, o.bytes_written, None, stdout_text)
                }
                Err(e) => {
                    ("unavailable", None, String::new(), 0u64, Some(e.to_string()), String::new())
                }
            };

            // Best-effort cleanup of the transient last-message file.
            let _ = tokio::fs::remove_file(&last_message_path).await;

            let ended_payload = AgentSessionEndedPayload {
                session_id: session_id.clone(),
                status: status.to_string(),
                exit_code,
                ended_at: Utc::now().to_rfc3339(),
                bytes_written,
                error: error_msg,
            };
            let ended_event = Event::new(
                EventType::AgentSessionEnded,
                request.project.clone(),
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

// --- Pure helpers (unit-tested without spawning) ----------------------------

/// Build the `codex exec` argv. Prompt is passed positionally last.
fn build_codex_argv(
    model: &str,
    effort: &str,
    access: AgentAccess,
    last_message_path: &std::path::Path,
    prompt: &str,
) -> Vec<String> {
    let mut args = vec![
        "exec".to_string(),
        "--json".to_string(),
        "--skip-git-repo-check".to_string(),
        "-m".to_string(),
        model.to_string(),
        "-c".to_string(),
        format!("model_reasoning_effort={effort}"),
        "-o".to_string(),
        last_message_path.display().to_string(),
    ];
    match access {
        AgentAccess::ReadOnly => {
            args.push("-s".to_string());
            args.push("read-only".to_string());
        }
        AgentAccess::Full => {
            args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        }
    }
    args.push(prompt.to_string());
    args
}

/// Combine an optional agent-definition file with the request prompt. When a
/// readable agent file is present, its body (frontmatter stripped) is prepended
/// as a persona preamble. Otherwise the prompt is returned unchanged.
fn build_prompt(agent_file: Option<&std::path::Path>, prompt: &str, project: &str) -> String {
    let Some(agent_file) = agent_file else {
        return prompt.to_string();
    };
    match std::fs::read_to_string(agent_file) {
        Ok(body) => {
            let persona = strip_frontmatter(&body);
            if persona.is_empty() {
                prompt.to_string()
            } else {
                format!("{persona}\n\n---\n\n{prompt}")
            }
        }
        Err(err) => {
            tracing::warn!(
                project = %project,
                agent_file = %agent_file.display(),
                error = %err,
                "codex: could not read agent file; proceeding without persona preamble"
            );
            prompt.to_string()
        }
    }
}

/// Strip a leading YAML frontmatter block (`---` … `---`) from an agent
/// definition, returning the trimmed body. No frontmatter → trimmed input.
fn strip_frontmatter(s: &str) -> String {
    let s = s.trim_start_matches('\u{feff}');
    if !s.trim_start().starts_with("---") {
        return s.trim().to_string();
    }
    let lines: Vec<&str> = s.lines().collect();
    let mut seen_open = false;
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "---" {
            if seen_open {
                return lines[i + 1..].join("\n").trim().to_string();
            }
            seen_open = true;
        }
    }
    s.trim().to_string()
}

/// Read the `-o` last-message file. Returns `None` if it is missing, unreadable,
/// or empty after trimming.
async fn read_last_message(path: &std::path::Path) -> Option<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        Err(_) => None,
    }
}

/// Fallback result extraction: the text of the last `agent_message`
/// `item.completed` event in the JSONL stream.
fn extract_agent_message(lines: &[StreamedLine]) -> String {
    for line in lines.iter().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(&line.raw) {
            if v.get("type").and_then(Value::as_str) == Some("item.completed")
                && v.pointer("/item/type").and_then(Value::as_str) == Some("agent_message")
            {
                if let Some(t) = v.pointer("/item/text").and_then(Value::as_str) {
                    return t.trim().to_string();
                }
            }
        }
    }
    String::new()
}

/// Defensive failure detection: `true` if the stream carries an explicit
/// `{"type":"error"}` or `{"type":"turn.failed"}` event. The process exit code
/// is the primary signal; this catches a clean exit alongside a reported error.
fn has_failure_event(lines: &[StreamedLine]) -> bool {
    lines.iter().any(|l| {
        serde_json::from_str::<Value>(&l.raw)
            .ok()
            .and_then(|v| {
                v.get("type")
                    .and_then(Value::as_str)
                    .map(|t| t == "error" || t == "turn.failed")
            })
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn line(s: &str) -> StreamedLine {
        StreamedLine { raw: s.to_string() }
    }

    #[test]
    fn argv_includes_exec_json_model_effort_output_and_prompt() {
        let out = Path::new("/tmp/sess.last.txt");
        let args = build_codex_argv("gpt-5.4", "medium", AgentAccess::Full, out, "do the thing");
        assert_eq!(args[0], "exec");
        assert!(args.iter().any(|a| a == "--json"));
        assert!(args.iter().any(|a| a == "--skip-git-repo-check"));
        let mp = args.iter().position(|a| a == "-m").unwrap();
        assert_eq!(args[mp + 1], "gpt-5.4");
        let cp = args.iter().position(|a| a == "-c").unwrap();
        assert_eq!(args[cp + 1], "model_reasoning_effort=medium");
        let op = args.iter().position(|a| a == "-o").unwrap();
        assert_eq!(args[op + 1], "/tmp/sess.last.txt");
        // prompt is last
        assert_eq!(args.last().unwrap(), "do the thing");
    }

    #[test]
    fn argv_full_access_bypasses_sandbox() {
        let out = Path::new("/tmp/s.txt");
        let args = build_codex_argv("gpt-5.4", "medium", AgentAccess::Full, out, "p");
        assert!(args.iter().any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
        assert!(!args.iter().any(|a| a == "read-only"));
    }

    #[test]
    fn argv_readonly_access_uses_read_only_sandbox() {
        let out = Path::new("/tmp/s.txt");
        let args = build_codex_argv("gpt-5.5", "high", AgentAccess::ReadOnly, out, "p");
        let sp = args.iter().position(|a| a == "-s").unwrap();
        assert_eq!(args[sp + 1], "read-only");
        assert!(!args.iter().any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn capability_maps_to_expected_model_and_effort() {
        assert_eq!(CodexAgentGateway::model_for(AgentCapability::Reasoning), "gpt-5.5");
        assert_eq!(CodexAgentGateway::effort_for(AgentCapability::Reasoning), "high");
        assert_eq!(CodexAgentGateway::model_for(AgentCapability::Coding), "gpt-5.4");
        assert_eq!(CodexAgentGateway::effort_for(AgentCapability::Coding), "medium");
        assert_eq!(CodexAgentGateway::model_for(AgentCapability::Quick), "gpt-5.4-mini");
        assert_eq!(CodexAgentGateway::effort_for(AgentCapability::Quick), "low");
    }

    #[test]
    fn build_prompt_without_agent_file_returns_prompt() {
        assert_eq!(build_prompt(None, "just do it", "demo"), "just do it");
    }

    #[test]
    fn build_prompt_prepends_persona_when_agent_file_present() {
        let dir = std::env::temp_dir().join(format!("codex-bp-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("rust.md");
        std::fs::write(&f, "---\nname: rust\n---\nYou are a Rust expert.\n").unwrap();
        let out = build_prompt(Some(&f), "Fix the bug.", "demo");
        assert_eq!(out, "You are a Rust expert.\n\n---\n\nFix the bug.");
    }

    #[test]
    fn strip_frontmatter_removes_yaml_block() {
        let md = "---\nname: foo\n---\nYou are helpful.\n";
        assert_eq!(strip_frontmatter(md), "You are helpful.");
    }

    #[test]
    fn extract_agent_message_returns_last_agent_message_text() {
        let lines = vec![
            line(r#"{"type":"thread.started"}"#),
            line(r#"{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}"#),
            line(
                r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"PONG"}}"#,
            ),
            line(r#"{"type":"turn.completed","usage":{}}"#),
        ];
        assert_eq!(extract_agent_message(&lines), "PONG");
    }

    #[test]
    fn extract_agent_message_empty_when_absent() {
        let lines = vec![line(r#"{"type":"thread.started"}"#)];
        assert_eq!(extract_agent_message(&lines), "");
    }

    #[test]
    fn has_failure_event_detects_error_and_turn_failed() {
        assert!(has_failure_event(&[line(r#"{"type":"error","message":"boom"}"#)]));
        assert!(has_failure_event(&[line(r#"{"type":"turn.failed"}"#)]));
        assert!(!has_failure_event(&[line(r#"{"type":"turn.completed"}"#)]));
    }

    // --- Full invoke() flow (offline: fake stream runner) -------------------

    use crate::agent_stream::AgentStreamOutcome;
    use std::time::Duration;

    /// Fake stream runner: tees a canned JSONL transcript to the log file, and
    /// optionally writes a canned last-message file to the `-o` path parsed
    /// from the argv.
    struct FakeRunner {
        transcript: Vec<String>,
        last_message: Option<String>,
        outcome: AgentStreamOutcome,
    }

    impl AgentStreamRunner for FakeRunner {
        fn run<'a>(
            &'a self,
            _working_dir: &'a Path,
            _command: &'a str,
            args: &'a [&'a str],
            _env: Option<&'a [(String, String)]>,
            _timeout: Option<Duration>,
            log_path: &'a Path,
        ) -> Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<AgentStreamOutcome>> + Send + 'a>,
        > {
            let transcript = self.transcript.clone();
            let last_message = self.last_message.clone();
            let mut outcome = self.outcome.clone();
            // Recover the -o path from argv to simulate codex writing it.
            let out_path = args
                .iter()
                .position(|a| *a == "-o")
                .and_then(|i| args.get(i + 1))
                .map(PathBuf::from);
            Box::pin(async move {
                if let Some(parent) = log_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(log_path, transcript.join("\n")).await?;
                if let (Some(p), Some(msg)) = (out_path, last_message) {
                    tokio::fs::write(&p, msg).await?;
                }
                outcome.lines = transcript.into_iter().map(|raw| StreamedLine { raw }).collect();
                Ok(outcome)
            })
        }
    }

    fn tmp_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("foundry-codex-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn ok_outcome() -> AgentStreamOutcome {
        AgentStreamOutcome {
            exit_code: 0,
            success: true,
            stderr: String::new(),
            bytes_written: 0,
            lines: vec![],
        }
    }

    #[tokio::test]
    async fn invoke_prefers_output_file_and_emits_lifecycle_events() {
        let transcript = vec![
            r#"{"type":"thread.started"}"#.to_string(),
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"STREAM"}}"#
                .to_string(),
            r#"{"type":"turn.completed","usage":{}}"#.to_string(),
        ];
        let runner = Arc::new(FakeRunner {
            transcript,
            last_message: Some("OUTPUT FILE ANSWER".to_string()),
            outcome: ok_outcome(),
        });
        let shell = crate::gateway::fakes::FakeShellGateway::success();
        let (tx, mut rx) = broadcast::channel(16);
        let gateway = CodexAgentGateway::new_with_streaming(shell, runner, tmp_dir(), tx);

        let request = AgentRequest {
            prompt: "say something".to_string(),
            project: "demo".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::Full,
            capability: AgentCapability::Coding,
            agent_file: None,
            provider: None,
            timeout: Duration::from_secs(5),
        };

        let response = gateway.invoke(&request).await.expect("invoke ok");
        assert!(response.success);
        assert_eq!(response.exit_code, 0);
        // Output file wins over stream text.
        assert_eq!(response.stdout, "OUTPUT FILE ANSWER");

        let started = rx.recv().await.expect("started");
        assert_eq!(started.event_type, EventType::AgentSessionStarted);
        assert_eq!(started.payload["agent_type"], "codex");
        assert_eq!(started.payload["capability"], "coding");

        let ended = rx.recv().await.expect("ended");
        assert_eq!(ended.event_type, EventType::AgentSessionEnded);
        assert_eq!(ended.payload["status"], "ok");
    }

    #[tokio::test]
    async fn invoke_falls_back_to_stream_when_no_output_file() {
        let transcript = vec![
            r#"{"type":"thread.started"}"#.to_string(),
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"STREAM ANSWER"}}"#
                .to_string(),
        ];
        let runner = Arc::new(FakeRunner {
            transcript,
            last_message: None, // codex did not write the -o file
            outcome: ok_outcome(),
        });
        let shell = crate::gateway::fakes::FakeShellGateway::success();
        let (tx, _rx) = broadcast::channel(16);
        let gateway = CodexAgentGateway::new_with_streaming(shell, runner, tmp_dir(), tx);

        let request = AgentRequest {
            prompt: "x".to_string(),
            project: "demo".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::ReadOnly,
            capability: AgentCapability::Reasoning,
            agent_file: None,
            provider: None,
            timeout: Duration::from_secs(5),
        };

        let response = gateway.invoke(&request).await.expect("invoke ok");
        assert!(response.success);
        assert_eq!(response.stdout, "STREAM ANSWER");
    }

    #[tokio::test]
    async fn invoke_treats_error_event_as_failure_despite_exit_zero() {
        let transcript = vec![
            r#"{"type":"thread.started"}"#.to_string(),
            r#"{"type":"error","message":"provider exploded"}"#.to_string(),
        ];
        let runner = Arc::new(FakeRunner {
            transcript,
            last_message: None,
            outcome: ok_outcome(),
        });
        let shell = crate::gateway::fakes::FakeShellGateway::success();
        let (tx, mut rx) = broadcast::channel(16);
        let gateway = CodexAgentGateway::new_with_streaming(shell, runner, tmp_dir(), tx);

        let request = AgentRequest {
            prompt: "x".to_string(),
            project: "demo".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::Full,
            capability: AgentCapability::Quick,
            agent_file: None,
            provider: None,
            timeout: Duration::from_secs(5),
        };

        let response = gateway.invoke(&request).await.expect("invoke ok");
        assert!(!response.success, "error event should mark the run failed");
        assert_eq!(response.exit_code, 1);

        let _started = rx.recv().await.unwrap();
        let ended = rx.recv().await.unwrap();
        assert_eq!(ended.payload["status"], "agent_failed");
    }
}
