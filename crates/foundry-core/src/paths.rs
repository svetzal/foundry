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

/// Returns the sentinels file path.
///
/// Sentinels are declarative, named, scheduled triggers that emit events into
/// the engine when their schedule fires (e.g., the nightly maintenance run).
///
/// Override with `FOUNDRY_SENTINELS_PATH`.
pub fn sentinels_path() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_SENTINELS_PATH") {
        PathBuf::from(p)
    } else {
        foundry_home().join("sentinels.json")
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

/// Returns the daily commit-digest output directory.
///
/// Each day's digest lands at `{digests_dir}/{YYYY-MM-DD}.md`. Override with
/// `FOUNDRY_DIGESTS_DIR`; the typical Operations-side override is to point
/// this at `~/Work/Operations/Automation/commit-digests`.
pub fn digests_dir() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_DIGESTS_DIR") {
        PathBuf::from(p)
    } else {
        foundry_home().join("digests")
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

    #[test]
    fn digests_dir_defaults_under_foundry_home_when_env_unset() {
        // We do not mutate env in tests (Rust 2024 makes env::set_var unsafe
        // and racy across the test binary's parallel runners). If a CI shell
        // exports FOUNDRY_DIGESTS_DIR for some reason, skip the assertion
        // rather than fight the harness.
        if env::var("FOUNDRY_DIGESTS_DIR").is_ok() {
            return;
        }
        let dir = digests_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with(".foundry/digests"), "got: {s}");
    }
}
