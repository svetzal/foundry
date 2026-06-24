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
    AgentAccess, AgentGateway, AgentProvider, AgentRequest, AgentResponse, ProviderModels,
    ShellGateway,
    engine::{CliAgentAdapter, CliAgentGateway, Interpreted, Invocation},
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

        let args = build_codex_argv(model, effort, request.access, &last_message_path, &prompt);

        Invocation {
            args,
            env: vec![],
            last_message_path: Some(last_message_path),
        }
    }

    fn interpret<'a>(
        &'a self,
        outcome: &'a AgentStreamOutcome,
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
            if let Some(ref p) = inv.last_message_path {
                let _ = tokio::fs::remove_file(p).await;
            }

            Interpreted {
                success,
                exit_code,
                stdout,
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
        Err(_) => None,
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
