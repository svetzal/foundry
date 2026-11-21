//! Foundry well-known path helpers.
//!
//! Centralises all Foundry home path resolution so that every binary uses
//! identical env-var override logic. The home is `~/.foundry` unless
//! `FOUNDRY_HOME` relocates it (see [`foundry_home`]).

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

/// Default foundryd listen address used when no startup override is configured.
pub const DEFAULT_DAEMON_LISTEN_ADDR: &str = "127.0.0.1:50051";

/// Default foundry CLI daemon URL used when no client override is configured.
pub const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:50051";

/// Environment variable that relocates the whole Foundry home.
pub const FOUNDRY_HOME_ENV: &str = "FOUNDRY_HOME";

/// Returns the Foundry home directory.
///
/// `FOUNDRY_HOME` when it is set and not empty, otherwise `$HOME/.foundry`.
/// Every default path in this module is under it, so one variable moves all
/// of Foundry's state, for example onto a host's data disk. The per-path
/// overrides such as `FOUNDRY_REGISTRY_PATH` still win over it.
pub fn foundry_home() -> PathBuf {
    resolve_foundry_home(env::var_os(FOUNDRY_HOME_ENV), || {
        env::var("HOME").unwrap_or_else(|e| {
            // Best-effort: HOME is expected to be set in every real environment; falling
            // back to "." keeps foundry_home() infallible, but the resulting path would be
            // process-cwd-relative rather than the intended ~/.foundry, so this is worth
            // surfacing rather than swallowing.
            tracing::warn!(error = %e, "HOME env var unavailable; foundry_home() falling back to \".\"");
            ".".to_string()
        })
    })
}

/// The Foundry home for an explicit `FOUNDRY_HOME` value, falling back to
/// `<home>/.foundry`. An empty value counts as unset, so a bare
/// `FOUNDRY_HOME=` line in an environment file cannot put state in the
/// process's working directory.
fn resolve_foundry_home(explicit: Option<OsString>, home: impl FnOnce() -> String) -> PathBuf {
    match explicit {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(home()).join(".foundry"),
    }
}

/// Environment variable that gives an agent session its scratch directory.
///
/// Foundry sets it for every coding-agent session to an absolute path in the
/// evidence store, `{evidence_dir}/{trace_id}/{attempt}/`. Agents write
/// proof, blocker records and command logs there, never inside the project
/// checkout. `foundry capture` writes its logs to `$FOUNDRY_SCRATCH_DIR/logs`
/// when it is set.
pub const SCRATCH_DIR_ENV: &str = "FOUNDRY_SCRATCH_DIR";

/// Returns the evidence store root: `{foundry_home}/evidence`.
///
/// Each agent run's scratch directory is `{evidence_dir}/{trace_id}/{attempt}/`
/// (see [`SCRATCH_DIR_ENV`]), and failed-gate logs go to
/// `{evidence_dir}/gate-logs/`. The daemon prunes old trace directories
/// (`FOUNDRY_EVIDENCE_RETENTION_DAYS`); gate logs keep their own retention.
pub fn evidence_dir() -> PathBuf {
    foundry_home().join("evidence")
}

/// Returns the configured foundryd listen address, if any.
///
/// Override with `FOUNDRYD_LISTEN_ADDR`. The value must be a socket address
/// such as `127.0.0.1:50051` or `0.0.0.0:50051`.
pub fn daemon_listen_addr() -> Option<String> {
    env::var("FOUNDRYD_LISTEN_ADDR").ok()
}

/// Returns the path of the `foundryd` single-instance lock file.
///
/// `foundryd` holds an exclusive advisory lock on this file for its whole
/// lifetime and records its pid in it, so a second daemon started against the
/// same Foundry home refuses to start before it touches any shared state.
///
/// Override with `FOUNDRYD_LOCK_PATH`.
pub fn daemon_lock_path() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRYD_LOCK_PATH") {
        PathBuf::from(p)
    } else {
        foundry_home().join("foundryd.lock")
    }
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

/// Returns the durable work-item ledger path.
///
/// The ledger holds one record per unit of work Foundry dispatched (see
/// [`crate::work_item`]). It is daemon-owned and authoritative: every
/// mutation loads this file, applies the change, and saves it.
///
/// Override with `FOUNDRY_WORK_ITEMS_PATH`.
pub fn work_items_path() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_WORK_ITEMS_PATH") {
        PathBuf::from(p)
    } else {
        foundry_home().join("work-items.json")
    }
}

/// Returns the pacing limits file path.
///
/// The operator-edited limits the scheduler reads on every tick (see
/// [`crate::pacing::Limits`]). Absent means the defaults. Foundry never writes
/// it.
///
/// Override with `FOUNDRY_PACING_PATH`.
pub fn pacing_path() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_PACING_PATH") {
        PathBuf::from(p)
    } else {
        foundry_home().join("pacing.json")
    }
}

/// Returns the pacing state file path.
///
/// Daemon-owned: which lanes an operator has paused (see
/// [`crate::pacing::PauseState`]), saved through a same-directory temp-file
/// rename so a pause survives a restart.
///
/// Override with `FOUNDRY_PACING_STATE_PATH`.
pub fn pacing_state_path() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_PACING_STATE_PATH") {
        PathBuf::from(p)
    } else {
        foundry_home().join("pacing-state.json")
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

/// Returns the token price-book file path.
///
/// This JSON store holds published vendor list rates per model per token type
/// (see [`crate::token_rates`]). Rates change on vendor schedules rather than
/// release schedules, so defaults are baked in and this file overrides them
/// without a rebuild.
///
/// Override with `FOUNDRY_TOKEN_RATES_PATH`.
pub fn token_rates_path() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_TOKEN_RATES_PATH") {
        PathBuf::from(p)
    } else {
        foundry_home().join("token-rates.json")
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
/// Defaults to `{foundry_home}/agent-sessions`.
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

/// Reconciliation reports; override with `FOUNDRY_RECONCILE_DIR`.
pub fn reconcile_dir() -> PathBuf {
    env::var_os("FOUNDRY_RECONCILE_DIR")
        .map_or_else(|| foundry_home().join("reconcile"), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    // We do not mutate env in tests (Rust 2024 makes env::set_var unsafe and
    // racy across the test binary's parallel runners). The defaults are
    // asserted against `foundry_home()`, so they hold whether or not the shell
    // exports FOUNDRY_HOME; a test whose own per-path override is exported
    // skips rather than fight the harness.

    #[test]
    fn foundry_home_defaults_to_dot_foundry_under_home() {
        assert_eq!(
            resolve_foundry_home(None, || "/home/u".to_string()),
            PathBuf::from("/home/u/.foundry")
        );
    }

    #[test]
    fn foundry_home_env_relocates_the_whole_home() {
        assert_eq!(
            resolve_foundry_home(Some("/srv/work/foundry".into()), || unreachable!()),
            PathBuf::from("/srv/work/foundry")
        );
    }

    #[test]
    fn an_empty_foundry_home_counts_as_unset() {
        assert_eq!(
            resolve_foundry_home(Some(OsString::new()), || "/home/u".to_string()),
            PathBuf::from("/home/u/.foundry")
        );
    }

    #[test]
    fn foundry_home_follows_the_environment() {
        match env::var_os(FOUNDRY_HOME_ENV).filter(|dir| !dir.is_empty()) {
            Some(dir) => assert_eq!(foundry_home(), PathBuf::from(dir)),
            None => assert!(foundry_home().ends_with(".foundry")),
        }
    }

    #[test]
    fn evidence_dir_is_under_foundry_home() {
        assert_eq!(evidence_dir(), foundry_home().join("evidence"));
    }

    #[test]
    fn daemon_lock_path_defaults_under_foundry_home_when_env_unset() {
        if env::var("FOUNDRYD_LOCK_PATH").is_ok() {
            return;
        }
        assert_eq!(daemon_lock_path(), foundry_home().join("foundryd.lock"));
    }

    #[test]
    fn triage_dir_defaults_under_foundry_home_when_env_unset() {
        if env::var("FOUNDRY_TRIAGE_DIR").is_ok() {
            return;
        }
        assert_eq!(triage_dir(), foundry_home().join("triage"));
    }

    #[test]
    fn work_items_path_defaults_under_foundry_home_when_env_unset() {
        if env::var("FOUNDRY_WORK_ITEMS_PATH").is_ok() {
            return;
        }
        assert_eq!(work_items_path(), foundry_home().join("work-items.json"));
    }

    #[test]
    fn agent_sessions_dir_is_under_foundry_home() {
        assert_eq!(agent_sessions_dir(), foundry_home().join("agent-sessions"));
    }

    #[test]
    fn ops_digests_dir_defaults_under_foundry_home_when_env_unset() {
        if env::var("FOUNDRY_OPS_DIGESTS_DIR").is_ok() {
            return;
        }
        assert_eq!(ops_digests_dir(), foundry_home().join("ops-digests"));
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
        assert_eq!(supply_chain_dir(), foundry_home().join("supply-chain"));
    }

    #[test]
    fn ops_watermark_path_is_under_foundry_home() {
        assert_eq!(ops_watermark_path(), foundry_home().join("ops-digest.watermark"));
    }

    #[test]
    fn digests_dir_defaults_under_foundry_home_when_env_unset() {
        if env::var("FOUNDRY_DIGESTS_DIR").is_ok() {
            return;
        }
        assert_eq!(digests_dir(), foundry_home().join("digests"));
    }

    #[test]
    fn daemon_defaults_match_historical_loopback_control_plane() {
        assert_eq!(DEFAULT_DAEMON_LISTEN_ADDR, "127.0.0.1:50051");
        assert_eq!(DEFAULT_DAEMON_URL, "http://127.0.0.1:50051");
    }
}
