//! Shared daemon/offline helpers for CLI commands.
//!
//! Two control-plane policies coexist in the CLI:
//!
//! - Commands whose default online path must remain daemon-authoritative use
//!   [`connect_daemon_required`] and fail cleanly when the daemon is
//!   unreachable. The registry, sentinel, and campaign online paths are in
//!   this category.
//!
//! This module is the single home for connection and error-shaping logic so
//! command modules do not duplicate it.

use anyhow::Result;

use crate::proto::foundry_client::FoundryClient;

/// Connect to the daemon or return a stable actionable error.
///
/// Use this for commands whose default online path must not fall back to a
/// direct file mutation when `foundryd` is unreachable.
pub async fn connect_daemon_required(
    addr: &str,
    offline_hint: &str,
) -> Result<FoundryClient<tonic::transport::Channel>> {
    FoundryClient::connect(addr.to_string()).await.map_err(|_| {
        anyhow::anyhow!(
            "foundryd is not reachable at {addr}; start the daemon or rerun with `{offline_hint}`"
        )
    })
}

/// Convert a [`tonic::Status`] to an [`anyhow::Error`] with a consistent
/// `"daemon error: <code> — <message>"` format.
#[allow(clippy::needless_pass_by_value)]
pub fn status_to_anyhow(s: tonic::Status) -> anyhow::Error {
    anyhow::anyhow!("daemon error: {} — {}", s.code(), s.message())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_to_anyhow_contains_daemon_error_prefix() {
        let status = tonic::Status::not_found("project missing");
        let err = status_to_anyhow(status);
        assert!(
            err.to_string().contains("daemon error:"),
            "formatted error should start with 'daemon error:'"
        );
    }

    #[tokio::test]
    async fn connect_daemon_required_returns_stable_actionable_error() {
        let err =
            connect_daemon_required("http://127.0.0.1:0", "foundry campaign decide c --offline")
                .await
                .expect_err("unreachable daemon must error");

        assert_eq!(
            err.to_string(),
            "foundryd is not reachable at http://127.0.0.1:0; start the daemon or rerun with `foundry campaign decide c --offline`"
        );
    }

    // ── Structural gate ───────────────────────────────────────────────────────

    /// `campaign_commands.rs` must not use the graceful-degradation helpers or
    /// embed the warning string, because the default online path is now
    /// daemon-authoritative end to end.
    #[test]
    fn campaign_commands_never_use_daemon_fallback_helpers() {
        let src = include_str!("campaign_commands.rs");
        assert!(
            !src.contains("with_daemon_or_offline("),
            "campaign_commands.rs must not use with_daemon_or_offline; \
            the online campaign path is daemon-authoritative"
        );
        assert!(
            !src.contains("with_daemon_or_offline_render("),
            "campaign_commands.rs must not use with_daemon_or_offline_render; \
            the online campaign path is daemon-authoritative"
        );
        assert!(
            !src.contains("daemon not reachable"),
            "campaign_commands.rs must not contain the fallback warning string; \
            the online campaign path must fail cleanly instead"
        );
        assert!(
            src.contains("connect_daemon_required"),
            "campaign_commands.rs must use connect_daemon_required for online commands"
        );
    }

    #[test]
    fn registry_commands_never_use_daemon_fallback_helpers() {
        let src = include_str!("registry_commands.rs");
        assert!(
            !src.contains("daemon not reachable"),
            "registry_commands.rs must not contain the fallback warning string; \
            the online registry path must fail cleanly instead"
        );
        assert!(
            src.contains("connect_daemon_required"),
            "registry_commands.rs must connect through connect_daemon_required \
            so unreachable daemons fail cleanly without touching the file store"
        );
    }

    #[test]
    fn sentinel_commands_never_use_daemon_fallback_helpers() {
        let src = include_str!("sentinel_commands.rs");
        assert!(
            !src.contains("with_daemon_or_offline("),
            "sentinel_commands.rs must not use with_daemon_or_offline; \
            the online sentinel path is daemon-authoritative"
        );
        assert!(
            !src.contains("daemon not reachable"),
            "sentinel_commands.rs must not contain the fallback warning string; \
            the online sentinel path must fail cleanly instead"
        );
        assert!(
            src.contains("connect_daemon_required"),
            "sentinel_commands.rs must use connect_daemon_required for online commands"
        );
    }
}
