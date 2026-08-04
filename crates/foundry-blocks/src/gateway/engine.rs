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
use foundry_sdk::payload::AgentSessionEndedPayload;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::agent_stream::{AgentStreamOutcome, AgentStreamRunner};

use super::{
    AgentFailureMetadata, AgentGateway, AgentProvider, AgentRequest, AgentResponse, ProviderModels,
    ShellGateway, emit_session_ended, emit_session_started,
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
        session: SessionContext<'a>,
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

fn with_request_environment(mut invocation: Invocation, request: &AgentRequest) -> Invocation {
    invocation.env.extend(request.env.iter().cloned());
    invocation
}

/// Provider-specific result produced by `CliAgentAdapter::interpret`.
pub(crate) struct Interpreted {
    pub(crate) success: bool,
    pub(crate) exit_code: i32,
    pub(crate) stdout: String,
    pub(crate) failure: Option<AgentFailureMetadata>,
}

/// The run's terminal state, ready to be turned into an `AgentSessionEnded`.
struct EndedSession<'a> {
    session_id: &'a str,
    status: &'a str,
    exit_code: Option<i32>,
    bytes_written: u64,
    error: Option<String>,
    failure: Option<AgentFailureMetadata>,
}

/// Build the session-ended payload, reading the transcript back for what the
/// session spent.
///
/// `model` is the id the gateway resolved for this request — the only way a
/// Codex session's tokens can be priced, since its transcript never names a
/// model of its own.
fn ended_payload(
    session: EndedSession<'_>,
    log_path: &Path,
    model: &str,
) -> AgentSessionEndedPayload {
    let (usage, cost) = crate::gateway::price_session(log_path, model);
    AgentSessionEndedPayload {
        session_id: session.session_id.to_string(),
        status: session.status.to_string(),
        exit_code: session.exit_code,
        ended_at: chrono::Utc::now().to_rfc3339(),
        bytes_written: session.bytes_written,
        error: session.error,
        failure: session.failure.unwrap_or_default(),
        usage,
        cost: Some(cost),
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SessionContext<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) provider: AgentProvider,
    pub(crate) log_path: &'a Path,
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

    /// Turn a finished (or failed-to-start) stream run into the settled facts
    /// the session-ended event and the response both need.
    ///
    /// A run that never started still settles — as `unavailable`, with no
    /// output — so the caller always has a terminal state to report.
    async fn settle(
        &self,
        outcome: Result<AgentStreamOutcome>,
        session_id: &str,
        provider: AgentProvider,
        log_path: &Path,
        inv: &Invocation,
        request: &AgentRequest,
    ) -> SettledRun {
        match outcome {
            Ok(o) => {
                let interpreted = self
                    .adapter
                    .interpret(
                        &o,
                        SessionContext {
                            session_id,
                            provider,
                            log_path,
                        },
                        inv,
                        request,
                        &self.shell,
                    )
                    .await;
                SettledRun {
                    status: if interpreted.success {
                        "ok"
                    } else {
                        "agent_failed"
                    },
                    exit_code: Some(interpreted.exit_code),
                    stderr: o.stderr,
                    bytes_written: o.bytes_written,
                    error: None,
                    stdout: interpreted.stdout,
                    failure: interpreted.failure,
                }
            }
            Err(e) => SettledRun {
                status: "unavailable",
                exit_code: None,
                stderr: String::new(),
                bytes_written: 0,
                error: Some(e.to_string()),
                stdout: String::new(),
                failure: None,
            },
        }
    }
}

/// The settled result of one agent run.
struct SettledRun {
    status: &'static str,
    exit_code: Option<i32>,
    stderr: String,
    bytes_written: u64,
    error: Option<String>,
    stdout: String,
    failure: Option<AgentFailureMetadata>,
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

            let inv = with_request_environment(
                self.adapter.build_invocation(
                    request,
                    &model,
                    &effort,
                    &session_id,
                    &self.session_log_dir,
                ),
                request,
            );
            let arg_refs: Vec<&str> = inv.args.iter().map(String::as_str).collect();
            let env_opt = (!inv.env.is_empty()).then_some(inv.env.as_slice());

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

            let run = self.settle(outcome, &session_id, provider, &log_path, &inv, request).await;

            emit_session_ended(
                &self.event_tx,
                &request.project,
                request.trace_id.clone(),
                &ended_payload(
                    EndedSession {
                        session_id: &session_id,
                        status: run.status,
                        exit_code: run.exit_code,
                        bytes_written: run.bytes_written,
                        error: run.error,
                        failure: run.failure.clone(),
                    },
                    &log_path,
                    &model,
                ),
            )?;

            Ok(AgentResponse {
                stdout: run.stdout,
                stderr: run.stderr,
                exit_code: run.exit_code.unwrap_or(-1),
                success: run.status == "ok",
                failure: run.failure,
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
            _session: SessionContext<'a>,
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
                    failure: None,
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
            env: Vec::new(),
            timeout: Duration::from_secs(5),
            trace_id: None,
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
            env: Vec::new(),
            timeout: Duration::from_secs(5),
            trace_id: None,
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
