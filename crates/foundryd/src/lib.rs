//! Foundry daemon — library interface.
//!
//! This crate is primarily the `foundryd` binary.  The library target exists
//! solely to expose internal types for integration testing.  Application code
//! should not depend on this crate as a library in production.
//!
//! Many of the internal modules are private and only used in test compilation.
//! This lib target exists solely to support integration tests; broad lint
//! suppressions here are intentional.  The private modules (blocks, gateway,
//! etc.) are included so that engine.rs's `#[cfg(test)]` helpers (which
//! reference `crate::blocks` and `crate::gateway`) compile under the lib
//! target.  Most of those re-exported symbols are only referenced by a subset
//! of the test helpers, leaving others technically unused in any given
//! compilation mode.
#![allow(dead_code, unused_imports)]

pub mod service;
pub mod trace_store;
pub mod workflow_tracker;

/// Generated gRPC bindings (tonic / prost).
pub mod proto {
    #![allow(clippy::all, clippy::pedantic)]
    tonic::include_proto!("foundry");
}

// All daemon-internal modules.  These are private (not part of the library's
// public API) but must be declared here so that engine.rs test helpers that
// reference `crate::blocks` and `crate::gateway` compile under the lib target.
mod orchestrator;

// Engine↔block integration tests relocated here when the engine and blocks
// moved to their own crates (they wire real blocks into a foundry_engine::Engine).
#[cfg(test)]
mod chain_tests;
#[cfg(test)]
mod engine_remediation_tests;
