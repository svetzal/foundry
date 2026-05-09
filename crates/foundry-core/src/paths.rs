//! Foundry well-known path helpers.
//!
//! Centralises all `~/.foundry/*` path resolution so that every binary uses
//! identical env-var override logic.

use std::env;
use std::path::PathBuf;

/// Returns the Foundry home directory (`~/.foundry` by default).
fn foundry_home() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(format!("{home}/.foundry"))
}

/// Returns the project registry file path.
///
/// Override with `FOUNDRY_REGISTRY_PATH`.
pub fn registry_path() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_REGISTRY_PATH") {
        PathBuf::from(p)
    } else {
        foundry_home().join("registry.json")
    }
}

/// Returns the JSONL event output directory.
///
/// Override with `FOUNDRY_EVENTS_DIR`.
pub fn events_dir() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_EVENTS_DIR") {
        PathBuf::from(p)
    } else {
        foundry_home().join("events")
    }
}

/// Returns the persistent trace storage directory.
///
/// Override with `FOUNDRY_TRACES_DIR`.
pub fn traces_dir() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_TRACES_DIR") {
        PathBuf::from(p)
    } else {
        foundry_home().join("traces")
    }
}

/// Returns the centralized audit log directory.
///
/// Override with `FOUNDRY_AUDITS_DIR`.
pub fn audits_dir() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_AUDITS_DIR") {
        PathBuf::from(p)
    } else {
        foundry_home().join("audits")
    }
}

/// Returns the directory holding per-session agent transcript JSONL files.
///
/// Defaults to `$HOME/.foundry/agent-sessions`.
pub fn agent_sessions_dir() -> PathBuf {
    foundry_home().join("agent-sessions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_sessions_dir_is_under_foundry_home() {
        let dir = agent_sessions_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with(".foundry/agent-sessions"), "got: {s}");
    }
}
