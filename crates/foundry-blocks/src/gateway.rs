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

use crate::agent_stream::{
    AgentStreamOutcome, AgentStreamRunner, ProcessAgentStreamRunner, StreamedLine,
};

// The gateway *contract* — the traits and the data types they exchange — lives
// in the SDK (`foundry_sdk::gateway`). Re-exported here so `crate::gateway::…`
// paths used throughout the daemon keep resolving. This module contains only
// the production *implementations* of those traits.
pub use foundry_sdk::agent_config::ProviderModels;
pub use foundry_sdk::gateway::{
    AgentAccess, AgentFailureKind, AgentFailureMetadata, AgentGateway, AgentOutcome, AgentProvider,
    AgentRequest, AgentResponse, AuditResult, CommandResult, ModelTier, ReasoningEffort,
    ScannerGateway, ShellGateway, classify_claude_result_record,
};

// Shared generic gateway engine — the single `invoke()` lifecycle reused by all
// CLI-backed agent provider implementations. Per-provider variation lives in
// `CliAgentAdapter` impls in this module and the `opencode`/`codex` submodules.
pub(crate) mod engine;
use engine::{CliAgentAdapter, CliAgentGateway, Interpreted, Invocation, SessionContext};

// In-memory fakes for testing also live in the SDK, behind its `test-support`
// feature (enabled as a dev-dependency). Re-exported so block and daemon tests
// can keep using `crate::gateway::fakes::…`.
#[cfg(test)]
pub use foundry_sdk::gateway::fakes;

// Shared macro for the CLI-backed gateway newtype wrapper. `#[macro_use]` exports
// the macro up to this module and down into all submodules (opencode, codex).
#[macro_use]
mod macros;

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

/// Adapter that captures the Claude-specific CLI invocation contract.
pub(crate) struct ClaudeAdapter;

impl CliAgentAdapter for ClaudeAdapter {
    fn provider(&self) -> AgentProvider {
        AgentProvider::Claude
    }

    fn agent_type(&self) -> &'static str {
        "claude-code"
    }

    fn command(&self) -> &'static str {
        "claude"
    }

    fn build_invocation(
        &self,
        request: &AgentRequest,
        model: &str,
        effort: &str,
        _session_id: &str,
        _session_log_dir: &Path,
    ) -> Invocation {
        let mut args: Vec<String> = vec![
            "--print".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--model".to_string(),
            model.to_string(),
            "--effort".to_string(),
            effort.to_string(),
        ];
        if let Some(ref agent_file) = request.agent_file {
            args.push("--agent".to_string());
            args.push(claude_agent_name(agent_file));
        }
        if request.access == AgentAccess::ReadOnly {
            args.push("--allowedTools".to_string());
            args.push("Read Glob Grep WebFetch WebSearch".to_string());
        }
        args.push("--dangerously-skip-permissions".to_string());
        args.push("-p".to_string());
        args.push(request.prompt.clone());
        // CLAUDECODE="" prevents nested-session detection.
        Invocation {
            args,
            env: vec![("CLAUDECODE".to_string(), String::new())],
            last_message_path: None,
        }
    }

    fn interpret<'a>(
        &'a self,
        outcome: &'a AgentStreamOutcome,
        session: SessionContext<'a>,
        _inv: &'a Invocation,
        _request: &'a AgentRequest,
        _shell: &'a Arc<dyn ShellGateway>,
    ) -> Pin<Box<dyn std::future::Future<Output = Interpreted> + Send + 'a>> {
        Box::pin(async move {
            debug_assert_eq!(session.provider, AgentProvider::Claude);
            let failure =
                match read_claude_terminal_failure(session.log_path, session.session_id).await {
                    Ok(failure) => failure,
                    Err(_) => outcome.lines.iter().rev().find_map(|line| {
                        classify_claude_result_record(
                            &line.raw,
                            Some(session.session_id),
                            Some(session.log_path),
                        )
                    }),
                };
            let stdout = if failure.is_some() {
                extract_result_text_from_log(session.log_path)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| extract_final_text(&outcome.lines))
            } else {
                extract_final_text(&outcome.lines)
            };
            let success = outcome.success && failure.is_none();
            let exit_code = if outcome.success && failure.is_some() {
                1
            } else {
                outcome.exit_code
            };
            Interpreted {
                success,
                exit_code,
                stdout,
                failure,
            }
        })
    }
}

cli_agent_gateway! {
    /// Production implementation that invokes the Claude CLI via a streaming runner,
    /// tees stdout to `~/.foundry/agent-sessions/<session_id>.jsonl`, and emits
    /// `AgentSessionStarted` / `AgentSessionEnded` lifecycle events on the supplied
    /// broadcast channel.
    ClaudeAgentGateway, ClaudeAdapter
}

fn claude_agent_name(agent_file: &Path) -> String {
    agent_file
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or_else(|| agent_file.display().to_string(), ToString::to_string)
}

/// Extract the final assistant text from a stream-json transcript.
///
/// Prefers a `{"type":"result", ..., "result":"…"}` envelope; falls back to
/// concatenation of `{"type":"assistant", "message":{"content":[{"type":"text","text":"…"}…]}}` entries.
fn extract_final_text(lines: &[StreamedLine]) -> String {
    use serde_json::Value;
    for line in lines.iter().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(&line.raw)
            && v.get("type").and_then(Value::as_str) == Some("result")
            && let Some(s) = v.get("result").and_then(Value::as_str)
        {
            return s.to_string();
        }
    }
    let mut out = String::new();
    for line in lines {
        if let Ok(v) = serde_json::from_str::<Value>(&line.raw)
            && v.get("type").and_then(Value::as_str) == Some("assistant")
            && let Some(content) = v.pointer("/message/content").and_then(Value::as_array)
        {
            for block in content {
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = block.get("text").and_then(Value::as_str)
                {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
    }
    out
}

/// Strip a leading YAML frontmatter block (`---` … `---`) from an agent
/// definition, returning the trimmed body. No frontmatter → trimmed input.
pub(crate) fn strip_frontmatter(s: &str) -> String {
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

/// Emit an `AgentSessionStarted` event on `event_tx`.
pub(crate) fn emit_session_started(
    event_tx: &broadcast::Sender<Event>,
    request: &AgentRequest,
    session_id: &str,
    agent_type: &str,
    log_path: &std::path::Path,
) -> Result<()> {
    let started_payload = AgentSessionStartedPayload {
        session_id: session_id.to_string(),
        agent_type: agent_type.to_string(),
        project: request.project.clone(),
        working_dir: request.working_dir.clone(),
        source_log_path: log_path.to_path_buf(),
        tier: request.tier.as_str().to_string(),
        effort: request.effort.as_str().to_string(),
        access: request.access.label().to_string(),
        started_at: Utc::now().to_rfc3339(),
        trace_id: String::new(),
    };
    let started_event = Event::new(
        EventType::AgentSessionStarted,
        request.project.clone(),
        Throttle::Full,
        serde_json::to_value(&started_payload)?,
    );
    // Best-effort: a send error means no Watch subscribers are attached,
    // which is the normal steady state; session emission must not depend on
    // a listener.
    if let Err(e) = event_tx.send(started_event) {
        tracing::debug!(error = %e, session_id, "no Watch subscribers for AgentSessionStarted");
    }
    Ok(())
}

/// Emit an `AgentSessionEnded` event on `event_tx`.
pub(crate) fn emit_session_ended(
    event_tx: &broadcast::Sender<Event>,
    project: &str,
    payload: &AgentSessionEndedPayload,
) -> Result<()> {
    let ended_event = Event::new(
        EventType::AgentSessionEnded,
        project.to_string(),
        Throttle::Full,
        serde_json::to_value(payload)?,
    );
    // Best-effort: a send error means no Watch subscribers are attached,
    // which is the normal steady state; session emission must not depend on
    // a listener.
    if let Err(e) = event_tx.send(ended_event) {
        tracing::debug!(error = %e, project, "no Watch subscribers for AgentSessionEnded");
    }
    Ok(())
}

async fn extract_result_text_from_log(log_path: &Path) -> std::io::Result<Option<String>> {
    let log = tokio::fs::read_to_string(log_path).await?;
    for line in log.lines().rev() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
            && v.get("type").and_then(serde_json::Value::as_str) == Some("result")
            && let Some(s) = v.get("result").and_then(serde_json::Value::as_str)
        {
            return Ok(Some(s.to_string()));
        }
    }
    Ok(None)
}

async fn read_claude_terminal_failure(
    log_path: &Path,
    session_id: &str,
) -> std::io::Result<Option<AgentFailureMetadata>> {
    let log = tokio::fs::read_to_string(log_path).await?;
    Ok(log
        .lines()
        .rev()
        .find_map(|line| classify_claude_result_record(line, Some(session_id), Some(log_path))))
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

    #[tokio::test]
    async fn invoke_emits_started_then_ended_and_writes_transcript() {
        let session_log_dir = super::test_support::tmp_dir("foundry-test");
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
            env: Vec::new(),
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
        let session_log_dir = super::test_support::tmp_dir("foundry-test");
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
            env: Vec::new(),
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
    async fn invoke_classifies_terminal_provider_failure_from_session_log() {
        let session_log_dir = super::test_support::tmp_dir("foundry-terminal-failure");
        let (tx, mut rx) = broadcast::channel(16);

        let runner = Arc::new(FakeAgentStreamRunner {
            transcript: vec![r#"{"type":"result","is_error":true,"api_error_status":429,"result":"You've hit your monthly spend limit - raise it at claude.ai/settings/usage"}"#.to_string()],
            outcome_template: AgentStreamOutcome {
                exit_code: 0,
                success: true,
                stderr: String::new(),
                bytes_written: 0,
                lines: vec![],
            },
        });

        let gateway = ClaudeAgentGateway::new_with_streaming(
            FakeShellGateway::success(),
            runner,
            session_log_dir,
            tx,
        );

        let request = AgentRequest {
            prompt: "fail please".to_string(),
            project: "demo-project".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::Full,
            tier: ModelTier::Balanced,
            effort: ReasoningEffort::Medium,
            agent_file: None,
            provider: None,
            env: Vec::new(),
            timeout: Duration::from_secs(5),
        };

        let response = gateway.invoke(&request).await.expect("invoke ok");
        assert!(!response.success);
        assert_eq!(response.exit_code, 1);
        let failure = response.failure.expect("terminal failure metadata");
        assert_eq!(failure.api_error_status, Some(429));
        assert_eq!(failure.failure_kind, Some(AgentFailureKind::AccountLimit));
        assert!(failure.terminal);
        assert_eq!(
            failure.message.as_deref(),
            Some("You've hit your monthly spend limit - raise it at claude.ai/settings/usage")
        );

        let _started = rx.recv().await.unwrap();
        let ended = rx.recv().await.unwrap();
        assert_eq!(ended.event_type, EventType::AgentSessionEnded);
        assert_eq!(ended.payload["status"], "agent_failed");
        assert_eq!(ended.payload["api_error_status"], 429);
        assert_eq!(ended.payload["failure_kind"], "account_limit");
        assert_eq!(ended.payload["terminal"], true);
        assert_eq!(
            ended.payload["message"],
            "You've hit your monthly spend limit - raise it at claude.ai/settings/usage"
        );
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
            super::test_support::tmp_dir("foundry-test"),
            tx,
        );

        let request = AgentRequest {
            prompt: "x".to_string(),
            project: String::new(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Deep,
            effort: ReasoningEffort::High,
            agent_file: Some(PathBuf::from(
                "/Users/svetzal/.claude/agents/typescript-bun-cli-craftsperson.md",
            )),
            provider: None,
            env: Vec::new(),
            timeout: Duration::from_secs(5),
        };
        let _ = gateway.invoke(&request).await.unwrap();

        let captured = recorded.lock().unwrap().clone();
        assert!(captured.iter().any(|a| a == "--output-format"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "stream-json"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "--verbose"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "--model"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "claude-opus-5"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "--effort"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "high"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "--allowedTools"), "args: {captured:?}");
        let agent_flag = captured.iter().position(|a| a == "--agent").expect("agent flag present");
        assert_eq!(
            captured.get(agent_flag + 1).map(String::as_str),
            Some("typescript-bun-cli-craftsperson")
        );
        assert!(
            !captured.iter().any(|a| std::path::Path::new(a)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))),
            "args: {captured:?}"
        );
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::time::Duration;

    use crate::agent_stream::{AgentStreamOutcome, AgentStreamRunner, StreamedLine};

    pub(crate) struct FakeRunner {
        pub(crate) transcript: Vec<String>,
        /// When `Some`, the `-o` path is recovered from argv and this message is written to it
        /// (codex behaviour). When `None`, no `-o` file is written (opencode behaviour).
        pub(crate) last_message: Option<String>,
        pub(crate) outcome: AgentStreamOutcome,
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

    pub(crate) fn ok_outcome() -> AgentStreamOutcome {
        AgentStreamOutcome {
            exit_code: 0,
            success: true,
            stderr: String::new(),
            bytes_written: 0,
            lines: vec![],
        }
    }

    pub(crate) fn tmp_dir(prefix: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("{}-{}", prefix, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

#[cfg(test)]
mod strip_frontmatter_tests {
    use super::strip_frontmatter;

    #[test]
    fn strip_frontmatter_removes_yaml_block() {
        let md = "---\nname: foo\n---\nYou are helpful.\n";
        assert_eq!(strip_frontmatter(md), "You are helpful.");
    }

    #[test]
    fn strip_frontmatter_removes_yaml_block_with_description() {
        let md = "---\nname: foo\ndescription: bar\n---\nYou are a helpful agent.\n";
        assert_eq!(strip_frontmatter(md), "You are a helpful agent.");
    }

    #[test]
    fn strip_frontmatter_passthrough_when_absent() {
        assert_eq!(strip_frontmatter("Just a body."), "Just a body.");
    }
}
