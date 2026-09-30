use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::paths;
use foundry_sdk::payload::{AgentSessionEndedPayload, AgentSessionStartedPayload};
use foundry_sdk::registry::Stack;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::token_rates::{self, CostEstimate, RateBook};
use foundry_sdk::token_usage::{self, SessionUsage};
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
        session_id: &str,
        _session_log_dir: &Path,
    ) -> Invocation {
        let mut args: Vec<String> = vec![
            // Hand the CLI Foundry's own session id rather than letting it mint
            // one. Without this the id Foundry logs under and the id the CLI
            // stores the conversation under differ, and `--resume <foundry id>`
            // fails with "No conversation found with session ID" — which is the
            // whole recovery path in `resume_for_json`.
            "--session-id".to_string(),
            session_id.to_string(),
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
        // `-p` with no positional prompt: the CLI reads the prompt from stdin in
        // print mode. The prompt must NOT ride in argv — Linux caps one argv
        // element at MAX_ARG_STRLEN (131072 bytes), and a campaign prompt that
        // crosses it fails the spawn outright with E2BIG.
        args.push("-p".to_string());
        // CLAUDECODE="" prevents nested-session detection.
        Invocation {
            args,
            env: vec![("CLAUDECODE".to_string(), String::new())],
            stdin: Some(request.prompt.clone().into_bytes()),
            last_message_path: None,
        }
    }

    fn interpret<'a>(
        &'a self,
        outcome: &'a AgentStreamOutcome,
        session: SessionContext<'a>,
        inv: &'a Invocation,
        request: &'a AgentRequest,
        shell: &'a Arc<dyn ShellGateway>,
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
            let stdout = if success && request.requires_json && !carries_json_object(&stdout) {
                resume_for_json(session, inv, request, shell, stdout).await
            } else {
                stdout
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

/// Prompt used to recover a verdict from a session that ended its turn
/// without one.
const RESUME_FOR_JSON_PROMPT: &str = "Your previous turn ended without the required JSON object. \
     This is a single non-interactive turn and no notification will reach you: do not start any \
     command in the background and do not wait for anything. If you still need a command's \
     result, run it to completion in the foreground now. Then output the required JSON object \
     and nothing else.";

/// Does this answer already carry the typed JSON object the caller parses?
fn carries_json_object(output: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(&crate::blocks::extract_json(output))
        .is_ok_and(|value| value.is_object())
}

/// Build the argv for a one-shot `claude --resume` recovery turn.
///
/// Carries the original invocation's model, effort, agent and tool allowlist
/// forward verbatim, so the recovery runs at the same tier and under the same
/// access as the session it is resuming — a read-only reviewer stays
/// read-only. The recovery prompt is short and rides in argv; the output is
/// plain text rather than a stream-json transcript because nothing reads this
/// second turn's transcript.
fn claude_resume_args(original: &[String], session_id: &str, prompt: &str) -> Vec<String> {
    let mut args = vec![
        "--resume".to_string(),
        session_id.to_string(),
        "--print".to_string(),
        "--output-format".to_string(),
        "text".to_string(),
    ];
    let mut index = 0;
    while index < original.len() {
        match original[index].as_str() {
            flag @ ("--model" | "--effort" | "--agent" | "--allowedTools") => {
                if let Some(value) = original.get(index + 1) {
                    args.push(flag.to_string());
                    args.push(value.clone());
                }
                index += 2;
            }
            flag @ "--dangerously-skip-permissions" => {
                args.push(flag.to_string());
                index += 1;
            }
            _ => index += 1,
        }
    }
    args.push(prompt.to_string());
    args
}

/// Resume a finished session once to recover the JSON answer its turn ended
/// without.
///
/// A print-mode turn is final: an agent that backgrounds a command and ends
/// its turn waiting for a notification has already spent the whole session,
/// and the block that parses its answer fails on prose. Resuming asks the same
/// session — with its full context — for the object it owed. Returns the
/// original answer unchanged when the recovery does not produce one, so the
/// caller still reports the original parse failure rather than a second,
/// less informative one.
async fn resume_for_json(
    session: SessionContext<'_>,
    inv: &Invocation,
    request: &AgentRequest,
    shell: &Arc<dyn ShellGateway>,
    original: String,
) -> String {
    tracing::warn!(
        session_id = session.session_id,
        project = %request.project,
        "agent turn ended without the required JSON answer; resuming the session once to recover it"
    );
    let args = claude_resume_args(&inv.args, session.session_id, RESUME_FOR_JSON_PROMPT);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_opt = (!inv.env.is_empty()).then_some(inv.env.as_slice());
    match shell
        .run(&request.working_dir, "claude", &arg_refs, env_opt, Some(request.timeout))
        .await
    {
        Ok(result) if carries_json_object(&result.stdout) => {
            tracing::warn!(
                session_id = session.session_id,
                project = %request.project,
                "recovered the required JSON answer by resuming the session"
            );
            result.stdout
        }
        Ok(result) => {
            tracing::warn!(
                session_id = session.session_id,
                project = %request.project,
                exit_code = result.exit_code,
                "resumed session still returned no JSON answer; keeping the original answer"
            );
            original
        }
        Err(e) => {
            tracing::warn!(
                session_id = session.session_id,
                project = %request.project,
                error = %e,
                "could not resume the session to recover the JSON answer"
            );
            original
        }
    }
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
    effective_effort: ReasoningEffort,
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
        effective_effort: effective_effort.as_str().to_string(),
        access: request.access.label().to_string(),
        started_at: Utc::now().to_rfc3339(),
        trace_id: request.trace_id.clone().unwrap_or_default(),
    };
    // Set on the envelope as well as the payload: everything that reconstructs
    // a workflow reads the envelope, so this is what makes a session's spend
    // land inside the trace that incurred it rather than beside it.
    let started_event = Event::new(
        EventType::AgentSessionStarted,
        request.project.clone(),
        Throttle::Full,
        serde_json::to_value(&started_payload)?,
    )
    .with_trace_id(request.trace_id.clone());
    // Best-effort: a send error means no Watch subscribers are attached,
    // which is the normal steady state; session emission must not depend on
    // a listener.
    if let Err(e) = event_tx.send(started_event) {
        tracing::debug!(error = %e, session_id, "no Watch subscribers for AgentSessionStarted");
    }
    Ok(())
}

/// Recover what a finished session spent, and price it.
///
/// Every token Foundry spends passes through a session transcript, so this is
/// the one place the estate's cost becomes visible. `model_hint` is the model
/// the gateway resolved for the request — Codex transcripts never name their
/// own model, and without the hint its spend cannot be priced at all.
///
/// A session that ended before writing a terminal usage record returns
/// `(None, CostEstimate::unmeasured())`: the work happened and the tokens were
/// billed, so reporting zero would understate the estate. Unmeasured is a
/// distinct answer from free.
pub(crate) fn price_session(
    log_path: &std::path::Path,
    model_hint: &str,
) -> (Option<SessionUsage>, CostEstimate) {
    let usage = match token_usage::parse_transcript(log_path, Some(model_hint)) {
        Ok(Some(usage)) => usage,
        Ok(None) => return (None, CostEstimate::unmeasured()),
        Err(e) => {
            tracing::debug!(error = %e, path = %log_path.display(), "could not read session transcript for usage");
            return (None, CostEstimate::unmeasured());
        }
    };

    // Read the book per session rather than caching it: rates are runtime data
    // an operator edits between runs, and a session ends rarely enough that a
    // few-KB read is irrelevant next to the inference that preceded it.
    let book =
        RateBook::load(&paths::token_rates_path()).unwrap_or_else(|_| RateBook::default_seed());
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let cost = token_rates::estimate(&usage, &book, &today);

    if !cost.unpriced_models.is_empty() {
        // A model missing from the book means real spend is being counted at
        // zero. Warn rather than debug: this silently understates the estate.
        tracing::warn!(
            models = ?cost.unpriced_models,
            path = %log_path.display(),
            "agent session spent tokens on models absent from the price book; cost understated"
        );
    }

    (Some(usage), cost)
}

/// Emit an `AgentSessionEnded` event on `event_tx`.
pub(crate) fn emit_session_ended(
    event_tx: &broadcast::Sender<Event>,
    project: &str,
    trace_id: Option<String>,
    payload: &AgentSessionEndedPayload,
) -> Result<()> {
    let ended_event = Event::new(
        EventType::AgentSessionEnded,
        project.to_string(),
        Throttle::Full,
        serde_json::to_value(payload)?,
    )
    .with_trace_id(trace_id);
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
    use crate::agent_stream::{AgentStreamOutcome, AgentStreamRunner, StreamRun, StreamedLine};
    use foundry_sdk::event::EventType;
    use std::path::PathBuf;
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
            run: StreamRun<'a>,
        ) -> Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<AgentStreamOutcome>> + Send + 'a>,
        > {
            let log_path = run.log_path;
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
            trace_id: None,
            requires_json: false,
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
            trace_id: None,
            requires_json: false,
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
            trace_id: None,
            requires_json: false,
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
            stdin: Arc<Mutex<Option<Vec<u8>>>>,
        }

        impl AgentStreamRunner for ArgRecorder {
            fn run<'a>(
                &'a self,
                run: StreamRun<'a>,
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = anyhow::Result<AgentStreamOutcome>>
                        + Send
                        + 'a,
                >,
            > {
                let recorded = self.recorded.clone();
                let stdin_seen = self.stdin.clone();
                let log_path = run.log_path;
                let captured: Vec<String> = run.args.iter().map(|s| (*s).to_string()).collect();
                let stdin = run.stdin.map(<[u8]>::to_vec);
                Box::pin(async move {
                    if let Some(parent) = log_path.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::File::create(log_path).await?;
                    *recorded.lock().unwrap() = captured;
                    *stdin_seen.lock().unwrap() = stdin;
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
        let stdin_seen: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let runner = Arc::new(ArgRecorder {
            recorded: recorded.clone(),
            stdin: stdin_seen.clone(),
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
            trace_id: None,
            requires_json: false,
        };
        let _ = gateway.invoke(&request).await.unwrap();

        let captured = recorded.lock().unwrap().clone();
        // The prompt rides on stdin, never in argv: a single argv element is
        // capped at MAX_ARG_STRLEN (131072 bytes) on Linux.
        assert!(!captured.iter().any(|a| a == "x"), "prompt must not be in argv: {captured:?}");
        assert_eq!(captured.last().map(String::as_str), Some("-p"), "args: {captured:?}");
        assert_eq!(
            stdin_seen.lock().unwrap().as_deref(),
            Some(b"x".as_slice()),
            "prompt should be delivered on stdin"
        );
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
mod claude_json_recovery_tests {
    use super::fakes::FakeShellGateway;
    use super::*;
    use crate::agent_stream::{AgentStreamOutcome, AgentStreamRunner, StreamRun, StreamedLine};
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::time::Duration;
    use tokio::sync::broadcast;

    /// Runner that reports one successful turn whose final answer is `answer`.
    ///
    /// Stands in for the Claude CLI closely enough to keep the recovery path
    /// honest: the CLI owns the session id the conversation is stored under and
    /// reports it in the result envelope, adopting the `--session-id` it was
    /// handed. A resume that names any other id would fail with "No
    /// conversation found with session ID", so the tests assert against the id
    /// this runner reports, never against the one Foundry logged.
    struct AnswerRunner {
        answer: String,
        /// The argv of the original invocation, and the `session_id` the fake
        /// CLI reported for it.
        observed: std::sync::Mutex<Option<(Vec<String>, String)>>,
    }

    impl AnswerRunner {
        fn new(answer: &str) -> Self {
            Self {
                answer: answer.to_string(),
                observed: std::sync::Mutex::new(None),
            }
        }

        /// The argv of the original invocation.
        fn original_args(&self) -> Vec<String> {
            self.observed.lock().expect("observed").clone().expect("a turn ran").0
        }

        /// The session id the fake CLI stored the conversation under.
        fn cli_session_id(&self) -> String {
            self.observed.lock().expect("observed").clone().expect("a turn ran").1
        }
    }

    impl AgentStreamRunner for AnswerRunner {
        fn run<'a>(
            &'a self,
            run: StreamRun<'a>,
        ) -> Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<AgentStreamOutcome>> + Send + 'a>,
        > {
            let log_path = run.log_path;
            // The real CLI adopts `--session-id` when given one, and otherwise
            // mints its own id that the caller never learns.
            let requested = run
                .args
                .iter()
                .position(|a| *a == "--session-id")
                .and_then(|at| run.args.get(at + 1))
                .map(|id| (*id).to_string());
            let cli_session_id =
                requested.unwrap_or_else(|| "cli-minted-session-id-unknown-to-foundry".to_string());
            *self.observed.lock().expect("observed") =
                Some((run.args.iter().map(|a| (*a).to_string()).collect(), cli_session_id.clone()));
            let line = serde_json::json!({
                "type": "result",
                "subtype": "success",
                "session_id": cli_session_id,
                "result": self.answer.clone(),
            })
            .to_string();
            Box::pin(async move {
                if let Some(parent) = log_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(log_path, format!("{line}\n")).await?;
                Ok(AgentStreamOutcome {
                    exit_code: 0,
                    success: true,
                    stderr: String::new(),
                    bytes_written: line.len() as u64,
                    lines: vec![StreamedLine { raw: line }],
                })
            })
        }
    }

    fn review_request(requires_json: bool) -> AgentRequest {
        AgentRequest {
            prompt: "review this".to_string(),
            project: "demo-project".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Deep,
            effort: ReasoningEffort::High,
            agent_file: None,
            provider: None,
            env: Vec::new(),
            timeout: Duration::from_secs(30),
            trace_id: None,
            requires_json,
        }
    }

    /// The observed failure: the reviewer ended its turn waiting on a
    /// backgrounded command, so the block got prose where it needed a verdict.
    const WAITING_ANSWER: &str = "Nothing else is outstanding except the background mutation run, so I'm waiting for its \
         completion notification.";

    fn recovered_json() -> CommandResult {
        CommandResult {
            stdout: "{\"verdict\":\"complete\"}".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    #[tokio::test]
    async fn a_turn_without_json_resumes_the_session_once_and_accepts_the_recovered_object() {
        let (tx, mut rx) = broadcast::channel(16);
        let shell = FakeShellGateway::always(recovered_json());
        let runner = Arc::new(AnswerRunner::new(WAITING_ANSWER));
        let gateway = ClaudeAgentGateway::new_with_streaming(
            Arc::clone(&shell) as Arc<dyn ShellGateway>,
            Arc::clone(&runner) as Arc<dyn AgentStreamRunner>,
            test_support::tmp_dir("foundry-resume"),
            tx,
        );

        let response = gateway.invoke(&review_request(true)).await.expect("invoke ok");
        assert_eq!(response.stdout, "{\"verdict\":\"complete\"}");

        // The original invocation must hand the CLI Foundry's id, so the CLI
        // stores the conversation under an id Foundry can resume.
        let original = runner.original_args();
        let given_at = original
            .iter()
            .position(|a| a == "--session-id")
            .expect("--session-id on the turn");
        let given = original[given_at + 1].clone();
        let cli_session_id = runner.cli_session_id();
        assert_eq!(cli_session_id, given, "the CLI must adopt the id Foundry handed it");

        let started = rx.recv().await.expect("started event");
        assert_eq!(
            started.payload["session_id"].as_str(),
            Some(cli_session_id.as_str()),
            "the logged session id and the CLI's must coincide"
        );

        let calls = shell.invocations();
        assert_eq!(calls.len(), 1, "the session must be resumed exactly once: {calls:?}");
        assert_eq!(calls[0].command, "claude");
        let resume_at = calls[0].args.iter().position(|a| a == "--resume").expect("--resume flag");
        assert_eq!(
            calls[0].args.get(resume_at + 1),
            Some(&cli_session_id),
            "the resume must name the id the CLI stored the conversation under"
        );
    }

    #[tokio::test]
    async fn a_resume_that_also_returns_no_json_keeps_the_original_answer() {
        let (tx, _rx) = broadcast::channel(16);
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "still waiting on the background run".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let gateway = ClaudeAgentGateway::new_with_streaming(
            Arc::clone(&shell) as Arc<dyn ShellGateway>,
            Arc::new(AnswerRunner::new(WAITING_ANSWER)),
            test_support::tmp_dir("foundry-resume-fail"),
            tx,
        );

        let response = gateway.invoke(&review_request(true)).await.expect("invoke ok");
        assert_eq!(
            response.stdout, WAITING_ANSWER,
            "a failed recovery must leave the caller its original parse failure"
        );
        assert_eq!(shell.invocations().len(), 1, "recovery is attempted at most once");
    }

    #[tokio::test]
    async fn an_answer_that_already_carries_json_is_never_resumed() {
        let (tx, _rx) = broadcast::channel(16);
        let shell = FakeShellGateway::always(recovered_json());
        let gateway = ClaudeAgentGateway::new_with_streaming(
            Arc::clone(&shell) as Arc<dyn ShellGateway>,
            Arc::new(AnswerRunner::new(
                "Here it is:\n```json\n{\"verdict\":\"remainder\",\"gaps\":[\"x\"]}\n```",
            )),
            test_support::tmp_dir("foundry-resume-none"),
            tx,
        );

        let response = gateway.invoke(&review_request(true)).await.expect("invoke ok");
        assert!(response.stdout.contains("remainder"));
        assert!(shell.invocations().is_empty(), "no recovery needed");
    }

    #[tokio::test]
    async fn a_block_that_does_not_parse_json_is_never_resumed() {
        let (tx, _rx) = broadcast::channel(16);
        let shell = FakeShellGateway::always(recovered_json());
        let gateway = ClaudeAgentGateway::new_with_streaming(
            Arc::clone(&shell) as Arc<dyn ShellGateway>,
            Arc::new(AnswerRunner::new("a prose summary")),
            test_support::tmp_dir("foundry-resume-off"),
            tx,
        );

        let response = gateway.invoke(&review_request(false)).await.expect("invoke ok");
        assert_eq!(response.stdout, "a prose summary");
        assert!(shell.invocations().is_empty());
    }

    #[test]
    fn resume_args_carry_the_original_tier_and_read_only_tools() {
        let original = vec![
            "--print".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--model".to_string(),
            "claude-opus-5".to_string(),
            "--effort".to_string(),
            "high".to_string(),
            "--allowedTools".to_string(),
            "Read Glob Grep WebFetch WebSearch".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "-p".to_string(),
        ];
        let args = claude_resume_args(&original, "sess-1", "answer now");

        assert_eq!(args[0], "--resume");
        assert_eq!(args[1], "sess-1");
        let model_at = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[model_at + 1], "claude-opus-5");
        let effort_at = args.iter().position(|a| a == "--effort").expect("--effort");
        assert_eq!(args[effort_at + 1], "high");
        let tools_at = args.iter().position(|a| a == "--allowedTools").expect("--allowedTools");
        assert_eq!(args[tools_at + 1], "Read Glob Grep WebFetch WebSearch");
        assert_eq!(args.last().map(String::as_str), Some("answer now"));
        assert!(!args.iter().any(|a| a == "stream-json"), "recovery reads plain text: {args:?}");
    }

    #[test]
    fn carries_json_object_rejects_prose_and_accepts_a_fenced_object() {
        assert!(!carries_json_object(WAITING_ANSWER));
        assert!(carries_json_object("```json\n{\"verdict\":\"complete\"}\n```"));
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::pin::Pin;

    use crate::agent_stream::{AgentStreamOutcome, AgentStreamRunner, StreamRun, StreamedLine};

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
            run: StreamRun<'a>,
        ) -> Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<AgentStreamOutcome>> + Send + 'a>,
        > {
            let log_path = run.log_path;
            let transcript = self.transcript.clone();
            let last_message = self.last_message.clone();
            let mut outcome = self.outcome.clone();
            let out_path = run
                .args
                .iter()
                .position(|a| *a == "-o")
                .and_then(|i| run.args.get(i + 1))
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
