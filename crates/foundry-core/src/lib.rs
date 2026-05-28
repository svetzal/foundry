//! Compatibility shim for the former `foundry-core` crate.
//!
//! The contract crate was renamed to [`foundry_sdk`] to make its role — the
//! stable SDK that third-party blocks and workflows compile against — explicit.
//! This crate re-exports everything from `foundry_sdk` so existing
//! `use foundry_core::…` paths keep working unchanged during the transition.
//!
//! New code should depend on `foundry-sdk` directly. This shim will be removed
//! in a future release.
pub use foundry_sdk::*;
