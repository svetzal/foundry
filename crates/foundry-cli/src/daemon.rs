//! Shared daemon-or-offline fallback helper.
//!
//! The fallback protocol — try gRPC, warn on unreachable, fall back to
//! direct file mutation, print success once — is one decision that applies
//! to every registry and sentinel mutation command.  This module is the
//! single home for that decision.  See [`with_daemon_or_offline`].

use anyhow::Result;

use crate::proto::foundry_client::FoundryClient;

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
}
