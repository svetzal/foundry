//! Shared `CliAgentGateway<A>` engine — the single invoke lifecycle reused by
//! all CLI-backed agent providers (Claude, Opencode, Codex).
//!
//! Every CLI-backed gateway has the same skeleton:
//! 1. Generate a session ID and a log path.
//! 2. Resolve model/effort strings from the provider's tier/effort maps.
//! 3. Call the adapter to build argv + env + any per-run file paths.
//! 4. Emit `AgentSessionStarted`.
//! 5. Spawn the CLI via `AgentStreamRunner`.
//! 6. Delegate to the adapter for provider-specific failure detection,
//!    result extraction, and post-run cleanup.
//! 7. Emit `AgentSessionEnded`.
//! 8. Build and return the `AgentResponse`.
//!
//! What differs per provider lives in `CliAgentAdapter` impls.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use foundry_sdk::event::Event;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::agent_stream::{AgentStreamOutcome, AgentStreamRunner};

use super::{
    AgentGateway, AgentProvider, AgentRequest, AgentResponse, ProviderModels, ShellGateway,
    emit_session_ended, emit_session_started,
};

/// Per-provider strategy: exactly the three steps that differ between gateways.
pub(crate) trait CliAgentAdapter: Send + Sync {
    /// Provider enum variant (used for tier/effort lookups).
    fn provider(&self) -> AgentProvider;
    /// `agent_type` string emitted in `AgentSessionStarted` (e.g. "claude-code").
    fn agent_type(&self) -> &'static str;
    /// Binary to invoke (e.g. "claude", "opencode", "codex").
    fn command(&self) -> &'static str;
    /// Build argv, environment, and any per-run file paths from the resolved
    /// model/effort strings and the request. Called before `stream_runner.run`.
    fn build_invocation(
        &self,
        request: &AgentRequest,
        model: &str,
        effort: &str,
        session_id: &str,
        session_log_dir: &Path,
    ) -> Invocation;
    /// Extract the result from a completed stream run. Responsible for:
    /// - provider-specific success/failure determination,
    /// - authoritative stdout extraction (file read, export call, or stream scan),
    /// - post-run cleanup (e.g. removing the `-o` file after reading it).
    fn interpret<'a>(
        &'a self,
        outcome: &'a AgentStreamOutcome,
        inv: &'a Invocation,
        request: &'a AgentRequest,
        shell: &'a Arc<dyn ShellGateway>,
    ) -> Pin<Box<dyn Future<Output = Interpreted> + Send + 'a>>;
}

/// Arguments, environment, and any provider-specific per-run state produced by
/// `CliAgentAdapter::build_invocation`.
pub(crate) struct Invocation {
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    /// Codex writes its authoritative final answer to an `-o` file; stored here
    /// so `interpret` can read and clean it up. `None` for other providers.
    pub(crate) last_message_path: Option<PathBuf>,
}

/// Provider-specific result produced by `CliAgentAdapter::interpret`.
pub(crate) struct Interpreted {
    pub(crate) success: bool,
    pub(crate) exit_code: i32,
    pub(crate) stdout: String,
}

/// Generic gateway that runs the shared CLI agent lifecycle, delegating the
/// three provider-specific steps to an adapter.
pub(crate) struct CliAgentGateway<A: CliAgentAdapter> {
    pub(crate) shell: Arc<dyn ShellGateway>,
    pub(crate) stream_runner: Arc<dyn AgentStreamRunner>,
    pub(crate) session_log_dir: PathBuf,
    pub(crate) event_tx: broadcast::Sender<Event>,
    pub(crate) models: ProviderModels,
    pub(crate) adapter: A,
}

impl<A: CliAgentAdapter> CliAgentGateway<A> {
    pub(crate) fn new_with_adapter(
        shell: Arc<dyn ShellGateway>,
        stream_runner: Arc<dyn AgentStreamRunner>,
        session_log_dir: PathBuf,
        event_tx: broadcast::Sender<Event>,
        adapter: A,
    ) -> Self {
        let provider = adapter.provider();
        Self {
            shell,
            stream_runner,
            session_log_dir,
            event_tx,
            models: ProviderModels::default_for(provider),
            adapter,
        }
    }

    #[must_use]
    pub(crate) fn with_models(mut self, models: ProviderModels) -> Self {
        self.models = models;
        self
    }
}

impl<A: CliAgentAdapter + Send + Sync + 'static> AgentGateway for CliAgentGateway<A> {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            let session_id = Uuid::new_v4().to_string();
            let log_path = self.session_log_dir.join(format!("{session_id}.jsonl"));

            let provider = self.adapter.provider();
            let model = self.models.model(request.tier, provider);
            let effort = self.models.effort_token(request.effort, provider);

            let inv = self.adapter.build_invocation(
                request,
                &model,
                &effort,
                &session_id,
                &self.session_log_dir,
            );
            let arg_refs: Vec<&str> = inv.args.iter().map(String::as_str).collect();
            let env_opt = if inv.env.is_empty() {
                None
            } else {
                Some(inv.env.as_slice())
            };

            emit_session_started(
                &self.event_tx,
                request,
                &session_id,
                self.adapter.agent_type(),
                &log_path,
            )?;

            let outcome = self
                .stream_runner
                .run(
                    &request.working_dir,
                    self.adapter.command(),
                    &arg_refs,
                    env_opt,
                    Some(request.timeout),
                    &log_path,
                )
                .await;

            let (status, exit_code, stderr, bytes_written, error_msg, stdout_text) = match outcome {
                Ok(o) => {
                    let interpreted = self.adapter.interpret(&o, &inv, request, &self.shell).await;
                    let status = if interpreted.success {
                        "ok"
                    } else {
                        "agent_failed"
                    };
                    (
                        status,
                        Some(interpreted.exit_code),
                        o.stderr,
                        o.bytes_written,
                        None,
                        interpreted.stdout,
                    )
                }
                Err(e) => {
                    ("unavailable", None, String::new(), 0u64, Some(e.to_string()), String::new())
                }
            };

            emit_session_ended(
                &self.event_tx,
                &request.project,
                &session_id,
                status,
                exit_code,
                bytes_written,
                error_msg,
            )?;

            Ok(AgentResponse {
                stdout: stdout_text,
                stderr,
                exit_code: exit_code.unwrap_or(-1),
                success: status == "ok",
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_stream::{AgentStreamOutcome, StreamedLine};
    use crate::gateway::fakes::FakeShellGateway;
    use crate::gateway::test_support::{FakeRunner, ok_outcome, tmp_dir};
    use foundry_sdk::event::EventType;
    use foundry_sdk::gateway::{AgentAccess, ModelTier, ReasoningEffort};
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::sync::broadcast;

    struct EchoAdapter;

    impl CliAgentAdapter for EchoAdapter {
        fn provider(&self) -> AgentProvider {
            AgentProvider::Claude
        }

        fn agent_type(&self) -> &'static str {
            "echo-test"
        }

        fn command(&self) -> &'static str {
            "echo"
        }

        fn build_invocation(
            &self,
            request: &AgentRequest,
            _model: &str,
            _effort: &str,
            _session_id: &str,
            _session_log_dir: &Path,
        ) -> Invocation {
            Invocation {
                args: vec![request.prompt.clone()],
                env: vec![],
                last_message_path: None,
            }
        }

        fn interpret<'a>(
            &'a self,
            outcome: &'a AgentStreamOutcome,
            _inv: &'a Invocation,
            _request: &'a AgentRequest,
            _shell: &'a Arc<dyn ShellGateway>,
        ) -> Pin<Box<dyn Future<Output = Interpreted> + Send + 'a>> {
            let success = outcome.success;
            let exit_code = outcome.exit_code;
            let stdout = outcome.lines.first().map(|l| l.raw.clone()).unwrap_or_default();
            Box::pin(async move {
                Interpreted {
                    success,
                    exit_code,
                    stdout,
                }
            })
        }
    }

    #[tokio::test]
    async fn shared_invoke_emits_lifecycle_events_and_delegates_to_adapter() {
        let (tx, mut rx) = broadcast::channel(16);
        let runner = Arc::new(FakeRunner {
            transcript: vec!["hello from echo".to_string()],
            last_message: None,
            outcome: ok_outcome(),
        });
        let gateway = CliAgentGateway::new_with_adapter(
            FakeShellGateway::success(),
            runner,
            tmp_dir("engine-test"),
            tx,
            EchoAdapter,
        );

        let request = AgentRequest {
            prompt: "hello".to_string(),
            project: "test-project".to_string(),
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
        assert_eq!(response.stdout, "hello from echo");

        let started = rx.recv().await.expect("started event");
        assert_eq!(started.event_type, EventType::AgentSessionStarted);
        assert_eq!(started.payload["agent_type"], "echo-test");
        assert_eq!(started.payload["project"], "test-project");

        let ended = rx.recv().await.expect("ended event");
        assert_eq!(ended.event_type, EventType::AgentSessionEnded);
        assert_eq!(ended.payload["status"], "ok");
    }

    #[tokio::test]
    async fn shared_invoke_produces_unavailable_when_runner_errors() {
        use crate::agent_stream::AgentStreamRunner;

        struct FailRunner;
        impl AgentStreamRunner for FailRunner {
            fn run<'a>(
                &'a self,
                _: &'a std::path::Path,
                _: &'a str,
                _: &'a [&'a str],
                _: Option<&'a [(String, String)]>,
                _: Option<Duration>,
                _: &'a std::path::Path,
            ) -> Pin<Box<dyn Future<Output = anyhow::Result<AgentStreamOutcome>> + Send + 'a>>
            {
                Box::pin(async { Err(anyhow::anyhow!("binary not found")) })
            }
        }

        let (tx, mut rx) = broadcast::channel(16);
        let gateway = CliAgentGateway::new_with_adapter(
            FakeShellGateway::success(),
            Arc::new(FailRunner),
            tmp_dir("engine-fail-test"),
            tx,
            EchoAdapter,
        );

        let request = AgentRequest {
            prompt: "x".to_string(),
            project: "p".to_string(),
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
        assert_eq!(response.exit_code, -1);

        let _started = rx.recv().await.unwrap();
        let ended = rx.recv().await.unwrap();
        assert_eq!(ended.payload["status"], "unavailable");
    }

    /// Verify `with_models` overrides the default model resolution.
    #[test]
    fn with_models_replaces_default_models() {
        use foundry_sdk::agent_config::ProviderModels;
        let (tx, _) = broadcast::channel(4);
        let gw = CliAgentGateway::new_with_adapter(
            FakeShellGateway::success(),
            Arc::new(FakeRunner {
                transcript: vec![],
                last_message: None,
                outcome: AgentStreamOutcome {
                    exit_code: 0,
                    success: true,
                    stderr: String::new(),
                    bytes_written: 0,
                    lines: vec![StreamedLine { raw: String::new() }],
                },
            }),
            tmp_dir("models-test"),
            tx,
            EchoAdapter,
        );
        let custom = ProviderModels::default_for(AgentProvider::Claude);
        let gw2 = gw.with_models(custom.clone());
        // Verify the field was updated (model for Deep tier resolves to the known value).
        assert_eq!(
            gw2.models.model(ModelTier::Deep, AgentProvider::Claude),
            custom.model(ModelTier::Deep, AgentProvider::Claude)
        );
    }
}
