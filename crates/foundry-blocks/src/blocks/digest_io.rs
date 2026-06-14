//! Shared I/O primitives for the three digest-writing blocks.
//!
//! `write_atomic` and `today_dated_path` were copy-pasted across
//! `write_commit_digest`, `write_triage_digest`, and `write_ops_digest`.
//! This module provides a single authoritative implementation.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::Local;

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
}
