# Writing Task Blocks

To add a new task block to Foundry:

1. Implement the `TaskBlock` trait
2. Register it with the engine in `main.rs`

## The TaskBlock Trait

```rust
use std::pin::Pin;
use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

pub struct MyBlock;

impl TaskBlock for MyBlock {
    fn name(&self) -> &'static str {
        "My Block"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer  // or BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::GreetRequested]  // which events trigger this block
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        Box::pin(async move {
            // Do your work here...

            Ok(TaskBlockResult {
                events: vec![
                    Event::new(
                        EventType::GreetingComposed,
                        project,
                        throttle,
                        serde_json::json!({"result": "done"}),
                    ),
                ],
                success: true,
                summary: "Did the thing".to_string(),
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
    }
}
```

## Key Points

- **Propagate throttle**: always pass `trigger.throttle` to emitted events
- **Clone what you need**: extract data from `trigger` before the `async move` block
- **Return events**: the engine handles routing them to downstream blocks
- **Observer vs Mutator**: choose based on whether your block has side effects

## TaskBlockResult Fields

| Field | Type | Description |
|-------|------|-------------|
| `events` | `Vec<Event>` | Events to emit downstream (subject to throttle) |
| `success` | `bool` | Whether the block's work succeeded |
| `summary` | `String` | Human-readable one-line summary shown in traces |
| `raw_output` | `Option<String>` | Combined stdout+stderr from any shell command — shown in `foundry trace --verbose` |
| `exit_code` | `Option<i32>` | Exit code from any shell command — useful for observability |
| `audit_artifacts` | `Vec<String>` | Paths to files produced by this block (e.g. audit logs). Listed under `artifacts:` in verbose trace output |

Blocks that do not run external processes should set `raw_output: None`,
`exit_code: None`, and `audit_artifacts: vec![]`. Blocks that shell out should
populate `raw_output` with the combined output and `exit_code` with the process
exit code so that traces provide full observability without needing to reproduce
the command.

## Registering

In `foundryd/src/main.rs`:

```rust
let mut engine = engine::Engine::new();
engine.register(Box::new(blocks::MyBlock));
```

## RetryPolicy

Override `retry_policy()` to enable automatic retry of transient failures.
The default is zero retries (execute exactly once).

```rust
use std::time::Duration;
use foundry_core::task_block::RetryPolicy;

fn retry_policy(&self) -> RetryPolicy {
    RetryPolicy {
        max_retries: 3,
        backoff: Duration::from_secs(5),
    }
}
```

With `max_retries: N`, the engine tries the block up to `N + 1` times total
(1 initial attempt plus up to N retries), sleeping `backoff` between each
attempt. Both `Err` results and `TaskBlockResult { success: false, .. }` trigger
a retry. The final attempt's outcome is what appears in the `BlockExecution`
trace.

Use retries for operations that may fail transiently (network calls, shell
commands that occasionally time out). Do not use retries for operations that
are expected to fail deterministically (e.g. self-filtering by payload).

## Gateway Pattern

Task blocks that execute external processes (shell commands, audit tools) receive
those capabilities through *gateway traits* rather than calling the implementation
directly.  This isolates I/O at the block boundary and makes every block fully
testable without spawning real processes.

### The ShellGateway Trait

```rust
pub trait ShellGateway: Send + Sync {
    fn run<'a>(
        &'a self,
        working_dir: &'a Path,
        command: &'a str,
        args: &'a [&'a str],
        env: Option<&'a [(String, String)]>,
        timeout: Option<Duration>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<CommandResult>> + Send + 'a>>;
}
```

In production, `ProcessShellGateway` delegates to `crate::shell::run`.  Blocks
accept the gateway through their constructor:

```rust
pub struct MyBlock {
    registry: Arc<Registry>,
    shell: Arc<dyn ShellGateway>,
}

impl MyBlock {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            shell: Arc::new(ProcessShellGateway),
        }
    }

    #[cfg(test)]
    fn with_shell(registry: Arc<Registry>, shell: Arc<dyn ShellGateway>) -> Self {
        Self { registry, shell }
    }
}
```

### Testing with Fakes

`gateway::fakes` (available only under `#[cfg(test)]`) provides pre-built fakes:

```rust
use crate::gateway::fakes::{FakeShellGateway, FakeScannerGateway};
use crate::shell::CommandResult;

// Always return a successful, empty result.
let shell = FakeShellGateway::success();

// Always return a failure with the given stderr.
let shell = FakeShellGateway::failure("not installed");

// Return a fixed result every time.
let shell = FakeShellGateway::always(CommandResult { ... });

// Return results in sequence (last one repeats).
let shell = FakeShellGateway::sequence(vec![first_result, second_result]);

// Inspect recorded invocations after the fact.
let invocations = shell.invocations();
assert_eq!(invocations[0].command, "git");
```

For scanner-based blocks:

```rust
let scanner = FakeScannerGateway::clean();
let scanner = FakeScannerGateway::with_vulnerabilities(vec![...]);
let scanner = FakeScannerGateway::with_error("cargo audit not installed");
```

This pattern allows testing every code path — including failure modes and edge
cases — without any real I/O:

```rust
#[tokio::test]
async fn detached_head_recovery_succeeds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = make_registry(/* ... */);

    // First call returns "HEAD" (detached); second call (checkout) succeeds.
    let shell = FakeShellGateway::sequence(vec![
        CommandResult { stdout: "HEAD\n".into(), exit_code: 0, success: true, .. },
        CommandResult { stdout: String::new(), exit_code: 0, success: true, .. },
    ]);
    let block = ValidateProject::with_shell(registry, shell);

    let result = block.execute(&trigger).await.unwrap();
    assert_eq!(result.events[0].payload["status"], "ok");
}
```

## File Organisation

Place block implementations in `foundryd/src/blocks/`:

```text
blocks/
├── mod.rs              # pub use declarations
├── greet.rs            # hello-world blocks (ComposeGreeting, DeliverGreeting)
├── validate.rs         # ValidateProject
├── resolve_gates.rs    # ResolveGates
├── run_preflight_gates.rs  # RunPreflightGates
├── run_verify_gates.rs     # RunVerifyGates
├── route_gate_result.rs    # RouteGateResult
├── route_validation_result.rs # RouteValidationResult
├── check_charter.rs    # CheckCharter
├── assess_project.rs   # AssessProject
├── triage_assessment.rs # TriageAssessment
├── create_plan.rs      # CreatePlan
├── execute_plan.rs     # ExecutePlan
├── execute_maintain.rs # ExecuteMaintain
├── retry_execution.rs  # RetryExecution
├── summarize_result.rs # SummarizeResult
├── git_ops.rs          # CommitAndPush
├── audit.rs            # AuditReleaseTag, AuditMainBranch
├── release.rs          # CutRelease, WatchPipeline
├── install.rs          # InstallLocally
├── remediate.rs        # RemediateVulnerability
└── scan.rs             # ScanDependencies
```
