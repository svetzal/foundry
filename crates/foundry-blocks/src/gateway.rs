use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{AgentSessionEndedPayload, AgentSessionStartedPayload};
use foundry_sdk::registry::Stack;
use foundry_sdk::throttle::Throttle;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::agent_stream::{AgentStreamRunner, ProcessAgentStreamRunner, StreamedLine};

// The gateway *contract* — the traits and the data types they exchange — lives
// in the SDK (`foundry_sdk::gateway`). Re-exported here so `crate::gateway::…`
// paths used throughout the daemon keep resolving. This module contains only
// the production *implementations* of those traits.
pub use foundry_sdk::agent_config::ProviderModels;
pub use foundry_sdk::gateway::{
    AgentAccess, AgentGateway, AgentOutcome, AgentProvider, AgentRequest, AgentResponse,
    AuditResult, CommandResult, ModelTier, ReasoningEffort, ScannerGateway, ShellGateway,
};

// In-memory fakes for testing also live in the SDK, behind its `test-support`
// feature (enabled as a dev-dependency). Re-exported so block and daemon tests
// can keep using `crate::gateway::fakes::…`.
#[cfg(test)]
pub use foundry_sdk::gateway::fakes;

// The opencode-backed agent gateway (OpenAI via the `opencode` CLI). Lives in its
// own module — a natural seam: a self-contained agent provider that could one day
// move to an optional provider crate. `ClaudeAgentGateway` stays here unchanged.
pub mod opencode;
pub use opencode::OpencodeAgentGateway;

// The codex-backed agent gateway (OpenAI via the `codex` CLI). Same seam as
// opencode: a self-contained agent provider behind the `AgentGateway` trait.
pub mod codex;
pub use codex::CodexAgentGateway;

// Routes an AgentRequest to one of the registered backends based on its
// per-request provider override (or a default). This is the single gateway the
// daemon clones into every block.
pub mod routing;
pub use routing::RoutingAgentGateway;

/// Production implementation that delegates to `crate::shell::run`.
pub struct ProcessShellGateway;

impl ShellGateway for ProcessShellGateway {
    fn run<'a>(
        &'a self,
        working_dir: &'a Path,
        command: &'a str,
        args: &'a [&'a str],
        env: Option<&'a [(String, String)]>,
        timeout: Option<Duration>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<CommandResult>> + Send + 'a>> {
        Box::pin(crate::shell::run(working_dir, command, args, env, timeout))
    }
}

/// Production implementation that delegates to `crate::scanner::run_audit`.
pub struct ProcessScannerGateway;

impl ScannerGateway for ProcessScannerGateway {
    fn run_audit<'a>(
        &'a self,
        path: &'a Path,
        stack: &'a Stack,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AuditResult>> + Send + 'a>> {
        Box::pin(crate::scanner::run_audit(path, stack))
    }
}

/// Production implementation that invokes the Claude CLI via a streaming runner,
/// tees stdout to `~/.foundry/agent-sessions/<session_id>.jsonl`, and emits
/// `AgentSessionStarted` / `AgentSessionEnded` lifecycle events on the supplied
/// broadcast channel.
pub struct ClaudeAgentGateway {
    /// Retained for API compatibility; the streaming path no longer dispatches
    /// through `ShellGateway` because we need line-by-line stdout capture.
    #[allow(dead_code)]
    shell: Arc<dyn ShellGateway>,
    stream_runner: Arc<dyn AgentStreamRunner>,
    session_log_dir: PathBuf,
    event_tx: broadcast::Sender<Event>,
    /// Resolved tier→model and effort→token maps for the claude provider.
    models: ProviderModels,
}

impl ClaudeAgentGateway {
    /// Backwards-compatible constructor. Uses default session log dir and a
    /// broadcast channel with no external receivers (events are emitted into
    /// the void). Production code paths should call `new_with_streaming`.
    pub fn new(shell: Arc<dyn ShellGateway>) -> Self {
        let (event_tx, _) = broadcast::channel(16);
        Self::new_with_streaming(
            shell,
            Arc::new(ProcessAgentStreamRunner),
            foundry_sdk::paths::agent_sessions_dir(),
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
            models: ProviderModels::default_for(AgentProvider::Claude),
        }
    }

    /// Override the resolved tier/effort maps (from the agent config).
    #[must_use]
    pub fn with_models(mut self, models: ProviderModels) -> Self {
        self.models = models;
        self
    }

    fn access_label(access: AgentAccess) -> &'static str {
        match access {
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
            let session_id = Uuid::new_v4().to_string();
            let log_path = self.session_log_dir.join(format!("{session_id}.jsonl"));

            let model = self.models.model(request.tier, AgentProvider::Claude);
            let effort = self.models.effort_token(request.effort, AgentProvider::Claude);
            let mut args: Vec<String> = vec![
                "--print".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
                "--model".to_string(),
                model,
                "--effort".to_string(),
                effort,
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

            let started_payload = AgentSessionStartedPayload {
                session_id: session_id.clone(),
                agent_type: "claude-code".to_string(),
                project: request.project.clone(),
                working_dir: request.working_dir.clone(),
                source_log_path: log_path.clone(),
                tier: request.tier.as_str().to_string(),
                effort: request.effort.as_str().to_string(),
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

            // CLAUDECODE="" prevents nested-session detection.
            let env = vec![("CLAUDECODE".to_string(), String::new())];
            let outcome = self
                .stream_runner
                .run(
                    &request.working_dir,
                    "claude",
                    &arg_refs,
                    Some(&env),
                    Some(request.timeout),
                    &log_path,
                )
                .await;

            let (status, exit_code, stderr, bytes_written, error_msg, stdout_text) = match outcome {
                Ok(o) => {
                    let extracted = extract_final_text(&o.lines);
                    let status = if o.success { "ok" } else { "agent_failed" };
                    (status, Some(o.exit_code), o.stderr, o.bytes_written, None, extracted)
                }
                Err(e) => {
                    ("unavailable", None, String::new(), 0u64, Some(e.to_string()), String::new())
                }
            };

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

/// Extract the final assistant text from a stream-json transcript.
///
/// Prefers a `{"type":"result", ..., "result":"…"}` envelope; falls back to
/// concatenation of `{"type":"assistant", "message":{"content":[{"type":"text","text":"…"}…]}}` entries.
fn extract_final_text(lines: &[StreamedLine]) -> String {
    use serde_json::Value;
    for line in lines.iter().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(&line.raw) {
            if v.get("type").and_then(Value::as_str) == Some("result") {
                if let Some(s) = v.get("result").and_then(Value::as_str) {
                    return s.to_string();
                }
            }
        }
    }
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

#[cfg(test)]
mod claude_agent_gateway_streaming_tests {
    use super::fakes::FakeShellGateway;
    use super::*;
    use crate::agent_stream::{AgentStreamOutcome, AgentStreamRunner, StreamedLine};
    use foundry_sdk::event::EventType;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::broadcast;

    /// Test fake: returns canned outcome and writes a canned transcript to `log_path`.
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
        ) -> Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<AgentStreamOutcome>> + Send + 'a>,
        > {
            let transcript = self.transcript.clone();
            let mut template = self.outcome_template.clone();
            Box::pin(async move {
                if let Some(parent) = log_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                let mut file = tokio::fs::File::create(log_path).await?;
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
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#
                .to_string(),
            r#"{"type":"result","subtype":"success","result":"Final answer."}"#.to_string(),
        ];

        let runner = Arc::new(FakeAgentStreamRunner {
            transcript: transcript.clone(),
            outcome_template: AgentStreamOutcome {
                exit_code: 0,
                success: true,
                stderr: String::new(),
                bytes_written: 0,
                lines: vec![],
            },
        });

        let shell = FakeShellGateway::success();
        let gateway =
            ClaudeAgentGateway::new_with_streaming(shell, runner, session_log_dir.clone(), tx);

        let request = AgentRequest {
            prompt: "say hi".to_string(),
            project: "demo-project".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::Full,
            tier: ModelTier::Balanced,
            effort: ReasoningEffort::Medium,
            agent_file: None,
            provider: None,
            timeout: Duration::from_secs(60),
        };

        let response = gateway.invoke(&request).await.expect("invoke ok");
        assert!(response.success);
        assert_eq!(response.exit_code, 0);
        assert_eq!(response.stdout, "Final answer.");

        let started = rx.recv().await.expect("started event");
        assert_eq!(started.event_type, EventType::AgentSessionStarted);
        assert_eq!(started.project, "demo-project");
        assert_eq!(started.payload["agent_type"], "claude-code");
        assert_eq!(started.payload["tier"], "balanced");
        assert_eq!(started.payload["effort"], "medium");
        assert_eq!(started.payload["access"], "full");
        assert_eq!(started.payload["project"], "demo-project");
        let session_id = started.payload["session_id"].as_str().unwrap().to_string();
        assert!(!session_id.is_empty());
        let log_path = started.payload["source_log_path"].as_str().unwrap();
        assert!(
            std::path::Path::new(log_path)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        );

        let ended = rx.recv().await.expect("ended event");
        assert_eq!(ended.event_type, EventType::AgentSessionEnded);
        assert_eq!(ended.project, "demo-project");
        assert_eq!(ended.payload["session_id"], session_id);
        assert_eq!(ended.payload["status"], "ok");
        assert_eq!(ended.payload["exit_code"], 0);
        assert!(ended.payload["bytes_written"].as_u64().unwrap() > 0);

        let written = tokio::fs::read_to_string(log_path).await.unwrap();
        let mut expected = String::new();
        for line in &transcript {
            expected.push_str(line);
            expected.push('\n');
        }
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
        let gateway = ClaudeAgentGateway::new_with_streaming(shell, runner, session_log_dir, tx);

        let request = AgentRequest {
            prompt: "fail please".to_string(),
            project: String::new(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::Full,
            tier: ModelTier::Balanced,
            effort: ReasoningEffort::Medium,
            agent_file: None,
            provider: None,
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
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = anyhow::Result<AgentStreamOutcome>>
                        + Send
                        + 'a,
                >,
            > {
                let recorded = self.recorded.clone();
                let captured: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
                Box::pin(async move {
                    if let Some(parent) = log_path.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::File::create(log_path).await?;
                    *recorded.lock().unwrap() = captured;
                    Ok(AgentStreamOutcome {
                        exit_code: 0,
                        success: true,
                        stderr: String::new(),
                        bytes_written: 0,
                        lines: vec![],
                    })
                })
            }
        }

        let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let runner = Arc::new(ArgRecorder {
            recorded: recorded.clone(),
        });
        let (tx, _rx) = broadcast::channel(4);
        let gateway = ClaudeAgentGateway::new_with_streaming(
            FakeShellGateway::success(),
            runner,
            tmp_dir(),
            tx,
        );

        let request = AgentRequest {
            prompt: "x".to_string(),
            project: String::new(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Deep,
            effort: ReasoningEffort::High,
            agent_file: None,
            provider: None,
            timeout: Duration::from_secs(5),
        };
        let _ = gateway.invoke(&request).await.unwrap();

        let captured = recorded.lock().unwrap().clone();
        assert!(captured.iter().any(|a| a == "--output-format"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "stream-json"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "--verbose"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "--model"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "claude-opus-4-8"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "--effort"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "high"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "--allowedTools"), "args: {captured:?}");
    }
}
