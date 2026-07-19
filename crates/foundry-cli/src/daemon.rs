//! Shared daemon-or-offline fallback helper.
//!
//! The fallback protocol — try gRPC, warn on unreachable, fall back to
//! direct file mutation — is one decision that applies to every registry,
//! sentinel, and campaign mutation command.  This module is the single home
//! for that decision.
//!
//! Two variants are provided:
//!
//! - [`with_daemon_or_offline`] — for commands whose success message is a
//!   fixed confirmation string (registry, sentinel mutations).
//! - [`with_daemon_or_offline_render`] — for commands whose output is built
//!   from the typed RPC response (e.g. `campaign pause`, which renders the
//!   full `PauseCampaignResponse.campaign` detail).
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

/// Run an operation via the daemon when reachable, or fall back to direct
/// file mutation when not, returning the rendered output string from
/// whichever path ran.
///
/// This is the response-threading variant of [`with_daemon_or_offline`] for
/// commands that build their output from the typed RPC response rather than
/// a fixed success string.
///
/// * `addr` — gRPC endpoint to connect to.
/// * `offline` — if `true`, skip the daemon and go straight to `via_file`.
/// * `via_daemon` — async closure that receives a connected [`FoundryClient`]
///   and returns the rendered output string.  Returns `anyhow::Result<String>`.
/// * `via_file` — synchronous closure that performs the direct file mutation
///   and returns the rendered output string.  Returns `anyhow::Result<String>`.
///
/// If either path returns an error it is propagated.
pub async fn with_daemon_or_offline_render<Online, OnlineFut, Offline>(
    addr: &str,
    offline: bool,
    via_daemon: Online,
    via_file: Offline,
) -> Result<String>
where
    Online: FnOnce(FoundryClient<tonic::transport::Channel>) -> OnlineFut,
    OnlineFut: std::future::Future<Output = Result<String>>,
    Offline: FnOnce() -> Result<String>,
{
    if !offline {
        match FoundryClient::connect(addr.to_string()).await {
            Ok(client) => {
                return via_daemon(client).await;
            }
            Err(_) => {
                eprintln!("warning: daemon not reachable, falling back to direct file mutation");
            }
        }
    }

    via_file()
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

    // ── with_daemon_or_offline_render tests ───────────────────────────────────

    #[tokio::test]
    async fn render_offline_true_skips_daemon_returns_file_string() {
        let result = with_daemon_or_offline_render(
            "http://127.0.0.1:0", // unreachable — but never tried when offline=true
            true,
            |_client| async { panic!("daemon should not be called when offline=true") },
            || Ok("offline output".to_string()),
        )
        .await
        .expect("should succeed");

        assert_eq!(result, "offline output");
    }

    #[tokio::test]
    async fn render_via_file_error_propagates() {
        let result = with_daemon_or_offline_render(
            "http://127.0.0.1:0",
            true,
            |_client| async { Ok("daemon output".to_string()) },
            || anyhow::bail!("render file error"),
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("render file error"));
    }

    // ── Structural gate ───────────────────────────────────────────────────────

    /// The fallback protocol (connect → warn → file) is defined once in
    /// `daemon.rs`.  `campaign_commands.rs` must delegate to
    /// `with_daemon_or_offline_render` rather than re-implementing the
    /// connect/warn/fallback branch inline.
    ///
    /// This test is a compile-time structural assertion: if either forbidden
    /// string appears in the module source, the test binary carries it and the
    /// assertion trips at runtime.
    #[test]
    fn campaign_commands_does_not_duplicate_fallback_protocol() {
        let src = include_str!("campaign_commands.rs");
        assert!(
            !src.contains("FoundryClient::connect"),
            "campaign_commands.rs must not call FoundryClient::connect directly; \
            route through with_daemon_or_offline_render (daemon.rs) instead"
        );
        assert!(
            !src.contains("daemon not reachable"),
            "campaign_commands.rs must not contain the fallback warning string; \
            it is owned exclusively by daemon.rs"
        );
    }
}
