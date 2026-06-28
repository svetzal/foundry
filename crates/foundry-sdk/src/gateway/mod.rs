//! Gateway contracts — the I/O boundary task blocks depend on.
//!
//! Task blocks are the *functional core*; gateways are the *imperative shell*.
//! A block never spawns a process, runs an audit, or invokes an agent
//! directly — it calls one of these traits. The host (`foundryd`) supplies the
//! production implementations; tests supply the `fakes` (enable the
//! `test-support` feature). This is what lets a contributor write and test a
//! block without any real I/O.
//!
//! The trait method signatures, and the data types they exchange
//! ([`CommandResult`], [`AuditResult`], [`AgentRequest`], …), are part of the
//! stable SDK contract.

mod agent;
mod scanner;
mod shell;

pub use agent::*;
pub use scanner::*;
pub use shell::*;

/// In-memory gateway fakes for testing blocks without real I/O.
///
/// Enable with the `test-support` feature (typically as a dev-dependency
/// feature). Each fake records its invocations and returns pre-configured
/// results, so a block's behaviour can be asserted in isolation.
#[cfg(feature = "test-support")]
pub mod fakes;
