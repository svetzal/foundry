//! Shared test fixtures for task block unit tests.
//!
//! Gated with `#[cfg(test)]` — this module is only compiled during testing.
#![allow(dead_code)]

use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
use foundry_core::throttle::Throttle;

/// Build a registry containing a single standard test project entry.
///
/// Uses `Stack::Rust`, agent `"claude"`, and `ActionFlags::default()`.
pub fn registry_with_project(name: &str, path: &str) -> Arc<Registry> {
    Arc::new(Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: name.to_string(),
            path: path.to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: ActionFlags::default(),
            install: None,
            timeout_secs: None,
        }],
    })
}

/// Build a registry containing a single project entry with custom fields via a pre-built entry.
pub fn registry_with_entry(entry: ProjectEntry) -> Arc<Registry> {
    Arc::new(Registry {
        version: 2,
        projects: vec![entry],
    })
}

/// Build a standard test project entry with default fields.
pub fn project_entry(name: &str, path: &str) -> ProjectEntry {
    ProjectEntry {
        name: name.to_string(),
        path: path.to_string(),
        stack: Stack::Rust,
        agent: "claude".to_string(),
        repo: String::new(),
        branch: "main".to_string(),
        skip: None,
        notes: None,
        actions: ActionFlags::default(),
        install: None,
        timeout_secs: None,
    }
}

/// Build a test event with the given type, project name, and payload.
pub fn make_trigger(event_type: EventType, project: &str, payload: serde_json::Value) -> Event {
    Event::new(event_type, project.to_string(), Throttle::Full, payload)
}
