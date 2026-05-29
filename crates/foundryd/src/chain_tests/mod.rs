//! Full-workflow chain integration tests.
//!
//! Relocated from `foundry-blocks` (where they lived as `blocks/*_chain_test.rs`)
//! because they wire the real blocks into a [`foundry_engine::engine::Engine`].
//! That combination — engine + concrete blocks — only comes together in the
//! host, so the tests live here to avoid a blocks↔engine dependency cycle.
//!
//! `test_helpers` is shared scaffolding (registers a full chain into an
//! `Engine`); the per-workflow test files exercise it.
#![cfg(test)]

pub mod test_helpers;

mod iterate_chain_test;
mod maintain_chain_test;
mod prompt_chain_test;
mod release_chain_test;
mod strategic_chain_test;
