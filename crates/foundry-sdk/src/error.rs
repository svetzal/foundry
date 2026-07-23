//! Typed domain errors for the Foundry SDK.
//!
//! These replace opaque `anyhow::Error` at the boundaries where the caller
//! can and should match on failure modes:
//!
//! - [`PayloadError`] — event payload serialization/deserialization
//! - [`StoreError`] — file-backed store reads and writes (registry, sentinels, gates, …)
//!
//! All variants implement `std::error::Error + Send + Sync + 'static`, so they
//! coerce into `anyhow::Error` via `?` without call-site changes in contexts
//! that return `anyhow::Result`.

use std::path::PathBuf;

use crate::event::EventType;

/// Errors that arise when serializing or deserializing an event payload.
#[derive(Debug, thiserror::Error)]
pub enum PayloadError {
    /// The payload JSON could not be deserialized into the requested type.
    #[error("failed to deserialize payload for event {event_type:?}: {source}")]
    Deserialize {
        /// The event type whose payload was being parsed.
        event_type: EventType,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// A typed payload struct could not be serialized to JSON.
    #[error("failed to serialize payload: {source}")]
    Serialize {
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },
}

/// Errors that arise when loading or saving a file-backed store.
///
/// Covers `Registry`, `SentinelStore`, `AgentConfigStore`, gates files, and
/// the supply-chain allow-list — all of which perform the same `read → parse`
/// and `serialize → write` operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The file does not exist at the expected path.
    #[error("store file not found: {path}")]
    NotFound {
        /// The path that was expected to exist.
        path: PathBuf,
    },

    /// An I/O error occurred while reading or writing the file.
    #[error("I/O error for store file {path}: {source}")]
    Io {
        /// The path being operated on.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The file exists but contains malformed JSON.
    #[error("malformed JSON in store file {path}: {source}")]
    Parse {
        /// The path whose contents failed to parse.
        path: PathBuf,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },
}

/// Acquire a read guard on `lock`, returning a descriptive error instead of
/// panicking when the lock is poisoned.
///
/// Lock poisoning occurs when a writer panicked while holding the write
/// guard. `foundryd` is long-lived state: an unhandled panic anywhere in the
/// process brings the whole daemon down and drops every other in-flight
/// request. A poisoned lock on one piece of state must not be allowed to
/// escalate into that — callers use this helper (via `?` or a `map_err` into
/// a typed status) so the fault is propagated to the one request that hit it,
/// and every other request keeps being served.
///
/// `what` names the lock in the error message (e.g. `"registry"`,
/// `"campaign store"`) so the fault is identifiable in logs.
pub fn read_lock<'a, T>(
    lock: &'a std::sync::RwLock<T>,
    what: &str,
) -> anyhow::Result<std::sync::RwLockReadGuard<'a, T>> {
    lock.read().map_err(|_| {
        anyhow::anyhow!(
            "{what} lock poisoned: a prior writer panicked while holding the write lock"
        )
    })
}

/// Acquire a write guard on `lock`, returning a descriptive error instead of
/// panicking when the lock is poisoned. See [`read_lock`] for the rationale.
pub fn write_lock<'a, T>(
    lock: &'a std::sync::RwLock<T>,
    what: &str,
) -> anyhow::Result<std::sync::RwLockWriteGuard<'a, T>> {
    lock.write().map_err(|_| {
        anyhow::anyhow!(
            "{what} lock poisoned: a prior writer panicked while holding the write lock"
        )
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;

    #[test]
    fn payload_error_deserialize_preserves_source() {
        use crate::payload::PreflightCompletedPayload;

        let bad_event = crate::event::Event::new(
            EventType::PreflightCompleted,
            "proj".to_string(),
            crate::throttle::Throttle::Full,
            serde_json::json!({"not_a_valid_preflight": true}),
        );
        let err = bad_event.parse_payload::<PreflightCompletedPayload>().unwrap_err();
        assert!(
            matches!(err, PayloadError::Deserialize { .. }),
            "expected Deserialize variant, got {err:?}"
        );
        // source must be populated so context chains survive
        assert!(
            err.source().is_some(),
            "PayloadError::Deserialize must carry a source serde_json::Error"
        );
    }

    #[test]
    fn store_error_not_found_displays_path() {
        let err = StoreError::NotFound {
            path: PathBuf::from("/tmp/does-not-exist.json"),
        };
        let msg = err.to_string();
        assert!(msg.contains("does-not-exist.json"), "display: {msg}");
    }

    #[test]
    fn store_error_parse_preserves_source() {
        let parse_err: serde_json::Error =
            serde_json::from_str::<serde_json::Value>("{ not json").unwrap_err();
        let err = StoreError::Parse {
            path: PathBuf::from("/tmp/bad.json"),
            source: parse_err,
        };
        assert!(
            err.source().is_some(),
            "StoreError::Parse must carry a source serde_json::Error"
        );
    }

    #[test]
    fn read_lock_returns_guard_when_healthy() {
        let lock = std::sync::RwLock::new(42);
        let guard = read_lock(&lock, "test").unwrap();
        assert_eq!(*guard, 42);
    }

    #[test]
    fn write_lock_returns_guard_when_healthy() {
        let lock = std::sync::RwLock::new(42);
        {
            let mut guard = write_lock(&lock, "test").unwrap();
            *guard = 7;
        }
        assert_eq!(*lock.read().unwrap(), 7);
    }

    #[test]
    fn read_lock_returns_err_when_poisoned() {
        let lock = std::sync::Arc::new(std::sync::RwLock::new(0));
        let l2 = std::sync::Arc::clone(&lock);
        let _ = std::thread::spawn(move || {
            let _guard = l2.write().unwrap();
            panic!("intentional poison");
        })
        .join();

        let err = read_lock(&lock, "widget").unwrap_err();
        assert!(err.to_string().contains("widget lock poisoned"));
    }

    #[test]
    fn write_lock_returns_err_when_poisoned() {
        let lock = std::sync::Arc::new(std::sync::RwLock::new(0));
        let l2 = std::sync::Arc::clone(&lock);
        let _ = std::thread::spawn(move || {
            let _guard = l2.write().unwrap();
            panic!("intentional poison");
        })
        .join();

        let err = write_lock(&lock, "widget").unwrap_err();
        assert!(err.to_string().contains("widget lock poisoned"));
    }
}
