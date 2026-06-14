//! `WriteOpsDigest` — terminal block of the ops-digest formation.
//!
//! Sinks on `OpsSummaryCompleted`. Atomically writes the markdown body to
//! `{ops_digests_dir}/{YYYY-MM-DD}.md` (date in local time), then advances
//! the watermark atomically so a subsequent run picks up only newer events.
//! On a dry-run firing neither the file nor the watermark is written — the
//! chain still terminates cleanly with `digest_path = None`.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{OpsDigestCompletedPayload, OpsSummaryCompletedPayload};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::throttle::Throttle;

use super::emit_result;

/// Writes the agent-composed ops digest to disk, advances the watermark, and
/// emits the formation's terminal event.
pub struct WriteOpsDigest {
    digests_dir: PathBuf,
    watermark_path: PathBuf,
}

impl WriteOpsDigest {
    pub fn new<P: Into<PathBuf>, W: Into<PathBuf>>(digests_dir: P, watermark_path: W) -> Self {
        Self {
            digests_dir: digests_dir.into(),
            watermark_path: watermark_path.into(),
        }
    }
}

impl TaskBlock for WriteOpsDigest {
    task_block_meta! {
        name: "Write Ops Digest",
        kind: Observer,
        sinks_on: [OpsSummaryCompleted],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let p = parse_payload!(trigger, OpsSummaryCompletedPayload);
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let digests_dir = self.digests_dir.clone();
        let watermark_path = self.watermark_path.clone();

        Box::pin(async move { write(&project, throttle, &p, &digests_dir, &watermark_path) })
    }
}

fn write(
    project: &str,
    throttle: Throttle,
    composed: &OpsSummaryCompletedPayload,
    digests_dir: &Path,
    watermark_path: &Path,
) -> anyhow::Result<TaskBlockResult> {
    let (date, intended_path) = super::today_dated_path(digests_dir);
    let rendered = render_full_document(&date, composed);

    if !throttle.permits_mutation() {
        tracing::info!(
            path = %intended_path.display(),
            bytes = rendered.len(),
            "dry-run: skipping ops-digest file write",
        );
        let payload = OpsDigestCompletedPayload {
            success: true,
            skipped: false,
            digest_path: None,
            event_count: composed.event_count,
        };
        return emit_result(
            format!("dry-run: ops digest not written to {}", intended_path.display()),
            EventType::OpsDigestCompleted,
            project,
            throttle,
            &payload,
        );
    }

    match super::write_atomic(digests_dir, &intended_path, &rendered) {
        Ok(()) => {
            tracing::info!(
                path = %intended_path.display(),
                bytes = rendered.len(),
                "ops digest written",
            );
            // Advance the watermark so the next run doesn't re-process events.
            if let Some(wm) = &composed.new_watermark
                && let Err(e) = write_watermark(watermark_path, wm)
            {
                tracing::warn!(
                    path = %watermark_path.display(),
                    error = %e,
                    "ops digest written but watermark advance failed",
                );
            }
            let payload = OpsDigestCompletedPayload {
                success: true,
                skipped: false,
                digest_path: Some(intended_path.to_string_lossy().to_string()),
                event_count: composed.event_count,
            };
            emit_result(
                format!("Ops digest written to {}", intended_path.display()),
                EventType::OpsDigestCompleted,
                project,
                throttle,
                &payload,
            )
        }
        Err(e) => {
            tracing::warn!(
                path = %intended_path.display(),
                error = %e,
                "ops digest write failed",
            );
            let payload = OpsDigestCompletedPayload {
                success: false,
                skipped: false,
                digest_path: None,
                event_count: composed.event_count,
            };
            emit_result(
                format!("failed to write ops digest: {e}"),
                EventType::OpsDigestCompleted,
                project,
                throttle,
                &payload,
            )
        }
    }
}

/// Compose the on-disk markdown: a `# Ops Digest — {date}` header, a
/// one-line totals summary, and the agent's body.
fn render_full_document(date: &str, composed: &OpsSummaryCompletedPayload) -> String {
    let mut out = String::with_capacity(composed.markdown.len() + 128);
    writeln!(out, "# Ops Digest — {date}\n").expect("write to String never fails");
    writeln!(
        out,
        "_{count} operational event{plural}._\n",
        count = composed.event_count,
        plural = if composed.event_count == 1 { "" } else { "s" },
    )
    .expect("write to String never fails");
    let body = composed.markdown.trim_end();
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Atomically write a new watermark value.
fn write_watermark(path: &Path, watermark: &str) -> anyhow::Result<()> {
    // Write to a sibling temp file then rename for atomicity.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut tmp = path.to_path_buf();
    tmp.set_extension("watermark.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(watermark.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use foundry_sdk::event::EventType;

    use super::*;

    fn composed(count: u64, body: &str, watermark: Option<&str>) -> OpsSummaryCompletedPayload {
        OpsSummaryCompletedPayload {
            markdown: body.to_string(),
            event_count: count,
            new_watermark: watermark.map(str::to_string),
        }
    }

    fn trigger(payload: &OpsSummaryCompletedPayload, throttle: Throttle) -> Event {
        Event::new(
            EventType::OpsSummaryCompleted,
            "system".to_string(),
            throttle,
            serde_json::to_value(payload).unwrap(),
        )
    }

    fn today_path(dir: &std::path::Path) -> PathBuf {
        super::super::today_dated_path(dir).1
    }

    #[test]
    fn render_full_document_includes_header_count_and_body() {
        let c = composed(7, "## Infrastructure\n- something happened\n", None);
        let rendered = render_full_document("2026-05-29", &c);
        assert!(rendered.starts_with("# Ops Digest — 2026-05-29\n"));
        assert!(rendered.contains("7 operational events"));
        assert!(rendered.contains("## Infrastructure"));
        assert!(rendered.ends_with('\n'), "always end with a newline");
    }

    #[test]
    fn render_singular_count_uses_singular_noun() {
        let c = composed(1, "body", None);
        let rendered = render_full_document("2026-05-29", &c);
        assert!(rendered.contains("1 operational event."));
        assert!(!rendered.contains("1 operational events"));
    }

    #[tokio::test]
    async fn dry_run_skips_file_and_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let wm_path = dir.path().join("ops-digest.watermark");
        let block = WriteOpsDigest::new(dir.path(), &wm_path);
        let payload = composed(3, "## Infrastructure\nbody\n", Some("2026-05-29T12:00:00Z"));
        let result = block.execute(&trigger(&payload, Throttle::DryRun)).await.unwrap();
        let out: OpsDigestCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(out.success, "dry-run is still a clean chain completion");
        assert!(!out.skipped);
        assert!(out.digest_path.is_none());
        assert!(!today_path(dir.path()).exists(), "dry-run must not write the file");
        assert!(!wm_path.exists(), "dry-run must not write the watermark");
    }

    #[tokio::test]
    async fn full_throttle_writes_dated_file_and_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let wm_path = dir.path().join("ops-digest.watermark");
        let block = WriteOpsDigest::new(dir.path(), &wm_path);
        let payload = composed(5, "## Infrastructure\n- ci failed\n", Some("2026-05-29T15:00:00Z"));
        let result = block.execute(&trigger(&payload, Throttle::Full)).await.unwrap();
        let out: OpsDigestCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(out.success);
        assert!(!out.skipped);
        let expected = today_path(dir.path());
        assert_eq!(out.digest_path.as_deref(), Some(expected.to_string_lossy().as_ref()));
        let contents = std::fs::read_to_string(&expected).unwrap();
        assert!(contents.starts_with("# Ops Digest — "));
        assert!(contents.contains("5 operational events"));
        // Watermark should be written.
        let wm = std::fs::read_to_string(&wm_path).unwrap();
        assert_eq!(wm.trim(), "2026-05-29T15:00:00Z");
    }

    #[tokio::test]
    async fn unwritable_dir_emits_failure() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("blocker");
        std::fs::write(&parent, b"i am a file").unwrap();
        let unwritable = parent.join("nested-dir");
        let wm_path = dir.path().join("ops-digest.watermark");
        let block = WriteOpsDigest::new(&unwritable, &wm_path);
        let payload = composed(1, "body", None);
        let result = block.execute(&trigger(&payload, Throttle::Full)).await.unwrap();
        let out: OpsDigestCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(!out.success);
        assert!(out.digest_path.is_none());
        assert!(result.summary.contains("failed to write"));
    }
}
