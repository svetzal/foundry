//! foundry-blocks — Foundry's built-in task blocks and their I/O support.
//!
//! This crate is the **batteries**: the ~38 concrete [`TaskBlock`]s that ship
//! with Foundry, plus the I/O modules they lean on (shell execution, audit
//! scanning, agent invocation, gateway implementations, gate running).
//!
//! Crucially, it depends only on the SDK contract (`foundry-sdk`, via the
//! `foundry-core` shim) — **not** on the engine or the host. That is the proof
//! that a third party can build their own block crate the same way: implement
//! [`TaskBlock`], depend on the SDK, and nothing else.
//!
//! [`TaskBlock`]: foundry_core::task_block::TaskBlock
#![allow(dead_code)]

pub mod agent_stream;
pub mod blocks;
pub mod charter;
pub mod gate_file;
pub mod gate_runner;
pub mod gateway;
pub mod scanner;
pub mod shell;
pub mod summary;
pub mod trace_writer;
