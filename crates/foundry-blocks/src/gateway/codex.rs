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
//!   {-s read-only | --dangerously-bypass-approvals-and-sandbox} -`.
//! - The prompt is delivered on **stdin**, not in argv. The final `-`
//!   argument tells `codex exec` to read its instructions from stdin (validated
//!   against `codex-cli` 0.159.2). The shared [`AgentStreamRunner`] writes the
//!   prompt and then closes the pipe, so `codex exec` sees end-of-input rather
//!   than blocking on an open stdin. The prompt must not ride in argv: Linux
//!   caps one argv element at `MAX_ARG_STRLEN` (131072 bytes), and a larger
//!   prompt fails the spawn outright with `Argument list too long` (`E2BIG`).
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

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use foundry_sdk::event::Event;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::agent_stream::{
    AgentStreamOutcome, AgentStreamRunner, ProcessAgentStreamRunner, StreamedLine,
};

use super::{
    AgentAccess, AgentFailureMetadata, AgentGateway, AgentProvider, AgentRequest, AgentResponse,
    ProviderModels, ShellGateway,
    engine::{CliAgentAdapter, CliAgentGateway, Interpreted, Invocation, SessionContext},
};

/// Adapter that captures the codex-specific CLI invocation contract.
pub(crate) struct CodexAdapter;

impl CliAgentAdapter for CodexAdapter {
    fn provider(&self) -> AgentProvider {
        AgentProvider::Codex
    }

    fn agent_type(&self) -> &'static str {
        "codex"
    }

    fn command(&self) -> &'static str {
        "codex"
    }

    fn build_invocation(
        &self,
        request: &AgentRequest,
        model: &str,
        effort: &str,
        session_id: &str,
        session_log_dir: &Path,
    ) -> Invocation {
        let last_message_path = session_log_dir.join(format!("{session_id}.last.txt"));

        // Prepend the agent persona (if any) to the prompt — codex has no
        // `--agent` flag.
        let prompt = build_prompt(request.agent_file.as_deref(), &request.prompt, &request.project);

        let mut args = build_codex_argv(model, effort, request.access, &last_message_path);
        args.splice(1..1, ["-c".into(), "tool_output_token_limit=2000".into()]);
        if request.env.iter().any(|(k, _)| k == "FOUNDRY_WRITABLE_ROOT") {
            args.retain(|a| a != "--dangerously-bypass-approvals-and-sandbox");
            args.splice(
                1..1,
                [
                    "-s".into(),
                    "workspace-write".into(),
                    "-c".into(),
                    "approval_policy=never".into(),
                    "-c".into(),
                    format!(
                        "sandbox_workspace_write.writable_roots=[{}]",
                        serde_json::json!(foundry_sdk::paths::foundry_home().join("tool-logs"))
                    ),
                    "-c".into(),
                    "sandbox_workspace_write.network_access=true".into(),
                ],
            );
        }

        Invocation {
            args,
            env: vec![],
            // The trailing `-` in argv makes codex read the prompt from stdin.
            stdin: Some(prompt.into_bytes()),
            last_message_path: Some(last_message_path),
        }
    }

    fn interpret<'a>(
        &'a self,
        outcome: &'a AgentStreamOutcome,
        _session: SessionContext<'a>,
        inv: &'a Invocation,
        _request: &'a AgentRequest,
        _shell: &'a Arc<dyn ShellGateway>,
    ) -> Pin<Box<dyn std::future::Future<Output = Interpreted> + Send + 'a>> {
        Box::pin(async move {
            let failed_event = has_failure_event(&outcome.lines);
            let success = outcome.success && !failed_event;

            // Authoritative result: the `-o` last-message file. Fall back
            // to the last `agent_message` event in the stream.
            let stdout = if let Some(ref p) = inv.last_message_path {
                read_last_message(p)
                    .await
                    .unwrap_or_else(|| extract_agent_message(&outcome.lines))
            } else {
                extract_agent_message(&outcome.lines)
            };

            let exit_code = if outcome.success && failed_event {
                1
            } else {
                outcome.exit_code
            };

            // Best-effort cleanup of the transient last-message file.
            if let Some(ref p) = inv.last_message_path
                && let Err(e) = tokio::fs::remove_file(p).await
            {
                tracing::debug!(error = %e, path = %p.display(), "failed to remove transient last-message file");
            }

            // On failure, carry the real diagnostic from the JSONL stream (the
            // `turn.failed`/`error` event's message) rather than leaving the
            // caller to fall back to stderr — codex always writes the same
            // harmless "Reading additional input from stdin..." line to stderr
            // regardless of outcome, so stderr alone identifies nothing about
            // *why* a run failed. See the module doc for the stdin-probe note.
            let failure =
                (!success).then(|| extract_failure_message(&outcome.lines)).flatten().map(
                    |message| AgentFailureMetadata::new(AgentProvider::Codex).with_message(message),
                );

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
    /// Production [`AgentGateway`] that invokes the `codex` CLI and emits
    /// `AgentSessionStarted` / `AgentSessionEnded` lifecycle events.
    CodexAgentGateway, CodexAdapter
}

// --- Pure helpers (unit-tested without spawning) ----------------------------

/// Build the `codex exec` argv. The last argument is `-`, which makes codex
/// read its prompt from stdin — the prompt itself never rides in argv.
fn build_codex_argv(
    model: &str,
    effort: &str,
    access: AgentAccess,
    last_message_path: &std::path::Path,
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
    args.push("-".to_string());
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
            let persona = super::strip_frontmatter(&body);
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
        Err(e) => {
            // Best-effort: the `-o` last-message file is expected to be missing or
            // unreadable when codex exits before writing it (e.g. early failure);
            // this is documented as a normal `None` case, but log at debug for
            // investigability.
            tracing::debug!(error = %e, path = %path.display(), "codex: last-message file not readable");
            None
        }
    }
}

/// Fallback result extraction: the text of the last `agent_message`
/// `item.completed` event in the JSONL stream.
fn extract_agent_message(lines: &[StreamedLine]) -> String {
    for line in lines.iter().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(&line.raw)
            && v.get("type").and_then(Value::as_str) == Some("item.completed")
            && v.pointer("/item/type").and_then(Value::as_str) == Some("agent_message")
            && let Some(t) = v.pointer("/item/text").and_then(Value::as_str)
        {
            return t.trim().to_string();
        }
    }
    String::new()
}

/// Extract the real diagnostic behind a failed run: the last `turn.failed` or
/// top-level `error` event's message, preferring `turn.failed` since it is the
/// terminal, authoritative failure record when both appear.
///
/// codex often wraps the message in a JSON-encoded string (the raw provider
/// API error body); when that inner text itself parses as JSON with an
/// `error.message` field, that nested message is returned instead, since it is
/// the human-readable cause (e.g. "The 'x' model is not supported...") rather
/// than a doubly-escaped blob.
fn extract_failure_message(lines: &[StreamedLine]) -> Option<String> {
    let mut top_level_error: Option<String> = None;
    let mut turn_failed: Option<String> = None;

    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(&line.raw) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("error") => {
                if let Some(m) = v.get("message").and_then(Value::as_str) {
                    top_level_error = Some(m.to_string());
                }
            }
            Some("turn.failed") => {
                if let Some(m) = v.pointer("/error/message").and_then(Value::as_str) {
                    turn_failed = Some(m.to_string());
                }
            }
            _ => {}
        }
    }

    let raw = turn_failed.or(top_level_error)?;
    Some(unwrap_nested_error_message(&raw))
}

/// Unwrap a message that is itself a JSON-encoded provider error body,
/// returning its inner `error.message` when present, or the original text
/// unchanged when it isn't JSON or lacks that shape.
fn unwrap_nested_error_message(raw: &str) -> String {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| raw.to_string())
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
    use crate::gateway::{ModelTier, ReasoningEffort};
    use foundry_sdk::event::EventType;
    use std::path::Path;
    use uuid::Uuid;

    fn line(s: &str) -> StreamedLine {
        StreamedLine { raw: s.to_string() }
    }

    #[test]
    fn argv_includes_exec_json_model_effort_output_and_stdin_marker() {
        let out = Path::new("/tmp/sess.last.txt");
        let args = build_codex_argv("gpt-5.4", "medium", AgentAccess::Full, out);
        assert_eq!(args[0], "exec");
        assert!(args.iter().any(|a| a == "--json"));
        assert!(args.iter().any(|a| a == "--skip-git-repo-check"));
        let mp = args.iter().position(|a| a == "-m").unwrap();
        assert_eq!(args[mp + 1], "gpt-5.4");
        let cp = args.iter().position(|a| a == "-c").unwrap();
        assert_eq!(args[cp + 1], "model_reasoning_effort=medium");
        let op = args.iter().position(|a| a == "-o").unwrap();
        assert_eq!(args[op + 1], "/tmp/sess.last.txt");
        // `-` is last: codex reads the prompt from stdin.
        assert_eq!(args.last().unwrap(), "-");
    }

    /// The prompt rides on stdin, never in argv — Linux caps one argv element
    /// at `MAX_ARG_STRLEN` (131072 bytes). That a payload past the cap arrives
    /// intact over stdin is proven at the runner level by
    /// `agent_stream::tests::delivers_a_stdin_payload_larger_than_max_arg_strlen_intact`.
    #[test]
    fn invocation_delivers_prompt_on_stdin_not_in_argv() {
        const MAX_ARG_STRLEN: usize = 131_072;
        let prompt = "p".repeat(MAX_ARG_STRLEN + 1);
        let request = AgentRequest {
            prompt: prompt.clone(),
            project: "demo".to_string(),
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

        let inv = CodexAdapter.build_invocation(
            &request,
            "gpt-5.4",
            "medium",
            "sess",
            Path::new("/tmp/codex-sessions"),
        );

        assert_eq!(inv.args.last().map(String::as_str), Some("-"));
        assert!(
            inv.args.iter().all(|a| a.len() < MAX_ARG_STRLEN && !a.contains(&prompt)),
            "prompt must not ride in argv"
        );
        assert_eq!(inv.stdin.as_deref(), Some(prompt.as_bytes()));
    }

    #[test]
    fn argv_full_access_bypasses_sandbox() {
        let out = Path::new("/tmp/s.txt");
        let args = build_codex_argv("gpt-5.4", "medium", AgentAccess::Full, out);
        assert!(args.iter().any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
        assert!(!args.iter().any(|a| a == "read-only"));
    }

    #[test]
    fn argv_readonly_access_uses_read_only_sandbox() {
        let out = Path::new("/tmp/s.txt");
        let args = build_codex_argv("gpt-5.5", "high", AgentAccess::ReadOnly, out);
        let sp = args.iter().position(|a| a == "-s").unwrap();
        assert_eq!(args[sp + 1], "read-only");
        assert!(!args.iter().any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn default_tier_and_effort_maps_match_expected() {
        let pm = ProviderModels::default_for(AgentProvider::Codex);
        assert_eq!(pm.model(ModelTier::Deep, AgentProvider::Codex), "gpt-5.5");
        assert_eq!(pm.model(ModelTier::Balanced, AgentProvider::Codex), "gpt-5.4");
        assert_eq!(pm.model(ModelTier::Fast, AgentProvider::Codex), "gpt-5.4-mini");
        assert_eq!(pm.effort_token(ReasoningEffort::High, AgentProvider::Codex), "high");
        assert_eq!(pm.effort_token(ReasoningEffort::Medium, AgentProvider::Codex), "medium");
        // codex has no `max`; it clamps to `high`.
        assert_eq!(pm.effort_token(ReasoningEffort::Max, AgentProvider::Codex), "high");
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

    // --- extract_failure_message: the real diagnostic behind a failed run ---

    /// The exact transcript codex wrote for a real foundry incident
    /// (2026-09-30, project `parite`, campaign cycle 1): a full-access run
    /// requested an invalid model id and failed in ~2.5s. Before this fix,
    /// `Interpreted.failure` was always `None` for codex, so the operator-facing
    /// summary fell back to stderr's first line — which is always the harmless
    /// "Reading additional input from stdin..." probe codex prints regardless of
    /// outcome, never the real cause.
    #[test]
    fn extract_failure_message_returns_the_turn_failed_diagnostic() {
        let lines = vec![
            line(r#"{"type":"thread.started","thread_id":"01a0f2e0-2e35-7a43-96e6-ee6ffa7a79dd"}"#),
            line(
                r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Model metadata for `gpt-6.1-sol` not found. Defaulting to fallback metadata; this can degrade performance and cause issues."}}"#,
            ),
            line(r#"{"type":"turn.started"}"#),
            line(
                r#"{"type":"error","message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account.\"}}"}"#,
            ),
            line(
                r#"{"type":"turn.failed","error":{"message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account.\"}}"}}"#,
            ),
        ];
        assert_eq!(
            extract_failure_message(&lines),
            Some(
                "The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account."
                    .to_string()
            )
        );
    }

    #[test]
    fn extract_failure_message_falls_back_to_top_level_error_without_turn_failed() {
        let lines = vec![line(r#"{"type":"error","message":"provider exploded"}"#)];
        assert_eq!(extract_failure_message(&lines), Some("provider exploded".to_string()));
    }

    #[test]
    fn extract_failure_message_none_when_no_error_event_present() {
        let lines = vec![line(r#"{"type":"turn.completed","usage":{}}"#)];
        assert_eq!(extract_failure_message(&lines), None);
    }

    #[test]
    fn extract_failure_message_leaves_non_json_text_unwrapped() {
        let lines = vec![line(r#"{"type":"error","message":"plain text, not JSON"}"#)];
        assert_eq!(extract_failure_message(&lines), Some("plain text, not JSON".to_string()));
    }

    #[tokio::test]
    async fn invoke_populates_failure_message_from_turn_failed_on_a_real_failure_transcript() {
        let transcript = vec![
            r#"{"type":"thread.started"}"#.to_string(),
            r#"{"type":"turn.started"}"#.to_string(),
            r#"{"type":"error","message":"{\"error\":{\"message\":\"The 'x' model is not supported when using Codex with a ChatGPT account.\"}}"}"#.to_string(),
            r#"{"type":"turn.failed","error":{"message":"{\"error\":{\"message\":\"The 'x' model is not supported when using Codex with a ChatGPT account.\"}}"}}"#.to_string(),
        ];
        let mut outcome = ok_outcome();
        outcome.success = false;
        outcome.exit_code = 1;
        let runner = Arc::new(FakeRunner {
            transcript,
            last_message: None,
            outcome,
        });
        let shell = crate::gateway::fakes::FakeShellGateway::success();
        let (tx, _rx) = broadcast::channel(16);
        let gateway =
            CodexAgentGateway::new_with_streaming(shell, runner, tmp_dir("foundry-codex-test"), tx);

        let request = AgentRequest {
            prompt: "x".to_string(),
            project: "demo".to_string(),
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
        let failure = response.failure.expect("codex must report the real diagnostic on failure");
        assert_eq!(
            failure.message.as_deref(),
            Some("The 'x' model is not supported when using Codex with a ChatGPT account.")
        );
    }

    // --- Full invoke() flow (offline: fake stream runner) -------------------

    use std::time::Duration;

    use super::super::test_support::{FakeRunner, ok_outcome, tmp_dir};

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
        let gateway =
            CodexAgentGateway::new_with_streaming(shell, runner, tmp_dir("foundry-codex-test"), tx);

        let request = AgentRequest {
            prompt: "say something".to_string(),
            project: "demo".to_string(),
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
        assert!(response.success);
        assert_eq!(response.exit_code, 0);
        // Output file wins over stream text.
        assert_eq!(response.stdout, "OUTPUT FILE ANSWER");

        let started = rx.recv().await.expect("started");
        assert_eq!(started.event_type, EventType::AgentSessionStarted);
        assert_eq!(started.payload["agent_type"], "codex");
        assert_eq!(started.payload["tier"], "balanced");
        assert_eq!(started.payload["effort"], "medium");

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
        let gateway =
            CodexAgentGateway::new_with_streaming(shell, runner, tmp_dir("foundry-codex-test"), tx);

        let request = AgentRequest {
            prompt: "x".to_string(),
            project: "demo".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Deep,
            effort: ReasoningEffort::High,
            agent_file: None,
            provider: None,
            env: Vec::new(),
            timeout: Duration::from_secs(5),
            trace_id: None,
            requires_json: false,
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
        let gateway =
            CodexAgentGateway::new_with_streaming(shell, runner, tmp_dir("foundry-codex-test"), tx);

        let request = AgentRequest {
            prompt: "x".to_string(),
            project: "demo".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::Full,
            tier: ModelTier::Fast,
            effort: ReasoningEffort::Low,
            agent_file: None,
            provider: None,
            env: Vec::new(),
            timeout: Duration::from_secs(5),
            trace_id: None,
            requires_json: false,
        };

        let response = gateway.invoke(&request).await.expect("invoke ok");
        assert!(!response.success, "error event should mark the run failed");
        assert_eq!(response.exit_code, 1);

        let _started = rx.recv().await.unwrap();
        let ended = rx.recv().await.unwrap();
        assert_eq!(ended.payload["status"], "agent_failed");
    }
    #[test]
    fn campaign_invocation_overrides_unrestricted_defaults_and_limits_tool_history() {
        let request = AgentRequest {
            prompt: "work".into(),
            project: "p".into(),
            working_dir: PathBuf::from("/tmp/worktree"),
            access: AgentAccess::Full,
            tier: ModelTier::Balanced,
            effort: ReasoningEffort::Medium,
            agent_file: None,
            provider: Some(AgentProvider::Codex),
            env: vec![("FOUNDRY_WRITABLE_ROOT".into(), "/tmp/worktree".into())],
            timeout: std::time::Duration::from_secs(30),
            trace_id: None,
            requires_json: false,
        };
        let invocation = CodexAdapter.build_invocation(
            &request,
            "model",
            "medium",
            "session",
            Path::new("/tmp/logs"),
        );
        let args = &invocation.args;
        assert!(args.windows(2).any(|p| p == ["-s", "workspace-write"]));
        assert!(args.contains(&"tool_output_token_limit=2000".into()));
        assert!(!args.contains(&"--dangerously-bypass-approvals-and-sandbox".into()));
        let roots = args
            .iter()
            .find(|a| a.starts_with("sandbox_workspace_write.writable_roots="))
            .unwrap();
        assert!(roots.contains("tool-logs"));
        assert!(!roots.contains("Work/Projects"));
        assert_eq!(invocation.stdin.as_deref(), Some(b"work".as_slice()));
    }
}
