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

## File Organization

Place block implementations in `foundryd/src/blocks/`:

```text
blocks/
├── mod.rs      # pub use declarations
├── greet.rs    # hello-world blocks
└── audit.rs    # vulnerability audit blocks (future)
```
