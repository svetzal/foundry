//! `WriteCommitDigest` — terminal block of the daily commit-digest formation.
//!
//! Sinks on `CommitSummaryComposed`. Atomically writes the markdown body to
//! `{digests_dir}/{YYYY-MM-DD}.md` (date in local time) and emits
//! `CommitDigestCompleted`. On a dry-run firing the file is not written —
//! the chain still terminates cleanly with `digest_path = None`.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use chrono::Local;
use foundry_core::event::{Event, EventType};
use foundry_core::payload::{CommitDigestCompletedPayload, CommitSummaryComposedPayload};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::throttle::Throttle;

use super::emit_result;

/// Writes the agent-composed digest to disk and emits the formation's
/// terminal event.
pub struct WriteCommitDigest {
    digests_dir: PathBuf,
}

impl WriteCommitDigest {
    pub fn new<P: Into<PathBuf>>(digests_dir: P) -> Self {
        Self {
            digests_dir: digests_dir.into(),
        }
    }
}

impl TaskBlock for WriteCommitDigest {
    task_block_meta! {
        name: "Write Commit Digest",
        kind: Observer,
        sinks_on: [CommitSummaryComposed],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let p = parse_payload!(trigger, CommitSummaryComposedPayload);
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let digests_dir = self.digests_dir.clone();

        Box::pin(async move { write(&project, throttle, &p, &digests_dir) })
    }
}

fn write(
    project: &str,
    throttle: Throttle,
    composed: &CommitSummaryComposedPayload,
    digests_dir: &Path,
) -> anyhow::Result<TaskBlockResult> {
    let date = Local::now().format("%Y-%m-%d").to_string();
    let rendered = render_full_document(&date, composed);
    let intended_path = digests_dir.join(format!("{date}.md"));

    if !throttle.permits_mutation() {
        tracing::info!(
            path = %intended_path.display(),
            bytes = rendered.len(),
            "dry-run: skipping commit-digest file write",
        );
        let payload = CommitDigestCompletedPayload {
            success: true,
            digest_path: None,
            project_count: composed.project_count,
            total_commits: composed.total_commits,
        };
        return emit_result(
            format!("dry-run: digest not written to {}", intended_path.display()),
            EventType::CommitDigestCompleted,
            project,
            throttle,
            &payload,
        );
    }

    match write_atomic(digests_dir, &intended_path, &rendered) {
        Ok(()) => {
            tracing::info!(
                path = %intended_path.display(),
                bytes = rendered.len(),
                "commit digest written",
            );
            let payload = CommitDigestCompletedPayload {
                success: true,
                digest_path: Some(intended_path.to_string_lossy().to_string()),
                project_count: composed.project_count,
                total_commits: composed.total_commits,
            };
            emit_result(
                format!("Digest written to {}", intended_path.display()),
                EventType::CommitDigestCompleted,
                project,
                throttle,
                &payload,
            )
        }
        Err(e) => {
            tracing::warn!(
                path = %intended_path.display(),
                error = %e,
                "commit digest write failed",
            );
            let payload = CommitDigestCompletedPayload {
                success: false,
                digest_path: None,
                project_count: composed.project_count,
                total_commits: composed.total_commits,
            };
            emit_result(
                format!("failed to write digest: {e}"),
                EventType::CommitDigestCompleted,
                project,
                throttle,
                &payload,
            )
        }
    }
}

/// Compose the on-disk markdown: a `# Commit Digest — {date}` header, a
/// one-line totals summary, and the agent's body.
fn render_full_document(date: &str, composed: &CommitSummaryComposedPayload) -> String {
    let mut out = String::with_capacity(composed.markdown.len() + 128);
    writeln!(out, "# Commit Digest — {date}\n").expect("write to String never fails");
    writeln!(
        out,
        "_{commits} commit{commits_plural} across {projects} project{projects_plural}._\n",
        commits = composed.total_commits,
        commits_plural = if composed.total_commits == 1 { "" } else { "s" },
        projects = composed.project_count,
        projects_plural = if composed.project_count == 1 { "" } else { "s" },
    )
    .expect("write to String never fails");
    let body = composed.markdown.trim_end();
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Write `content` to `target` atomically: write to a sibling tempfile, then
/// rename. Creates `dir` if it does not exist.
fn write_atomic(dir: &Path, target: &Path, content: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    // `tempfile` is already a workspace dev-dep; for the production path here
    // we hand-roll the temp+rename pattern to keep tempfile in dev-deps only.
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

#[cfg(test)]
mod tests {
    use foundry_core::event::EventType;

    use super::*;

    fn composed(total: u64, projects: u64, body: &str) -> CommitSummaryComposedPayload {
        CommitSummaryComposedPayload {
            markdown: body.to_string(),
            project_count: projects,
            total_commits: total,
        }
    }

    fn trigger(payload: &CommitSummaryComposedPayload, throttle: Throttle) -> Event {
        Event::new(
            EventType::CommitSummaryComposed,
            "system".to_string(),
            throttle,
            serde_json::to_value(payload).unwrap(),
        )
    }

    fn today_path(dir: &Path) -> PathBuf {
        dir.join(format!("{}.md", Local::now().format("%Y-%m-%d")))
    }

    #[test]
    fn render_full_document_includes_header_totals_and_body() {
        let composed = composed(3, 2, "## Alpha\n- did things (abc1234)\n");
        let rendered = render_full_document("2026-05-28", &composed);
        assert!(rendered.starts_with("# Commit Digest — 2026-05-28\n"));
        assert!(rendered.contains("3 commits"));
        assert!(rendered.contains("2 projects"));
        assert!(rendered.contains("## Alpha"));
        assert!(rendered.ends_with('\n'), "always end with a newline");
    }

    #[test]
    fn render_singular_totals_use_singular_nouns() {
        let composed = composed(1, 1, "body");
        let rendered = render_full_document("2026-05-28", &composed);
        assert!(rendered.contains("1 commit "));
        assert!(rendered.contains("1 project."));
    }

    #[tokio::test]
    async fn full_throttle_writes_dated_file_and_emits_path() {
        let dir = tempfile::tempdir().unwrap();
        let block = WriteCommitDigest::new(dir.path());
        let payload = composed(2, 1, "## Alpha\n- one (aaa)\n- two (bbb)\n");
        let result = block.execute(&trigger(&payload, Throttle::Full)).await.unwrap();
        let out: CommitDigestCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(out.success);
        let expected = today_path(dir.path());
        assert_eq!(out.digest_path.as_deref(), Some(expected.to_string_lossy().as_ref()));
        let contents = std::fs::read_to_string(&expected).unwrap();
        assert!(contents.starts_with("# Commit Digest — "));
        assert!(contents.contains("## Alpha"));
        assert!(contents.contains("2 commits across 1 project"));
    }

    #[tokio::test]
    async fn dry_run_skips_file_write_but_emits_completion() {
        let dir = tempfile::tempdir().unwrap();
        let block = WriteCommitDigest::new(dir.path());
        let payload = composed(2, 1, "body");
        let result = block.execute(&trigger(&payload, Throttle::DryRun)).await.unwrap();
        let out: CommitDigestCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(out.success, "dry-run is still a clean chain completion");
        assert!(out.digest_path.is_none());
        assert!(!today_path(dir.path()).exists(), "dry-run must not write the file");
    }

    #[tokio::test]
    async fn unwritable_directory_emits_failure_completion() {
        let dir = tempfile::tempdir().unwrap();
        // Place the digests dir at a path whose parent exists but is a file
        // (cannot become a directory) — std::fs::create_dir_all will fail.
        let parent = dir.path().join("blocker");
        std::fs::write(&parent, b"i am a file").unwrap();
        let unwritable = parent.join("nested-dir");
        let block = WriteCommitDigest::new(&unwritable);
        let payload = composed(1, 1, "body");
        let result = block.execute(&trigger(&payload, Throttle::Full)).await.unwrap();
        let out: CommitDigestCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(!out.success);
        assert!(out.digest_path.is_none());
        assert!(result.summary.contains("failed to write"));
    }
}
