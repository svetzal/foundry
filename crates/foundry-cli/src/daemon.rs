//! Shared daemon/offline helpers for CLI commands.
//!
//! Two control-plane policies coexist in the CLI:
//!
//! - Mutation commands such as sentinel enable/disable use a
//!   graceful-degradation policy: try gRPC first, warn on unreachable daemon,
//!   then fall back to direct store mutation.
//! - Commands whose default online path must remain daemon-authoritative use
//!   [`connect_daemon_required`] and fail cleanly when the daemon is
//!   unreachable. The registry and campaign online paths are in this category.
//!
//! This module is the single home for both policies so command modules do not
//! duplicate connection or error-shaping logic.
//!
//! Two variants are provided:
//!
//! - [`with_daemon_or_offline`] — for commands whose success message is a
//!   fixed confirmation string (sentinel mutations).
//! - [`connect_daemon_required`] — for commands whose online path must fail
//!   cleanly when the daemon is unreachable instead of mutating a local store.

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

/// Run an operation via the daemon when reachable, or fall back to direct
/// file mutation when not.
///
/// * `addr` — gRPC endpoint to connect to.
/// * `offline` — if `true`, skip the daemon and go straight to `via_file`.
/// * `success` — confirmation message printed once after either path succeeds.
/// * `via_daemon` — async closure that receives a connected [`FoundryClient`]
///   and performs the gRPC call.  Returns `anyhow::Result<()>`.
/// * `via_file` — synchronous closure that performs the direct file mutation.
///   Returns `anyhow::Result<()>`.
///
/// If either path returns an error it is propagated and the success message
/// is **not** printed.
pub async fn with_daemon_or_offline<Online, OnlineFut, Offline>(
    addr: &str,
    offline: bool,
    success: &str,
    via_daemon: Online,
    via_file: Offline,
) -> Result<()>
where
    Online: FnOnce(FoundryClient<tonic::transport::Channel>) -> OnlineFut,
    OnlineFut: std::future::Future<Output = Result<()>>,
    Offline: FnOnce() -> Result<()>,
{
    if !offline {
        match FoundryClient::connect(addr.to_string()).await {
            Ok(client) => {
                via_daemon(client).await?;
                println!("{success}");
                return Ok(());
            }
            Err(_) => {
                eprintln!("warning: daemon not reachable, falling back to direct file mutation");
            }
        }
    }

    via_file()?;
    println!("{success}");
    Ok(())
}

/// Convert a [`tonic::Status`] to an [`anyhow::Error`] with a consistent
/// `"daemon error: <code> — <message>"` format.
#[allow(clippy::needless_pass_by_value)]
pub fn status_to_anyhow(s: tonic::Status) -> anyhow::Error {
    anyhow::anyhow!("daemon error: {} — {}", s.code(), s.message())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    #[tokio::test]
    async fn offline_true_skips_daemon_and_runs_via_file() {
        let file_ran = Arc::new(AtomicBool::new(false));
        let file_ran_clone = Arc::clone(&file_ran);

        with_daemon_or_offline(
            "http://127.0.0.1:0", // unreachable — but never tried when offline=true
            true,
            "done",
            |_client| async { panic!("daemon should not be called when offline=true") },
            move || {
                file_ran_clone.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect("should succeed");

        assert!(file_ran.load(Ordering::SeqCst), "via_file closure must have run");
    }

    #[tokio::test]
    async fn via_file_error_propagates_without_printing_success() {
        let result = with_daemon_or_offline(
            "http://127.0.0.1:0",
            true,
            "done",
            |_client| async { Ok(()) },
            || anyhow::bail!("file mutation failed"),
        )
        .await;

        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("file mutation failed"),
            "error message should propagate"
        );
    }

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
            !src.contains("with_daemon_or_offline("),
            "registry_commands.rs must not use with_daemon_or_offline; \
            the online registry path is daemon-authoritative"
        );
        assert!(
            !src.contains("with_daemon_or_offline_render("),
            "registry_commands.rs must not use with_daemon_or_offline_render; \
            the online registry path is daemon-authoritative"
        );
        assert!(
            src.contains("connect_daemon_required"),
            "registry_commands.rs must connect through connect_daemon_required \
            so unreachable daemons fail cleanly without touching the file store"
        );
    }
}
