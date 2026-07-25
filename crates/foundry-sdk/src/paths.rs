//! Foundry well-known path helpers.
//!
//! Centralises all `~/.foundry/*` path resolution so that every binary uses
//! identical env-var override logic.

use std::env;
use std::path::PathBuf;

/// Default foundryd listen address used when no startup override is configured.
pub const DEFAULT_DAEMON_LISTEN_ADDR: &str = "127.0.0.1:50051";

/// Default foundry CLI daemon URL used when no client override is configured.
pub const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:50051";

/// Returns the Foundry home directory (`~/.foundry` by default).
fn foundry_home() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|e| {
        // Best-effort: HOME is expected to be set in every real environment; falling
        // back to "." keeps foundry_home() infallible, but the resulting path would be
        // process-cwd-relative rather than the intended ~/.foundry, so this is worth
        // surfacing rather than swallowing.
        tracing::warn!(error = %e, "HOME env var unavailable; foundry_home() falling back to \".\"");
        ".".to_string()
    });
    PathBuf::from(format!("{home}/.foundry"))
}

/// Returns the configured foundryd listen address, if any.
///
/// Override with `FOUNDRYD_LISTEN_ADDR`. The value must be a socket address
/// such as `127.0.0.1:50051` or `0.0.0.0:50051`.
pub fn daemon_listen_addr() -> Option<String> {
    env::var("FOUNDRYD_LISTEN_ADDR").ok()
}

/// Returns the configured foundry CLI daemon URL, if any.
///
/// Override with `FOUNDRY_DAEMON_ADDR`. The value must be a tonic-compatible
/// URL such as `http://127.0.0.1:50051`.
pub fn daemon_url() -> Option<String> {
    env::var("FOUNDRY_DAEMON_ADDR").ok()
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

/// Returns the durable campaign store path.
///
/// Override with `FOUNDRY_CAMPAIGNS_PATH`.
pub fn campaigns_path() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_CAMPAIGNS_PATH") {
        PathBuf::from(p)
    } else {
        foundry_home().join("campaigns.json")
    }
}

/// Returns the root used for isolated one-shot task worktrees.
///
/// Override with `FOUNDRY_WORKTREES_DIR`.
pub fn worktrees_dir() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_WORKTREES_DIR") {
        PathBuf::from(p)
    } else {
        foundry_home().join("worktrees")
    }
}

/// Returns the root used for preservation bundles when a branch cannot be pushed.
pub fn preserved_dir() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_PRESERVED_DIR") {
        PathBuf::from(p)
    } else {
        foundry_home().join("preserved")
    }
}

/// Returns the agent model configuration file path.
///
/// This JSON store maps each provider's abstract model tiers and reasoning
/// effort levels to concrete model ids and CLI tokens (see
/// [`crate::agent_config`]). Defaults are baked in; this file overrides them.
///
/// Override with `FOUNDRY_AGENT_CONFIG_PATH`.
pub fn agent_config_path() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_AGENT_CONFIG_PATH") {
        PathBuf::from(p)
    } else {
        foundry_home().join("agents.json")
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

/// Returns the ops-digest output directory.
///
/// Each ops digest lands at `{ops_digests_dir}/{YYYY-MM-DD}.md`. Override
/// with `FOUNDRY_OPS_DIGESTS_DIR`.
pub fn ops_digests_dir() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_OPS_DIGESTS_DIR") {
        PathBuf::from(p)
    } else {
        foundry_home().join("ops-digests")
    }
}

/// Returns the directory containing MBOS JSONL event intake files.
///
/// Each file in this directory is named `YYYY-MM.jsonl` and contains
/// newline-delimited MBOS event JSON objects. Override with
/// `FOUNDRY_OPS_EVENTS_DIR`; the default points at Stacey's Operations layout.
pub fn ops_events_intake_dir() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_OPS_EVENTS_DIR") {
        PathBuf::from(p)
    } else {
        let home = env::var("HOME").unwrap_or_else(|e| {
            // Best-effort: HOME is expected to be set in every real environment;
            // falling back to "." keeps this function infallible, but the resulting
            // path would be process-cwd-relative rather than under the real home
            // directory, so this is worth surfacing rather than swallowing.
            tracing::warn!(
                error = %e,
                "HOME env var unavailable; ops_events_intake_dir() falling back to \".\""
            );
            ".".to_string()
        });
        PathBuf::from(format!("{home}/Work/Operations/Events/intake"))
    }
}

/// Returns the path to the ops-digest watermark file.
///
/// The watermark holds the ISO 8601 timestamp of the last MBOS event that was
/// included in a successfully written ops digest. On the next run only events
/// with `occurredAt` strictly after this timestamp are considered new.
pub fn ops_watermark_path() -> PathBuf {
    foundry_home().join("ops-digest.watermark")
}

/// Returns the maintenance-triage digest output directory.
///
/// Each triage digest lands at `{triage_dir}/{YYYY-MM-DD}.md`. Override with
/// `FOUNDRY_TRIAGE_DIR`.
pub fn triage_dir() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_TRIAGE_DIR") {
        PathBuf::from(p)
    } else {
        foundry_home().join("triage")
    }
}

/// Returns the directory holding per-session agent transcript JSONL files.
///
/// Defaults to `$HOME/.foundry/agent-sessions`.
pub fn agent_sessions_dir() -> PathBuf {
    foundry_home().join("agent-sessions")
}

/// Returns the supply-chain advisory digest output directory.
///
/// Each nightly supply-chain scan lands at `{supply_chain_dir}/{YYYY-MM-DD}.md`.
/// Override with `FOUNDRY_SUPPLY_CHAIN_DIR`; the typical Operations-side override
/// points this at `~/Work/Operations/Automation/supply-chain-audits`.
pub fn supply_chain_dir() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_SUPPLY_CHAIN_DIR") {
        PathBuf::from(p)
    } else {
        foundry_home().join("supply-chain")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triage_dir_defaults_under_foundry_home_when_env_unset() {
        if env::var("FOUNDRY_TRIAGE_DIR").is_ok() {
            return;
        }
        let dir = triage_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with(".foundry/triage"), "got: {s}");
    }

    #[test]
    fn agent_sessions_dir_is_under_foundry_home() {
        let dir = agent_sessions_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with(".foundry/agent-sessions"), "got: {s}");
    }

    #[test]
    fn ops_digests_dir_defaults_under_foundry_home_when_env_unset() {
        if env::var("FOUNDRY_OPS_DIGESTS_DIR").is_ok() {
            return;
        }
        let dir = ops_digests_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with(".foundry/ops-digests"), "got: {s}");
    }

    #[test]
    fn ops_events_intake_dir_defaults_under_home_work_operations_when_env_unset() {
        if env::var("FOUNDRY_OPS_EVENTS_DIR").is_ok() {
            return;
        }
        let dir = ops_events_intake_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with("Work/Operations/Events/intake"), "got: {s}");
    }

    #[test]
    fn supply_chain_dir_defaults_under_foundry_home_when_env_unset() {
        if env::var("FOUNDRY_SUPPLY_CHAIN_DIR").is_ok() {
            return;
        }
        let dir = supply_chain_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with(".foundry/supply-chain"), "got: {s}");
    }

    #[test]
    fn ops_watermark_path_is_under_foundry_home() {
        let path = ops_watermark_path();
        let s = path.to_string_lossy();
        assert!(s.ends_with(".foundry/ops-digest.watermark"), "got: {s}");
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

    #[test]
    fn daemon_defaults_match_historical_loopback_control_plane() {
        assert_eq!(DEFAULT_DAEMON_LISTEN_ADDR, "127.0.0.1:50051");
        assert_eq!(DEFAULT_DAEMON_URL, "http://127.0.0.1:50051");
    }
}
