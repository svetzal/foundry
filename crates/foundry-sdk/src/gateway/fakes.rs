//! Test-support fakes, gated behind the `test-support` feature.
//!
//! The `Mutex::lock().unwrap()` calls throughout this module are genuinely
//! infallible in this context: these mutexes only ever guard a plain `Vec`
//! append/read with no user code (and therefore no panic point) running
//! while the lock is held, so they can never be poisoned.
#![allow(
    clippy::unwrap_used,
    reason = "test-support fakes: mutex holds only infallible Vec ops, never poisoned"
)]

use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use crate::registry::Stack;

use super::{
    AgentAccess, AgentGateway, AgentProvider, AgentRequest, AgentResponse, AuditResult,
    CommandResult, ModelTier, ReasoningEffort, ScannerGateway, ShellGateway, Vulnerability,
};

/// A recorded shell invocation for use in test assertions.
#[derive(Debug, Clone)]
pub struct ShellInvocation {
    pub command: String,
    pub args: Vec<String>,
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

    /// Always return an `Err` from `run_audit`, simulating a gateway-level
    /// failure (e.g. I/O error or process spawn failure).
    pub fn gateway_error(msg: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            result: Err(msg.into()),
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
pub struct AgentInvocation {
    pub prompt: String,
    pub project: String,
    pub working_dir: String,
    pub access: AgentAccess,
    pub tier: ModelTier,
    pub effort: ReasoningEffort,
    pub agent_file: Option<String>,
    pub provider: Option<AgentProvider>,
    /// Trace the request carried. Recorded so tests can prove a block forwards
    /// its trigger's trace into the session that spends the tokens.
    pub trace_id: Option<String>,
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
        Self::always(AgentResponse::success(String::new()))
    }

    /// Always return a successful result with the given stdout.
    pub fn success_with(stdout: impl Into<String>) -> Arc<Self> {
        Self::always(AgentResponse::success(stdout))
    }

    /// Always return a failure result with the given stderr.
    pub fn failure(stderr: impl Into<String>) -> Arc<Self> {
        Self::always(AgentResponse::failure(stderr))
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
            project: request.project.clone(),
            working_dir: request.working_dir.display().to_string(),
            access: request.access,
            tier: request.tier,
            effort: request.effort,
            trace_id: request.trace_id.clone(),
            agent_file: request.agent_file.as_ref().map(|p| p.display().to_string()),
            provider: request.provider,
        };
        self.invocations.lock().unwrap().push(inv);
        let result = self.next_result();
        Box::pin(async move { Ok(result) })
    }
}
