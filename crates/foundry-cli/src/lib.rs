//! Foundry CLI — library interface.
//!
//! This crate is primarily the `foundry` binary.  The library target exists
//! solely to expose internal types for integration testing.  Application code
//! should not depend on this crate as a library in production.

pub mod daemon;
pub mod registry_commands;
pub mod render;

/// Generated gRPC bindings (tonic / prost).
pub mod proto {
    #![allow(clippy::all, clippy::pedantic)]
    tonic::include_proto!("foundry");
}
