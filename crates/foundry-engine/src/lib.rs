//! Foundry workflow engine — the *mechanism*.
//!
//! The engine dispatches events to the [`TaskBlock`](foundry_sdk::task_block::TaskBlock)s
//! registered with it, runs scatter/gather (map/reduce) coordination, applies
//! retry policies, persists events, and stamps OpenTelemetry-shaped span fields.
//!
//! It is **generic over blocks**: dispatch is `block.sinks_on().contains(&ty)`,
//! and the engine contains no `match` on specific event types. This is what
//! lets new blocks and workflows plug in without touching the engine.
pub mod engine;
pub mod event_writer;
pub mod gather_store;

pub use engine::{BlockExecution, Engine, ProcessResult};
