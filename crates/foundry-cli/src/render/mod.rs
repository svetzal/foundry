//! Pure rendering functions — no I/O.
//!
//! Every function in this tree is pure: it takes data (proto types, SDK types,
//! parsed payloads) and returns/appends `String`.  No `println!`, no I/O, no
//! gRPC, no filesystem.  Command modules fetch data, call these, and `print!`
//! once.
#![deny(clippy::print_stdout, clippy::print_stderr)]

pub mod event;
pub mod registry;
pub mod sentinel;
pub mod trace_tree;
pub mod workflow;
