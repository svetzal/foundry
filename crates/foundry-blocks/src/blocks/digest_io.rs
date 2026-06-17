//! Shared I/O primitives for the digest-writing blocks.
//!
//! `write_atomic`, `today_dated_path`, and `write_digest_and_emit` were
//! copy-pasted across the four digest-writing blocks. This module provides
//! single authoritative implementations.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::Local;
use foundry_sdk::event::EventType;
use foundry_sdk::task_block::TaskBlockResult;
use foundry_sdk::throttle::Throttle;

/// Atomically write `content` to `target`: create `dir` if absent, write to a
/// sibling `.{name}.tmp` file, fsync, then rename into place.
pub(crate) fn write_atomic(dir: &Path, target: &Path, content: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut tmp = target.to_path_buf();
    let file_name = target.file_name().and_then(|n| n.to_str()).unwrap_or("digest.md");
    tmp.set_file_name(format!(".{file_name}.tmp"));

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, target)?;
    Ok(())
}

/// Return the today-date string and the corresponding `{dir}/{YYYY-MM-DD}.md` path.
pub(crate) fn today_dated_path(dir: &Path) -> (String, PathBuf) {
    let date = Local::now().format("%Y-%m-%d").to_string();
    let path = dir.join(format!("{date}.md"));
    (date, path)
}

#[allow(clippy::too_many_arguments)]
/// Shared orchestration skeleton for all four digest writers.
///
/// Handles the dry-run guard, atomic write, tracing, and terminal event
/// emission — the only variation across callers is the digest noun, event
/// type, and how the typed result payload is constructed.
///
/// * `noun` — short label used in log and summary messages (e.g. `"commit"`).
/// * `make_payload` — called once with `(success, digest_path)` to build the
///   serialisable terminal payload.
/// * `on_committed` — called exactly once after a successful `write_atomic`,
///   before `emit_result`. Pass `|| {}` when there is no post-commit work.
///   `WriteOpsDigest` uses this hook to advance the watermark.
pub(crate) fn write_digest_and_emit<P, F, C>(
    noun: &str,
    event_type: EventType,
    project: &str,
    throttle: Throttle,
    dir: &Path,
    rendered: &str,
    make_payload: F,
    on_committed: C,
) -> anyhow::Result<TaskBlockResult>
where
    P: serde::Serialize,
    F: Fn(bool, Option<String>) -> P,
    C: FnOnce(),
{
    let (_, intended_path) = today_dated_path(dir);

    if !throttle.permits_mutation() {
        tracing::info!(
            path = %intended_path.display(),
            bytes = rendered.len(),
            "dry-run: skipping {noun} digest write",
        );
        return super::emit_result(
            format!("dry-run: {noun} digest not written to {}", intended_path.display()),
            event_type,
            project,
            throttle,
            &make_payload(true, None),
        );
    }

    match write_atomic(dir, &intended_path, rendered) {
        Ok(()) => {
            tracing::info!(
                path = %intended_path.display(),
                bytes = rendered.len(),
                "{noun} digest written",
            );
            on_committed();
            super::emit_result(
                format!("{noun} digest written to {}", intended_path.display()),
                event_type,
                project,
                throttle,
                &make_payload(true, Some(intended_path.to_string_lossy().to_string())),
            )
        }
        Err(e) => {
            tracing::warn!(
                path = %intended_path.display(),
                error = %e,
                "{noun} digest write failed",
            );
            super::emit_result(
                format!("failed to write {noun} digest: {e}"),
                event_type,
                project,
                throttle,
                &make_payload(false, None),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_creates_dir_writes_content_and_leaves_no_tmp() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("sub");
        let target = dir.join("2026-06-14.md");

        write_atomic(&dir, &target, "hello world").unwrap();

        assert!(target.exists(), "target file must exist after write");
        let content = std::fs::read_to_string(&target).unwrap();
        assert_eq!(content, "hello world");

        let tmp = dir.join(".2026-06-14.md.tmp");
        assert!(!tmp.exists(), "tmp file must not remain after rename");
    }

    #[test]
    fn write_atomic_overwrites_existing_target() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().to_path_buf();
        let target = dir.join("digest.md");
        std::fs::write(&target, b"old").unwrap();

        write_atomic(&dir, &target, "new").unwrap();

        let content = std::fs::read_to_string(&target).unwrap();
        assert_eq!(content, "new");
    }

    #[test]
    fn today_dated_path_returns_consistent_date_and_path() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path();
        let (date, path) = today_dated_path(dir);
        assert!(path.ends_with(format!("{date}.md")), "path must use the returned date string");
        assert_eq!(date.len(), 10, "date must be YYYY-MM-DD");
        assert!(date.chars().all(|c| c.is_ascii_digit() || c == '-'));
    }

    #[test]
    fn write_digest_and_emit_full_throttle_writes_file() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path();
        let (date, expected_path) = today_dated_path(dir);
        let rendered = format!("# Test Digest — {date}\n");

        let result = write_digest_and_emit(
            "test",
            foundry_sdk::event::EventType::CommitDigestCompleted,
            "test-project",
            foundry_sdk::throttle::Throttle::Full,
            dir,
            &rendered,
            |success, digest_path| serde_json::json!({"success": success, "path": digest_path}),
            || {},
        )
        .unwrap();

        assert!(result.success);
        assert!(expected_path.exists(), "file must be written on full throttle");
        assert_eq!(std::fs::read_to_string(&expected_path).unwrap(), rendered);
    }

    #[test]
    fn write_digest_and_emit_dry_run_skips_write() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path();
        let (date, expected_path) = today_dated_path(dir);
        let rendered = format!("# Test Digest — {date}\n");

        let result = write_digest_and_emit(
            "test",
            foundry_sdk::event::EventType::CommitDigestCompleted,
            "test-project",
            foundry_sdk::throttle::Throttle::DryRun,
            dir,
            &rendered,
            |success, digest_path| serde_json::json!({"success": success, "path": digest_path}),
            || {},
        )
        .unwrap();

        assert!(result.success, "dry-run must still report success");
        assert!(!expected_path.exists(), "dry-run must not write the file");
    }

    #[test]
    fn write_digest_and_emit_unwritable_dir_emits_failure() {
        let base = tempfile::tempdir().unwrap();
        let blocker = base.path().join("blocker");
        std::fs::write(&blocker, b"i am a file").unwrap();
        let unwritable = blocker.join("nested");

        let result = write_digest_and_emit(
            "test",
            foundry_sdk::event::EventType::CommitDigestCompleted,
            "test-project",
            foundry_sdk::throttle::Throttle::Full,
            &unwritable,
            "content",
            |success, digest_path| serde_json::json!({"success": success, "path": digest_path}),
            || {},
        )
        .unwrap();

        let payload: serde_json::Value = result.events[0].parse_payload().unwrap();
        assert!(!payload["success"].as_bool().unwrap());
        assert!(result.summary.contains("failed to write"));
    }
}
