use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use foundry_core::event::{Event, EventType};
use foundry_core::payload::{AgentSessionEndedPayload, AgentSessionStartedPayload};
use foundry_core::registry::Stack;
use foundry_core::throttle::Throttle;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::agent_stream::{AgentStreamRunner, ProcessAgentStreamRunner, StreamedLine};
use crate::scanner::AuditResult;
use crate::shell::CommandResult;

// --- ShellGateway -----------------------------------------------------------

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

// --- ScannerGateway ---------------------------------------------------------

/// Abstracts over vulnerability scanning so that task blocks can be tested
/// without running real audit tools.
pub trait ScannerGateway: Send + Sync {
    fn run_audit<'a>(
        &'a self,
        path: &'a Path,
        stack: &'a Stack,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AuditResult>> + Send + 'a>>;
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

// --- AgentGateway -----------------------------------------------------------

/// Access level for an agent invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAccess {
    /// Read-only tools (Read, Glob, Grep, `WebFetch`, `WebSearch`).
    ReadOnly,
    /// Full tool access — no restrictions.
    Full,
}

/// Capability hint that the block needs from the agent runtime.
/// The gateway maps this to a concrete model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Reasoning used in Phase 3 (AssessProject, CreatePlan)
pub enum AgentCapability {
    /// Deep reasoning (maps to opus in Claude).
    Reasoning,
    /// Code generation and modification (maps to sonnet in Claude).
    Coding,
    /// Fast, lightweight tasks (maps to haiku in Claude).
    Quick,
}

/// A request to invoke an AI agent.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    /// The work to perform.
    pub prompt: String,
    /// Project working directory.
    pub working_dir: PathBuf,
    /// Tool access level.
    pub access: AgentAccess,
    /// Capability needed from the agent.
    pub capability: AgentCapability,
    /// Optional path to an agent definition file.
    pub agent_file: Option<PathBuf>,
    /// Maximum duration for the invocation.
    pub timeout: Duration,
}

/// Response from an agent invocation.
#[derive(Debug, Clone)]
pub struct AgentResponse {
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Process exit code.
    pub exit_code: i32,
    /// Whether the invocation succeeded (`exit_code` == 0).
    pub success: bool,
}

/// The outcome of an agent invocation, abstracting over `Result<AgentResponse>`.
/// Separates three distinct cases: successful run, agent ran but failed (non-zero exit),
/// and the agent could not be invoked at all.
pub enum AgentOutcome {
    /// Agent ran and succeeded (`success=true`).
    Success { stdout: String },
    /// Agent ran but exited with failure (`success=false`).
    AgentFailed { stderr: String },
    /// Agent could not be invoked (spawn failure, timeout, etc.).
    Unavailable { error: String },
}

impl AgentOutcome {
    pub fn from_response(response: anyhow::Result<AgentResponse>) -> Self {
        match response {
            Ok(r) if r.success => AgentOutcome::Success { stdout: r.stdout },
            Ok(r) => AgentOutcome::AgentFailed { stderr: r.stderr },
            Err(err) => AgentOutcome::Unavailable {
                error: err.to_string(),
            },
        }
    }
}

/// Abstracts over AI agent runtimes so that blocks can be tested without
/// invoking real agent processes.
pub trait AgentGateway: Send + Sync {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>>;
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

    fn model_for(capability: AgentCapability) -> &'static str {
        match capability {
            AgentCapability::Reasoning => "claude-opus-4-6",
            AgentCapability::Coding => "claude-sonnet-4-6",
            AgentCapability::Quick => "claude-haiku-4-5-20251001",
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

impl AgentGateway for ClaudeAgentGateway {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            let session_id = Uuid::new_v4().to_string();
            let log_path = self.session_log_dir.join(format!("{session_id}.jsonl"));

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

            let started_payload = AgentSessionStartedPayload {
                session_id: session_id.clone(),
                agent_type: "claude-code".to_string(),
                project: String::new(),
                working_dir: request.working_dir.clone(),
                source_log_path: log_path.clone(),
                capability: Self::capability_label(request.capability).to_string(),
                access: Self::access_label(request.access).to_string(),
                started_at: Utc::now().to_rfc3339(),
                trace_id: String::new(),
            };
            let started_event = Event::new(
                EventType::AgentSessionStarted,
                String::new(),
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
                bytes_written,
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

// --- Test fakes -------------------------------------------------------------

#[cfg(test)]
pub mod fakes {
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::Result;
    use foundry_core::registry::Stack;

    use crate::scanner::{AuditResult, Vulnerability};
    use crate::shell::CommandResult;

    use super::{
        AgentAccess, AgentCapability, AgentGateway, AgentRequest, AgentResponse, ScannerGateway,
        ShellGateway,
    };

    /// A recorded shell invocation for use in test assertions.
    #[derive(Debug, Clone)]
    pub struct ShellInvocation {
        pub command: String,
        pub args: Vec<String>,
        // Available for test assertions even when not checked by every test.
        #[allow(dead_code)]
        pub working_dir: String,
    }

    /// Behaviour specification for a single `FakeShellGateway` response.
    enum ShellResponse {
        Fixed(CommandResult),
        Sequence(Vec<CommandResult>),
    }

    /// Fake shell gateway for use in tests.
    ///
    /// Records every invocation and returns pre-configured results.
    pub struct FakeShellGateway {
        response: ShellResponse,
        invocations: Arc<Mutex<Vec<ShellInvocation>>>,
        /// Index for `Sequence` responses.
        index: Mutex<usize>,
    }

    impl FakeShellGateway {
        /// Always return the same result for every call.
        pub fn always(result: CommandResult) -> Arc<Self> {
            Arc::new(Self {
                response: ShellResponse::Fixed(result),
                invocations: Arc::new(Mutex::new(vec![])),
                index: Mutex::new(0),
            })
        }

        /// Return results in order; the last result repeats indefinitely.
        pub fn sequence(results: Vec<CommandResult>) -> Arc<Self> {
            assert!(!results.is_empty(), "FakeShellGateway::sequence requires at least one result");
            Arc::new(Self {
                response: ShellResponse::Sequence(results),
                invocations: Arc::new(Mutex::new(vec![])),
                index: Mutex::new(0),
            })
        }

        /// Always return a successful, empty result.
        pub fn success() -> Arc<Self> {
            Self::always(CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            })
        }

        /// Always return a failure result with the given stderr.
        pub fn failure(stderr: impl Into<String>) -> Arc<Self> {
            Self::always(CommandResult {
                stdout: String::new(),
                stderr: stderr.into(),
                exit_code: 1,
                success: false,
            })
        }

        /// Return a snapshot of all recorded invocations.
        pub fn invocations(&self) -> Vec<ShellInvocation> {
            self.invocations.lock().unwrap().clone()
        }

        fn next_result(&self) -> CommandResult {
            match &self.response {
                ShellResponse::Fixed(r) => r.clone(),
                ShellResponse::Sequence(seq) => {
                    let mut idx = self.index.lock().unwrap();
                    let r = seq[(*idx).min(seq.len() - 1)].clone();
                    *idx += 1;
                    r
                }
            }
        }
    }

    impl ShellGateway for FakeShellGateway {
        fn run<'a>(
            &'a self,
            working_dir: &'a Path,
            command: &'a str,
            args: &'a [&'a str],
            _env: Option<&'a [(String, String)]>,
            _timeout: Option<Duration>,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<CommandResult>> + Send + 'a>> {
            let inv = ShellInvocation {
                command: command.to_string(),
                args: args.iter().map(ToString::to_string).collect(),
                working_dir: working_dir.display().to_string(),
            };
            self.invocations.lock().unwrap().push(inv);
            let result = self.next_result();
            Box::pin(async move { Ok(result) })
        }
    }

    // --- FakeScannerGateway -------------------------------------------------

    /// Fake scanner gateway for use in tests.
    pub struct FakeScannerGateway {
        result: Result<AuditResult, String>,
    }

    impl FakeScannerGateway {
        /// Return an empty, clean audit result.
        pub fn clean() -> Arc<Self> {
            Arc::new(Self {
                result: Ok(AuditResult::default()),
            })
        }

        /// Return an audit result with the given vulnerabilities.
        pub fn with_vulnerabilities(vulns: Vec<Vulnerability>) -> Arc<Self> {
            Arc::new(Self {
                result: Ok(AuditResult {
                    vulnerabilities: vulns,
                    error: None,
                }),
            })
        }

        /// Return an audit result carrying a tool-level error.
        pub fn with_error(msg: impl Into<String>) -> Arc<Self> {
            Arc::new(Self {
                result: Ok(AuditResult {
                    vulnerabilities: vec![],
                    error: Some(msg.into()),
                }),
            })
        }
    }

    impl ScannerGateway for FakeScannerGateway {
        fn run_audit<'a>(
            &'a self,
            _path: &'a Path,
            _stack: &'a Stack,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<AuditResult>> + Send + 'a>> {
            let result = match &self.result {
                Ok(r) => Ok(r.clone()),
                Err(msg) => Err(anyhow::anyhow!("{msg}")),
            };
            Box::pin(async move { result })
        }
    }

    // --- FakeAgentGateway --------------------------------------------------

    /// A recorded agent invocation for use in test assertions.
    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    pub struct AgentInvocation {
        pub prompt: String,
        pub working_dir: String,
        pub access: AgentAccess,
        pub capability: AgentCapability,
        pub agent_file: Option<String>,
    }

    /// Behaviour specification for a single `FakeAgentGateway` response.
    enum AgentGatewayResponse {
        Fixed(AgentResponse),
        Sequence(Vec<AgentResponse>),
    }

    /// Fake agent gateway for use in tests.
    ///
    /// Records every invocation and returns pre-configured responses.
    pub struct FakeAgentGateway {
        response: AgentGatewayResponse,
        invocations: Arc<Mutex<Vec<AgentInvocation>>>,
        index: Mutex<usize>,
    }

    impl FakeAgentGateway {
        /// Always return the same result for every call.
        pub fn always(result: AgentResponse) -> Arc<Self> {
            Arc::new(Self {
                response: AgentGatewayResponse::Fixed(result),
                invocations: Arc::new(Mutex::new(vec![])),
                index: Mutex::new(0),
            })
        }

        /// Return results in order; the last result repeats indefinitely.
        pub fn sequence(results: Vec<AgentResponse>) -> Arc<Self> {
            assert!(!results.is_empty(), "FakeAgentGateway::sequence requires at least one result");
            Arc::new(Self {
                response: AgentGatewayResponse::Sequence(results),
                invocations: Arc::new(Mutex::new(vec![])),
                index: Mutex::new(0),
            })
        }

        /// Always return a successful, empty result.
        pub fn success() -> Arc<Self> {
            Self::always(AgentResponse {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            })
        }

        /// Always return a successful result with the given stdout.
        pub fn success_with(stdout: impl Into<String>) -> Arc<Self> {
            Self::always(AgentResponse {
                stdout: stdout.into(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            })
        }

        /// Always return a failure result with the given stderr.
        pub fn failure(stderr: impl Into<String>) -> Arc<Self> {
            Self::always(AgentResponse {
                stdout: String::new(),
                stderr: stderr.into(),
                exit_code: 1,
                success: false,
            })
        }

        /// Return a snapshot of all recorded invocations.
        pub fn invocations(&self) -> Vec<AgentInvocation> {
            self.invocations.lock().unwrap().clone()
        }

        fn next_result(&self) -> AgentResponse {
            match &self.response {
                AgentGatewayResponse::Fixed(r) => r.clone(),
                AgentGatewayResponse::Sequence(seq) => {
                    let mut idx = self.index.lock().unwrap();
                    let r = seq[(*idx).min(seq.len() - 1)].clone();
                    *idx += 1;
                    r
                }
            }
        }
    }

    impl AgentGateway for FakeAgentGateway {
        fn invoke<'a>(
            &'a self,
            request: &'a AgentRequest,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>> {
            let inv = AgentInvocation {
                prompt: request.prompt.clone(),
                working_dir: request.working_dir.display().to_string(),
                access: request.access,
                capability: request.capability,
                agent_file: request.agent_file.as_ref().map(|p| p.display().to_string()),
            };
            self.invocations.lock().unwrap().push(inv);
            let result = self.next_result();
            Box::pin(async move { Ok(result) })
        }
    }
}

#[cfg(test)]
mod claude_agent_gateway_streaming_tests {
    use super::fakes::FakeShellGateway;
    use super::*;
    use crate::agent_stream::{AgentStreamOutcome, AgentStreamRunner, StreamedLine};
    use foundry_core::event::EventType;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
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
        let gateway = ClaudeAgentGateway::new_with_streaming(shell, runner, session_log_dir, tx);

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
                Box<dyn std::future::Future<Output = anyhow::Result<AgentStreamOutcome>> + Send + 'a>,
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
        assert!(captured.iter().any(|a| a == "--output-format"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "stream-json"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "--verbose"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "--model"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "claude-opus-4-6"), "args: {captured:?}");
        assert!(captured.iter().any(|a| a == "--allowedTools"), "args: {captured:?}");
    }
}
