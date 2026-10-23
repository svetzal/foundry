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

use crate::agent_stream::{AgentStreamOutcome, AgentStreamRunner, StreamRun};

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
    ) -> Result<Invocation>;
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
    /// Bytes fed to the CLI's stdin, which is then closed. `Some` for providers
    /// that take their prompt on stdin — the only way past Linux's 131072-byte
    /// cap on a single argv element. `None` leaves stdin closed.
    pub(crate) stdin: Option<Vec<u8>>,
    /// Codex writes its authoritative final answer to an `-o` file; stored here
    /// so `interpret` can read and clean it up. `None` for other providers.
    pub(crate) last_message_path: Option<PathBuf>,
    /// Owned settings and scratch files. Kept alive through JSON recovery and
    /// removed when the invocation ends, including cancellation and errors.
    pub(crate) _run_files: Option<RunFiles>,
}

/// Private files for one CLI invocation, cleaned up when its future is dropped.
pub(crate) struct RunFiles {
    pub(crate) directory: PathBuf,
}

impl RunFiles {
    pub(crate) fn create(directory: PathBuf) -> Result<Self> {
        if let Some(parent) = directory.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Never reuse a previous invocation's directory or follow its symlink.
        std::fs::create_dir(&directory)?;
        Ok(Self { directory })
    }
}

impl Drop for RunFiles {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory) {
            // Best-effort: private scratch cleanup cannot change a completed
            // session's verdict, but leftover files must remain visible.
            tracing::warn!(%error, path = %self.directory.display(), "agent scratch cleanup failed");
        }
    }
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

const AUDIT_POLICY: &str = "Supply-chain audit constraints (unless the owner explicitly authorizes an exception):
Check resolved direct and transitive dependencies for known advisories using maintained tools native to the project's ecosystem. Do not add other-language runtimes, commercial services, or external accounts to satisfy an audit.
State coverage, findings, and unavailable checks. No matching known advisories within stated coverage is not proof that the supply chain is safe.
An unavailable check remains an explicit limitation or blocked audit, never a successful audit. Do not silently pass, lower vulnerability thresholds, suppress real findings, or remove required ecosystem coverage.
Optional analyzers for unrelated ecosystems may be disabled with an explicit coverage explanation; this is not automatically an audit bypass. Review against this bounded scope rather than requiring every scanner default.
Do not build bespoke scanners, feed mirrors, caches, downloaders, or verification infrastructure solely to satisfy an unavailable check without explicit owner authorization to expand tooling scope. Ordinary configuration and existing tool caching remain allowed.
If verification cannot meet these constraints, report the limitation and stop infrastructure expansion.";

const EVIDENCE_POLICY: &str = "Diligence and bounded evidence for tasks, campaigns, and initiatives:
Formation, derivation, steering, execution, review, and completion must use observable acceptance evidence proportionate to the authorized outcome. Do not invent universal guarantees, exhaustive absence-of-defects claims, or extra acceptance criteria from generic quality aspirations such as clean, safe, complete, or production-ready.
Use the existing project gates and owner criteria. Do not expand the authorized scope, add infrastructure or services, or create further campaigns merely to chase certainty. A passing check supports only its stated coverage and checked revision; report residual risks and unavailable checks explicitly.
Preserve explicit owner acceptance criteria, required gates, and required owner confirmation. Missing evidence is not success, and limitations do not waive a criterion. If an explicit criterion demands an impossible guarantee or evidence outside authorized scope, ask or escalate with the exact criterion, available evidence, limitation, and smallest bounded decision the owner can make. Do not silently reinterpret it or repeat cycles pursuing certainty.
Once the authorized outcome and required evidence are satisfied, propose completion with the remaining limitations; do not invent more work for hypothetical perfection.";

fn bounded_audit_prompt(prompt: &str) -> String {
    format!("{prompt}\n\n{AUDIT_POLICY}\n\n{EVIDENCE_POLICY}")
}

impl<A: CliAgentAdapter + Send + Sync + 'static> AgentGateway for CliAgentGateway<A> {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            let mut bounded_request = request.clone();
            bounded_request.prompt = bounded_audit_prompt(&request.prompt);
            let request = &bounded_request;
            if request.env.iter().any(|(k, _)| k == "FOUNDRY_WRITABLE_ROOT")
                && !self.adapter.provider().enforces_writable_root()
            {
                anyhow::bail!("this provider cannot enforce the declared writable repository");
            }
            let session_id = Uuid::new_v4().to_string();
            let log_path = self.session_log_dir.join(format!("{session_id}.jsonl"));

            let provider = self.adapter.provider();
            let model = self.models.model(request.tier, provider);
            let effective_effort = self.models.effective_effort(request.tier, request.effort);
            let effort = self.models.effort_token(effective_effort, provider);

            let inv = with_request_environment(
                self.adapter.build_invocation(
                    request,
                    &model,
                    &effort,
                    &session_id,
                    &self.session_log_dir,
                )?,
                request,
            );
            let arg_refs: Vec<&str> = inv.args.iter().map(String::as_str).collect();
            let env_opt = (!inv.env.is_empty()).then_some(inv.env.as_slice());

            emit_session_started(
                &self.event_tx,
                request,
                effective_effort,
                &session_id,
                self.adapter.agent_type(),
                provider,
                &log_path,
            )?;

            let outcome = self
                .stream_runner
                .run(StreamRun {
                    working_dir: &request.working_dir,
                    command: self.adapter.command(),
                    args: &arg_refs,
                    env: env_opt,
                    stdin: inv.stdin.as_deref(),
                    timeout: (!request.timeout.is_zero()).then_some(request.timeout),
                    log_path: &log_path,
                })
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
        ) -> Result<Invocation> {
            Ok(Invocation {
                args: {
                    assert!(request.prompt.contains("tools native to the project's ecosystem"));
                    assert!(request.prompt.contains("Do not add other-language runtimes, commercial services, or external accounts"));
                    assert!(request.prompt.contains("never a successful audit"));
                    assert!(request.prompt.contains(
                        "Do not build bespoke scanners, feed mirrors, caches, downloaders"
                    ));
                    assert!(request.prompt.contains(
                        "Ordinary configuration and existing tool caching remain allowed"
                    ));
                    assert!(request.prompt.contains(
                        "Formation, derivation, steering, execution, review, and completion"
                    ));
                    assert!(request.prompt.contains("limitations do not waive a criterion"));
                    assert!(
                        request
                            .prompt
                            .contains("do not invent more work for hypothetical perfection")
                    );
                    assert!(
                        request.prompt.contains(
                            "Optional analyzers for unrelated ecosystems may be disabled"
                        )
                    );
                    assert!(request.prompt.contains("lower vulnerability thresholds"));
                    assert!(request.prompt.contains("not proof that the supply chain is safe"));
                    vec![request.prompt.clone()]
                },
                env: vec![],
                stdin: None,
                last_message_path: None,
                _run_files: None,
            })
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
            requires_json: false,
        };

        let response = gateway.invoke(&request).await.expect("invoke ok");
        assert!(response.success);
        assert_eq!(response.exit_code, 0);
        assert_eq!(response.stdout, "hello from echo");

        let started = rx.recv().await.expect("started event");
        assert_eq!(started.event_type, EventType::AgentSessionStarted);
        assert_eq!(started.payload["agent_type"], "echo-test");
        assert_eq!(started.payload["project"], "test-project");
        assert_eq!(started.payload["effective_effort"], "medium");

        let ended = rx.recv().await.expect("ended event");
        assert_eq!(ended.event_type, EventType::AgentSessionEnded);
        assert_eq!(ended.payload["status"], "ok");
    }

    #[tokio::test]
    async fn zero_timeout_waits_for_agent_completion() {
        struct UnlimitedRunner;
        impl crate::agent_stream::AgentStreamRunner for UnlimitedRunner {
            fn run<'a>(
                &'a self,
                run: StreamRun<'a>,
            ) -> Pin<
                Box<
                    dyn Future<Output = anyhow::Result<crate::agent_stream::AgentStreamOutcome>>
                        + Send
                        + 'a,
                >,
            > {
                assert!(run.timeout.is_none(), "unlimited agents must bypass clock deadlines");
                Box::pin(async { Ok(ok_outcome()) })
            }
        }
        let (tx, _rx) = broadcast::channel(16);
        let gateway = CliAgentGateway::new_with_adapter(
            FakeShellGateway::success(),
            Arc::new(UnlimitedRunner),
            tmp_dir("unlimited-agent"),
            tx,
            EchoAdapter,
        );
        let request = AgentRequest {
            prompt: "hello".into(),
            project: "test".into(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Balanced,
            effort: ReasoningEffort::Medium,
            agent_file: None,
            provider: None,
            env: vec![],
            timeout: Duration::ZERO,
            trace_id: None,
            requires_json: false,
        };
        assert!(gateway.invoke(&request).await.unwrap().success);
    }

    /// Delegates to [`EchoAdapter`] but records the effort token it was handed.
    struct EffortCapturingAdapter(Arc<std::sync::Mutex<Option<String>>>);

    impl CliAgentAdapter for EffortCapturingAdapter {
        fn provider(&self) -> AgentProvider {
            EchoAdapter.provider()
        }

        fn agent_type(&self) -> &'static str {
            EchoAdapter.agent_type()
        }

        fn command(&self) -> &'static str {
            EchoAdapter.command()
        }

        fn build_invocation(
            &self,
            request: &AgentRequest,
            model: &str,
            effort: &str,
            session_id: &str,
            session_log_dir: &Path,
        ) -> Result<Invocation> {
            *self.0.lock().expect("capture lock") = Some(effort.to_string());
            EchoAdapter.build_invocation(request, model, effort, session_id, session_log_dir)
        }

        fn interpret<'a>(
            &'a self,
            outcome: &'a AgentStreamOutcome,
            session: SessionContext<'a>,
            inv: &'a Invocation,
            request: &'a AgentRequest,
            shell: &'a Arc<dyn ShellGateway>,
        ) -> Pin<Box<dyn Future<Output = Interpreted> + Send + 'a>> {
            EchoAdapter.interpret(outcome, session, inv, request, shell)
        }
    }

    #[tokio::test]
    async fn effort_cap_lowers_the_cli_token_and_is_recorded_in_session_started() {
        let (tx, mut rx) = broadcast::channel(16);
        let runner = Arc::new(FakeRunner {
            transcript: vec!["ok".to_string()],
            last_message: None,
            outcome: ok_outcome(),
        });
        let captured = Arc::new(std::sync::Mutex::new(None));
        let mut models = ProviderModels::default_for(AgentProvider::Claude);
        models.effort_caps.insert(ModelTier::Balanced, ReasoningEffort::Medium);
        let gateway = CliAgentGateway::new_with_adapter(
            FakeShellGateway::success(),
            runner,
            tmp_dir("engine-cap-test"),
            tx,
            EffortCapturingAdapter(Arc::clone(&captured)),
        )
        .with_models(models);

        let request = AgentRequest {
            prompt: "hello".to_string(),
            project: "test-project".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::Full,
            tier: ModelTier::Balanced,
            effort: ReasoningEffort::High,
            agent_file: None,
            provider: None,
            env: Vec::new(),
            timeout: Duration::from_secs(5),
            trace_id: None,
            requires_json: false,
        };

        gateway.invoke(&request).await.expect("invoke ok");

        assert_eq!(captured.lock().expect("capture lock").as_deref(), Some("medium"));
        let started = rx.recv().await.expect("started event");
        assert_eq!(started.event_type, EventType::AgentSessionStarted);
        assert_eq!(started.payload["effort"], "high");
        assert_eq!(started.payload["effective_effort"], "medium");
    }

    #[tokio::test]
    async fn shared_invoke_produces_unavailable_when_runner_errors() {
        use crate::agent_stream::AgentStreamRunner;

        struct FailRunner;
        impl AgentStreamRunner for FailRunner {
            fn run<'a>(
                &'a self,
                _: crate::agent_stream::StreamRun<'a>,
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
            requires_json: false,
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
