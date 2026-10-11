//! Host-provided admission at the event routing boundary.
//!
//! The engine owns event traversal; the host owns durable scheduling policy.
//! Deferred scatter children stay outstanding until their scheduled execution
//! returns, so waiting for admission cannot close a gather as stalled.

use std::future::Future;
use std::pin::Pin;

use foundry_sdk::event::Event;
use foundry_sdk::trace::ProcessResult;

/// Routing decision made before any block receives an event.
pub enum Admission {
    /// Route the event within its existing execution ownership.
    Execute,
    /// A durable item holds this event until the scheduler starts it.
    Deferred {
        /// Ledger identity used to await the scheduled result.
        id: String,
        /// Submission lifecycle fact, absent when joining an existing item.
        submitted: Option<Box<Event>>,
    },
}

/// Boxed asynchronous admission result.
pub type ResultFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<ProcessResult>> + Send + 'a>>;

/// Durable execution policy implemented by the daemon.
pub trait WorkAdmission: Send + Sync {
    /// Shared wake for admission, execution release, and operator controls.
    fn wake_handle(&self) -> std::sync::Arc<tokio::sync::Notify>;
    /// Typed terminal facts for a root refused before any block runs.
    fn failure_events(&self, root: &Event, reason: &str) -> Vec<Event>;
    /// Validate an exact scheduler-started root and return its execution owner.
    fn owner(&self, root: &Event) -> anyhow::Result<Option<String>>;
    /// Decide whether to route or durably defer an event.
    fn prepare(&self, event: &Event, owner: Option<&str>) -> anyhow::Result<Admission>;
    /// Wait for a deferred item's result, including cancellation and failure.
    fn wait(&self, id: &str) -> ResultFuture<'_>;
    /// Persist settlement after the admitted traversal and all its side effects end.
    fn complete(&self, root: &Event, result: &ProcessResult) -> anyhow::Result<Vec<Event>>;
}
