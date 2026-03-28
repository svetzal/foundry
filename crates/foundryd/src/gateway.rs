use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use foundry_core::registry::Stack;

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

/// Abstracts over AI agent runtimes so that blocks can be tested without
/// invoking real agent processes.
pub trait AgentGateway: Send + Sync {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>>;
}

/// Production implementation that invokes the Claude CLI via `ShellGateway`.
pub struct ClaudeAgentGateway {
    shell: Arc<dyn ShellGateway>,
}

impl ClaudeAgentGateway {
    pub fn new(shell: Arc<dyn ShellGateway>) -> Self {
        Self { shell }
    }

    fn model_for(capability: AgentCapability) -> &'static str {
        match capability {
            AgentCapability::Reasoning => "claude-opus-4-6",
            AgentCapability::Coding => "claude-sonnet-4-6",
            AgentCapability::Quick => "claude-haiku-4-5-20251001",
        }
    }
}

impl AgentGateway for ClaudeAgentGateway {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            let model = Self::model_for(request.capability);

            let mut args: Vec<String> = vec![
                "--print".to_string(),
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

            // CLAUDECODE="" prevents nested-session detection.
            let env = vec![("CLAUDECODE".to_string(), String::new())];

            let result = self
                .shell
                .run(&request.working_dir, "claude", &arg_refs, Some(&env), Some(request.timeout))
                .await?;

            Ok(AgentResponse {
                stdout: result.stdout,
                stderr: result.stderr,
                exit_code: result.exit_code,
                success: result.success,
            })
        })
    }
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
