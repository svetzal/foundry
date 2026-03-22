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

## File Organization

Place block implementations in `foundryd/src/blocks/`:

```text
blocks/
├── mod.rs           # pub use declarations
├── greet.rs         # hello-world blocks (ComposeGreeting, DeliverGreeting)
├── validate.rs      # ValidateProject
├── hone_iterate.rs  # RunHoneIterate
├── hone_maintain.rs # RunHoneMaintain
├── git_ops.rs       # CommitAndPush
├── audit.rs         # AuditReleaseTag, AuditMainBranch
├── release.rs       # CutRelease, WatchPipeline
├── install.rs       # InstallLocally
├── remediate.rs     # RemediateVulnerability
└── scan.rs          # ScanDependencies
```
