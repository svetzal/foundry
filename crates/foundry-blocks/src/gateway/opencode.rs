//! `OpencodeAgentGateway` — drives the `opencode` CLI (OpenAI-backed agentic
//! runner) behind the provider-neutral [`AgentGateway`] trait.
//!
//! This is the migration target away from the `claude` CLI: the Anthropic
//! subscription stops covering hands-free agent work, so unattended `foundryd`
//! runs must use an `OpenAI` provider. `opencode` is an agentic CLI (tool-use,
//! file editing) — so the Mutator blocks (`ExecutePlan`, `ExecuteMaintain`,
//! `Remediate*`) keep working; a chat-completion client could not do that.
//!
//! The invocation contract (validated against opencode v1.15.12, `OpenAI` oauth):
//!
//! - `opencode run --format json --dangerously-skip-permissions --model <m>
//!   --variant <effort> [--agent <key>] -- <prompt>`, spawned with **stdin
//!   closed** (the shared [`AgentStreamRunner`] does this — opencode hangs forever
//!   after bootstrap on an inherited open stdin).
//! - stdout is JSONL; every event carries a `sessionID`. There is no single
//!   `{"type":"result"}` envelope like Claude's, so the authoritative final text
//!   is fetched after the run via `opencode export <sessionID>` (through the
//!   [`ShellGateway`]) and read from the last `assistant` message's `text` parts.
//!   A stream-only fallback concatenates `text` events when export is unavailable.
//! - opencode can exit `0` while emitting `{"type":"error"}` events — those count
//!   as failure (mirrors hopper's effective-exit-code rule).
//! - agent definitions live in `~/.claude/agents/<name>.md` (resolved by blocks
//!   and passed as `agent_file`); opencode's `--agent` takes a *name*, so the body
//!   is inlined via the `OPENCODE_CONFIG_CONTENT` env var.

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

/// Production [`AgentGateway`] that invokes the `opencode` CLI and emits
/// `AgentSessionStarted` / `AgentSessionEnded` lifecycle events.
pub struct OpencodeAgentGateway {
    /// Used for the post-run `opencode export <sessionID>` call.
    shell: Arc<dyn ShellGateway>,
    stream_runner: Arc<dyn AgentStreamRunner>,
    session_log_dir: PathBuf,
    event_tx: broadcast::Sender<Event>,
}

impl OpencodeAgentGateway {
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

    /// Capability → provider-qualified model id.
    fn model_for(capability: AgentCapability) -> &'static str {
        match capability {
            AgentCapability::Reasoning => "openai/gpt-5.5",
            AgentCapability::Coding => "openai/gpt-5.4",
            AgentCapability::Quick => "openai/gpt-5.4-mini",
        }
    }

    /// Capability → `--variant` (provider reasoning effort).
    fn variant_for(capability: AgentCapability) -> &'static str {
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

impl AgentGateway for OpencodeAgentGateway {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            let session_id = Uuid::new_v4().to_string();
            let log_path = self.session_log_dir.join(format!("{session_id}.jsonl"));

            let model = Self::model_for(request.capability);
            let variant = Self::variant_for(request.capability);

            // Inline the agent definition (if any) via OPENCODE_CONFIG_CONTENT and
            // pass `--agent <key>`. opencode's `--agent` wants a name, not a path.
            let (agent_key, env) =
                prepare_agent_config(request.agent_file.as_deref(), &request.project);

            // ReadOnly is advisory under opencode v1.15.12: there is no CLI
            // tool-allowlist, and the per-agent permission schema is unverified.
            // The iterate/maintain safety net (commit only on passing gates,
            // discard uncommitted drift) is the actual guard. See plan open items.
            if request.access == AgentAccess::ReadOnly {
                tracing::debug!(
                    project = %request.project,
                    "opencode: ReadOnly access is advisory (no tool allowlist); relying on workflow safety net"
                );
            }

            let args = build_opencode_argv(model, variant, agent_key.as_deref(), &request.prompt);
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let started_payload = AgentSessionStartedPayload {
                session_id: session_id.clone(),
                agent_type: "opencode".to_string(),
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

            let env_opt = if env.is_empty() {
                None
            } else {
                Some(env.as_slice())
            };
            let outcome = self
                .stream_runner
                .run(
                    &request.working_dir,
                    "opencode",
                    &arg_refs,
                    env_opt,
                    Some(request.timeout),
                    &log_path,
                )
                .await;

            let (status, exit_code, stderr, bytes_written, error_msg, stdout_text) = match outcome {
                Ok(o) => {
                    let error_events = count_error_events(&o.lines);
                    let success = o.success && error_events == 0;

                    // Authoritative result: `opencode export <sessionID>`. Fall back
                    // to concatenating streamed `text` events if export is missing
                    // or fails.
                    let stdout_text = match extract_session_id(&o.lines) {
                        Some(sid) => self
                            .export_result(&request.working_dir, &sid, request.timeout)
                            .await
                            .unwrap_or_else(|| extract_text_from_stream(&o.lines)),
                        None => extract_text_from_stream(&o.lines),
                    };

                    let status = if success { "ok" } else { "agent_failed" };
                    let exit_code = if o.success && error_events > 0 {
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

impl OpencodeAgentGateway {
    /// Run `opencode export <session_id>` and extract the final assistant text.
    /// Returns `None` if the export command fails or yields no parseable result.
    async fn export_result(
        &self,
        working_dir: &std::path::Path,
        session_id: &str,
        timeout: std::time::Duration,
    ) -> Option<String> {
        let args = ["export", session_id];
        match self.shell.run(working_dir, "opencode", &args, None, Some(timeout)).await {
            Ok(cr) if cr.success => parse_export_result(&cr.stdout),
            Ok(cr) => {
                tracing::warn!(
                    session_id = %session_id,
                    exit_code = cr.exit_code,
                    "opencode export failed; falling back to stream text"
                );
                None
            }
            Err(err) => {
                tracing::warn!(session_id = %session_id, error = %err, "opencode export errored");
                None
            }
        }
    }
}

// --- Pure helpers (unit-tested without spawning) ----------------------------

/// Resolve the optional agent file into an opencode `--agent` key plus the
/// `OPENCODE_CONFIG_CONTENT` env entry that inlines its system prompt. Returns
/// `(None, [])` when there is no agent file or it can't be read.
fn prepare_agent_config(
    agent_file: Option<&std::path::Path>,
    project: &str,
) -> (Option<String>, Vec<(String, String)>) {
    let Some(agent_file) = agent_file else {
        return (None, Vec::new());
    };
    match std::fs::read_to_string(agent_file) {
        Ok(body) => {
            let key = derive_agent_key(agent_file);
            let stripped = strip_frontmatter(&body);
            let env = vec![(
                "OPENCODE_CONFIG_CONTENT".to_string(),
                build_config_content(&key, &stripped),
            )];
            (Some(key), env)
        }
        Err(err) => {
            tracing::warn!(
                project = %project,
                agent_file = %agent_file.display(),
                error = %err,
                "opencode: could not read agent file; proceeding without --agent"
            );
            (None, Vec::new())
        }
    }
}

/// Build the `opencode run` argv. Prompt is passed positionally after `--`.
fn build_opencode_argv(
    model: &str,
    variant: &str,
    agent_key: Option<&str>,
    prompt: &str,
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--format".to_string(),
        "json".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--model".to_string(),
        model.to_string(),
        "--variant".to_string(),
        variant.to_string(),
    ];
    if let Some(key) = agent_key {
        args.push("--agent".to_string());
        args.push(key.to_string());
    }
    args.push("--".to_string());
    args.push(prompt.to_string());
    args
}

/// Derive a stable opencode agent key from an agent-definition file path
/// (the file stem, sanitized to `[a-z0-9-]`). Falls back to `foundry-agent`.
fn derive_agent_key(agent_file: &std::path::Path) -> String {
    let stem = agent_file.file_stem().and_then(|s| s.to_str()).unwrap_or("foundry-agent");
    let key: String = stem
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = key.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "foundry-agent".to_string()
    } else {
        trimmed
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

/// Build the `OPENCODE_CONFIG_CONTENT` JSON inlining an agent's system prompt.
fn build_config_content(agent_key: &str, body: &str) -> String {
    serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "agent": {
            agent_key: {
                "description": format!("Foundry-injected agent: {agent_key}"),
                "prompt": body,
                "mode": "primary",
            }
        }
    })
    .to_string()
}

/// First `sessionID` seen in the JSONL stream.
fn extract_session_id(lines: &[StreamedLine]) -> Option<String> {
    for line in lines {
        if let Ok(v) = serde_json::from_str::<Value>(&line.raw) {
            if let Some(s) = v.get("sessionID").and_then(Value::as_str) {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Count `{"type":"error"}` events in the stream (opencode can exit 0 with these).
fn count_error_events(lines: &[StreamedLine]) -> usize {
    lines
        .iter()
        .filter(|l| {
            serde_json::from_str::<Value>(&l.raw)
                .ok()
                .and_then(|v| v.get("type").and_then(Value::as_str).map(|t| t == "error"))
                .unwrap_or(false)
        })
        .count()
}

/// Fallback result extraction: concatenate `text` events' `part.text` in order.
fn extract_text_from_stream(lines: &[StreamedLine]) -> String {
    let mut out = String::new();
    for line in lines {
        if let Ok(v) = serde_json::from_str::<Value>(&line.raw) {
            if v.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = v.pointer("/part/text").and_then(Value::as_str) {
                    out.push_str(t);
                }
            }
        }
    }
    out.trim().to_string()
}

/// Parse `opencode export <id>` stdout → final assistant text. Tolerates a
/// leading status line before the JSON object (strips up to the first `{`).
fn parse_export_result(stdout: &str) -> Option<String> {
    let start = stdout.find('{')?;
    let v: Value = serde_json::from_str(&stdout[start..]).ok()?;
    let messages = v.get("messages")?.as_array()?;
    let last_assistant = messages
        .iter()
        .rev()
        .find(|m| m.pointer("/info/role").and_then(Value::as_str) == Some("assistant"))?;
    let parts = last_assistant.get("parts")?.as_array()?;
    let mut out = String::new();
    for p in parts {
        if p.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                out.push_str(t);
            }
        }
    }
    Some(out.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &str) -> StreamedLine {
        StreamedLine { raw: s.to_string() }
    }

    #[test]
    fn argv_includes_run_format_model_variant_and_prompt() {
        let args = build_opencode_argv("openai/gpt-5.4", "medium", None, "do the thing");
        assert_eq!(args[0], "run");
        assert!(args.iter().any(|a| a == "--format"));
        assert!(args.iter().any(|a| a == "json"));
        assert!(args.iter().any(|a| a == "--dangerously-skip-permissions"));
        let mp = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[mp + 1], "openai/gpt-5.4");
        let vp = args.iter().position(|a| a == "--variant").unwrap();
        assert_eq!(args[vp + 1], "medium");
        // prompt is last, after the `--` terminator
        assert_eq!(args.last().unwrap(), "do the thing");
        let dd = args.iter().position(|a| a == "--").unwrap();
        assert!(dd < args.len() - 1);
        // no --agent when none supplied
        assert!(!args.iter().any(|a| a == "--agent"));
    }

    #[test]
    fn argv_includes_agent_key_when_supplied() {
        let args = build_opencode_argv("openai/gpt-5.5", "high", Some("rust-craftsperson"), "p");
        let ap = args.iter().position(|a| a == "--agent").unwrap();
        assert_eq!(args[ap + 1], "rust-craftsperson");
    }

    #[test]
    fn capability_maps_to_expected_model_and_variant() {
        assert_eq!(OpencodeAgentGateway::model_for(AgentCapability::Reasoning), "openai/gpt-5.5");
        assert_eq!(OpencodeAgentGateway::variant_for(AgentCapability::Reasoning), "high");
        assert_eq!(OpencodeAgentGateway::model_for(AgentCapability::Coding), "openai/gpt-5.4");
        assert_eq!(OpencodeAgentGateway::variant_for(AgentCapability::Coding), "medium");
        assert_eq!(OpencodeAgentGateway::model_for(AgentCapability::Quick), "openai/gpt-5.4-mini");
        assert_eq!(OpencodeAgentGateway::variant_for(AgentCapability::Quick), "low");
    }

    #[test]
    fn derive_agent_key_sanitizes_stem() {
        assert_eq!(
            derive_agent_key(std::path::Path::new("/x/rust-craftsperson.md")),
            "rust-craftsperson"
        );
        assert_eq!(derive_agent_key(std::path::Path::new("/x/Weird Name.md")), "weird-name");
        // Stem that sanitizes to all-dashes falls back to the default key.
        assert_eq!(derive_agent_key(std::path::Path::new("/x/___.md")), "foundry-agent");
    }

    #[test]
    fn strip_frontmatter_removes_yaml_block() {
        let md = "---\nname: foo\ndescription: bar\n---\nYou are a helpful agent.\n";
        assert_eq!(strip_frontmatter(md), "You are a helpful agent.");
    }

    #[test]
    fn strip_frontmatter_passthrough_when_absent() {
        assert_eq!(strip_frontmatter("Just a body."), "Just a body.");
    }

    #[test]
    fn build_config_content_inlines_prompt_under_agent_key() {
        let json = build_config_content("rust-craftsperson", "You are a Rust expert.");
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v.pointer("/agent/rust-craftsperson/prompt").and_then(Value::as_str),
            Some("You are a Rust expert.")
        );
        assert_eq!(
            v.pointer("/agent/rust-craftsperson/mode").and_then(Value::as_str),
            Some("primary")
        );
    }

    #[test]
    fn extract_session_id_returns_first() {
        let lines = vec![
            line(r#"{"type":"step_start","sessionID":"ses_abc","part":{}}"#),
            line(r#"{"type":"text","sessionID":"ses_abc","part":{"text":"hi"}}"#),
        ];
        assert_eq!(extract_session_id(&lines), Some("ses_abc".to_string()));
    }

    #[test]
    fn count_error_events_counts_only_errors() {
        let lines = vec![
            line(r#"{"type":"step_start","sessionID":"s"}"#),
            line(r#"{"type":"error","sessionID":"s","error":"boom"}"#),
            line(r#"{"type":"text","sessionID":"s","part":{"text":"x"}}"#),
        ];
        assert_eq!(count_error_events(&lines), 1);
    }

    #[test]
    fn extract_text_from_stream_concatenates_text_parts() {
        let lines = vec![
            line(r#"{"type":"step_start","sessionID":"s","part":{}}"#),
            line(r#"{"type":"text","sessionID":"s","part":{"text":"PO"}}"#),
            line(r#"{"type":"text","sessionID":"s","part":{"text":"NG"}}"#),
            line(r#"{"type":"step_finish","sessionID":"s"}"#),
        ];
        assert_eq!(extract_text_from_stream(&lines), "PONG");
    }

    #[test]
    fn parse_export_result_walks_to_last_assistant_text() {
        // Mirrors the real opencode v1.15.12 export shape.
        let export = r#"{
            "info": {"id": "ses_x", "model": {"id": "gpt-5.4", "providerID": "openai"}},
            "messages": [
                {"info": {"role": "user"}, "parts": [{"type": "text", "text": "Reply PONG"}]},
                {"info": {"role": "assistant"}, "parts": [
                    {"type": "step-start"},
                    {"type": "text", "text": "PONG"},
                    {"type": "step-finish"}
                ]}
            ]
        }"#;
        assert_eq!(parse_export_result(export), Some("PONG".to_string()));
    }

    #[test]
    fn parse_export_result_strips_leading_status_line() {
        let export = "Exporting session: ses_x\n{\"messages\":[{\"info\":{\"role\":\"assistant\"},\"parts\":[{\"type\":\"text\",\"text\":\"hi\"}]}]}";
        assert_eq!(parse_export_result(export), Some("hi".to_string()));
    }

    #[test]
    fn parse_export_result_none_on_garbage() {
        assert_eq!(parse_export_result("not json at all"), None);
    }

    // --- Full invoke() flow (offline: fake stream runner + fake shell) -------

    use crate::agent_stream::{AgentStreamOutcome, AgentStreamRunner};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Fake stream runner: tees a canned opencode JSONL transcript to the log
    /// file and returns it as `outcome.lines`.
    struct FakeRunner {
        transcript: Vec<String>,
        outcome: AgentStreamOutcome,
    }

    impl AgentStreamRunner for FakeRunner {
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
            let mut outcome = self.outcome.clone();
            Box::pin(async move {
                if let Some(parent) = log_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(log_path, transcript.join("\n")).await?;
                outcome.lines = transcript.into_iter().map(|raw| StreamedLine { raw }).collect();
                Ok(outcome)
            })
        }
    }

    fn tmp_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("foundry-oc-test-{}", Uuid::new_v4()));
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
    async fn invoke_extracts_export_result_and_emits_lifecycle_events() {
        let transcript = vec![
            r#"{"type":"step_start","sessionID":"ses_test","part":{}}"#.to_string(),
            r#"{"type":"text","sessionID":"ses_test","part":{"text":"stream-partial"}}"#
                .to_string(),
            r#"{"type":"step_finish","sessionID":"ses_test"}"#.to_string(),
        ];
        let runner = Arc::new(FakeRunner {
            transcript,
            outcome: ok_outcome(),
        });

        // Export call returns the authoritative answer (should win over stream text).
        let export_json = r#"{"messages":[{"info":{"role":"assistant"},"parts":[{"type":"text","text":"EXPORTED ANSWER"}]}]}"#;
        let shell =
            crate::gateway::fakes::FakeShellGateway::always(crate::gateway::CommandResult {
                stdout: export_json.to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            });

        let (tx, mut rx) = broadcast::channel(16);
        let gateway = OpencodeAgentGateway::new_with_streaming(shell, runner, tmp_dir(), tx);

        let request = AgentRequest {
            prompt: "say something".to_string(),
            project: "demo".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::Full,
            capability: AgentCapability::Coding,
            agent_file: None,
            timeout: Duration::from_secs(5),
        };

        let response = gateway.invoke(&request).await.expect("invoke ok");
        assert!(response.success);
        assert_eq!(response.exit_code, 0);
        assert_eq!(response.stdout, "EXPORTED ANSWER");

        let started = rx.recv().await.expect("started");
        assert_eq!(started.event_type, EventType::AgentSessionStarted);
        assert_eq!(started.payload["agent_type"], "opencode");
        assert_eq!(started.payload["capability"], "coding");

        let ended = rx.recv().await.expect("ended");
        assert_eq!(ended.event_type, EventType::AgentSessionEnded);
        assert_eq!(ended.payload["status"], "ok");
    }

    #[tokio::test]
    async fn invoke_treats_error_event_as_failure_despite_exit_zero() {
        let transcript = vec![
            r#"{"type":"step_start","sessionID":"ses_e","part":{}}"#.to_string(),
            r#"{"type":"error","sessionID":"ses_e","error":"provider exploded"}"#.to_string(),
        ];
        // Process itself exited 0, but an error event is present.
        let runner = Arc::new(FakeRunner {
            transcript,
            outcome: ok_outcome(),
        });
        let shell = crate::gateway::fakes::FakeShellGateway::failure("no session");

        let (tx, mut rx) = broadcast::channel(16);
        let gateway = OpencodeAgentGateway::new_with_streaming(shell, runner, tmp_dir(), tx);

        let request = AgentRequest {
            prompt: "x".to_string(),
            project: "demo".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::Full,
            capability: AgentCapability::Quick,
            agent_file: None,
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
