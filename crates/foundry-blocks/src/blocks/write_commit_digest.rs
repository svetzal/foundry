//! `WriteCommitDigest` — terminal block of the daily commit-digest formation.
//!
//! Sinks on `CommitSummaryCompleted`. Atomically writes the markdown body to
//! `{digests_dir}/{YYYY-MM-DD}.md` (date in local time) and emits
//! `CommitDigestCompleted`. On a dry-run firing the file is not written —
//! the chain still terminates cleanly with `digest_path = None`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{CommitDigestCompletedPayload, CommitSummaryCompletedPayload};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::throttle::Throttle;

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
        sinks_on: [CommitSummaryCompleted],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let p = parse_payload!(trigger, CommitSummaryCompletedPayload);
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let digests_dir = self.digests_dir.clone();

        Box::pin(async move { write(&project, throttle, &p, &digests_dir) })
    }
}

fn write(
    project: &str,
    throttle: Throttle,
    composed: &CommitSummaryCompletedPayload,
    digests_dir: &Path,
) -> anyhow::Result<TaskBlockResult> {
    let (date, _) = super::today_dated_path(digests_dir);
    let rendered = render_full_document(&date, composed);
    super::write_digest_and_emit(
        "commit",
        EventType::CommitDigestCompleted,
        project,
        throttle,
        digests_dir,
        &rendered,
        |success, digest_path| CommitDigestCompletedPayload {
            success,
            digest_path,
            project_count: composed.project_count,
            total_commits: composed.total_commits,
        },
        || {},
    )
}

/// Compose the on-disk markdown: a `# Commit Digest — {date}` header, a
/// one-line totals summary, and the agent's body.
fn render_full_document(date: &str, composed: &CommitSummaryCompletedPayload) -> String {
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

#[cfg(test)]
mod tests {
    use foundry_sdk::event::EventType;

    use super::*;

    fn composed(total: u64, projects: u64, body: &str) -> CommitSummaryCompletedPayload {
        CommitSummaryCompletedPayload {
            markdown: body.to_string(),
            project_count: projects,
            total_commits: total,
        }
    }

    fn trigger(payload: &CommitSummaryCompletedPayload, throttle: Throttle) -> Event {
        Event::new(
            EventType::CommitSummaryCompleted,
            "system".to_string(),
            throttle,
            serde_json::to_value(payload).unwrap(),
        )
    }

    fn today_path(dir: &Path) -> PathBuf {
        super::super::today_dated_path(dir).1
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
