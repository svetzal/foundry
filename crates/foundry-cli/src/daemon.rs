//! Shared daemon/offline helpers for CLI commands.
//!
//! Two control-plane policies coexist in the CLI:
//!
//! - Commands whose default online path must remain daemon-authoritative use
//!   [`connect_daemon_required`] and fail cleanly when the daemon is
//!   unreachable. The registry, sentinel, and campaign online paths are in
//!   this category.
//! - Commands whose only online source of truth is the daemon but that have no
//!   offline recovery path use [`connect_daemon_online`].
//!
//! This module is the single home for connection and error-shaping logic so
//! command modules do not duplicate it. Every client comes from [`connect`],
//! which applies the shared message limit (see [`foundry_sdk::grpc`]).

use anyhow::Result;

use crate::proto::foundry_client::FoundryClient;

/// Resolve the daemon URL for a CLI invocation.
///
/// Precedence is:
/// 1. Explicit `--addr`
/// 2. Persistent environment/config default (`FOUNDRY_DAEMON_ADDR`)
/// 3. Historical loopback fallback
#[must_use]
pub fn resolve_daemon_addr(explicit: Option<&str>, configured_default: Option<&str>) -> String {
    explicit
        .or(configured_default)
        .unwrap_or(foundry_sdk::paths::DEFAULT_DAEMON_URL)
        .to_string()
}

/// Connect to the daemon at `addr` with the message limit from
/// `FOUNDRY_GRPC_MAX_MESSAGE_MIB` in both directions, instead of tonic's
/// 4 MiB decode limit.
pub async fn connect(
    addr: &str,
) -> Result<FoundryClient<tonic::transport::Channel>, tonic::transport::Error> {
    let limit = foundry_sdk::grpc::max_message_bytes();
    Ok(FoundryClient::connect(addr.to_string())
        .await?
        .max_decoding_message_size(limit)
        .max_encoding_message_size(limit))
}

/// Connect to the daemon or return a stable actionable error.
///
/// Use this for commands whose default online path must not fall back to a
/// direct file mutation when `foundryd` is unreachable.
pub async fn connect_daemon_required(
    addr: &str,
    offline_hint: &str,
) -> Result<FoundryClient<tonic::transport::Channel>> {
    connect(addr).await.map_err(|_| {
        anyhow::anyhow!(
            "foundryd is not reachable at {addr}; start the daemon or rerun with `{offline_hint}`"
        )
    })
}

/// Connect to the daemon for commands that have no offline recovery mode.
pub async fn connect_daemon_online(addr: &str) -> Result<FoundryClient<tonic::transport::Channel>> {
    connect(addr)
        .await
        .map_err(|_| anyhow::anyhow!("foundryd is not reachable at {addr}; start the daemon"))
}

/// Convert a [`tonic::Status`] to an [`anyhow::Error`] with a consistent
/// `"daemon error: <code> — <message>"` format. A message over the size
/// limit also names the variable that raises it.
#[allow(clippy::needless_pass_by_value)]
pub fn status_to_anyhow(s: tonic::Status) -> anyhow::Error {
    if foundry_sdk::grpc::is_message_too_large(s.message()) {
        return anyhow::anyhow!(
            "daemon error: {} — {}; set {} (in MiB, default {}) higher on the daemon and this host",
            s.code(),
            s.message(),
            foundry_sdk::grpc::MAX_MESSAGE_MIB_ENV,
            foundry_sdk::grpc::DEFAULT_MAX_MESSAGE_MIB
        );
    }
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

    #[test]
    fn status_to_anyhow_names_the_limit_variable_for_an_oversized_message() {
        let status = tonic::Status::out_of_range(
            "Error, decoded message length too large: found 4208032 bytes, the limit is: 4194304 bytes",
        );
        let err = status_to_anyhow(status).to_string();
        assert!(err.contains("decoded message length too large"), "{err}");
        assert!(err.contains("FOUNDRY_GRPC_MAX_MESSAGE_MIB"), "{err}");
    }

    #[test]
    fn status_to_anyhow_leaves_other_errors_without_the_limit_hint() {
        let err = status_to_anyhow(tonic::Status::not_found("project missing")).to_string();
        assert!(!err.contains("FOUNDRY_GRPC_MAX_MESSAGE_MIB"), "{err}");
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

    #[tokio::test]
    async fn connect_daemon_online_returns_stable_live_only_error() {
        let err = connect_daemon_online("http://127.0.0.1:0")
            .await
            .expect_err("unreachable daemon must error");

        assert_eq!(
            err.to_string(),
            "foundryd is not reachable at http://127.0.0.1:0; start the daemon"
        );
    }

    #[test]
    fn resolve_daemon_addr_prefers_explicit_flag() {
        let addr =
            resolve_daemon_addr(Some("http://10.0.0.5:50051"), Some("http://10.0.0.6:50051"));
        assert_eq!(addr, "http://10.0.0.5:50051");
    }

    #[test]
    fn resolve_daemon_addr_uses_configured_default_when_flag_absent() {
        let addr = resolve_daemon_addr(None, Some("http://10.0.0.6:50051"));
        assert_eq!(addr, "http://10.0.0.6:50051");
    }

    #[test]
    fn resolve_daemon_addr_falls_back_to_historical_loopback_url() {
        let addr = resolve_daemon_addr(None, None);
        assert_eq!(addr, "http://127.0.0.1:50051");
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
    fn queue_commands_never_use_daemon_fallback_helpers() {
        let src = include_str!("queue_commands.rs");
        assert!(
            !src.contains("with_daemon_or_offline("),
            "queue_commands.rs must not use with_daemon_or_offline; \
            the online queue path is daemon-authoritative"
        );
        assert!(
            !src.contains("daemon not reachable"),
            "queue_commands.rs must not contain the fallback warning string; \
            the online queue path must fail cleanly instead"
        );
        assert!(
            src.contains("connect_daemon_required"),
            "queue_commands.rs must use connect_daemon_required for online commands"
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
