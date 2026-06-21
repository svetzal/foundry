//! `WriteTriageDigest` — terminal block of the post-maintenance triage formation.
//!
//! Sinks on `MaintenanceTriageCompleted`. Renders the classified verdicts and
//! infra incidents into a structured markdown digest and writes it atomically
//! to `{triage_dir}/{YYYY-MM-DD}.md`. Dry-run skips the write but runs the
//! full render.
//!
//! This block emits no further events — it is the final step in the triage
//! chain.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::MaintenanceTriageCompletedPayload;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::throttle::Throttle;
use foundry_sdk::triage::{Decision, FailureVerdict};

/// Writes the classified triage digest to disk and emits the formation's
/// terminal event, `MaintenanceTriageDigestWritten`, carrying `digest_path`.
///
/// The terminal event is a *distinct* type from the `MaintenanceTriageCompleted`
/// this block sinks on — emitting the same type it consumes would re-trigger the
/// block in an unbounded loop (the defect that ran the event log to ~56 GB).
///
/// The block is a no-op (returns success) when `skipped` is true in the
/// incoming payload.
pub struct WriteTriageDigest {
    triage_dir: PathBuf,
}

impl WriteTriageDigest {
    pub fn new<P: Into<PathBuf>>(triage_dir: P) -> Self {
        Self {
            triage_dir: triage_dir.into(),
        }
    }
}

impl TaskBlock for WriteTriageDigest {
    task_block_meta! {
        name: "Write Triage Digest",
        kind: Observer,
        sinks_on: [MaintenanceTriageCompleted],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        digest_block_execute!(self, trigger, MaintenanceTriageCompletedPayload, [triage_dir])
    }
}

fn write(
    project: &str,
    throttle: Throttle,
    payload: &MaintenanceTriageCompletedPayload,
    triage_dir: &Path,
) -> anyhow::Result<TaskBlockResult> {
    // When the triage itself was skipped (no failures), nothing to write.
    if payload.skipped {
        tracing::debug!("triage digest: payload skipped — no file to write");
        return Ok(TaskBlockResult::success(
            "triage digest: no failures — nothing to write".to_string(),
            vec![],
        ));
    }

    let (date, _) = super::today_dated_path(triage_dir);
    let rendered = render_document(&date, payload);
    super::write_digest_and_emit(
        "triage",
        EventType::MaintenanceTriageDigestWritten,
        project,
        throttle,
        triage_dir,
        &rendered,
        |success, digest_path| MaintenanceTriageCompletedPayload {
            success,
            digest_path,
            ..payload.clone()
        },
        || {},
    )
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn render_document(date: &str, payload: &MaintenanceTriageCompletedPayload) -> String {
    let mut out = String::with_capacity(2048);

    writeln!(out, "# Maintenance Triage — {date}\n").expect("write to String never fails");

    // Summary table
    writeln!(out, "| Category | Count |").expect("write to String never fails");
    writeln!(out, "|----------|-------|").expect("write to String never fails");
    writeln!(out, "| Total failures | {} |", payload.total_failures)
        .expect("write to String never fails");
    writeln!(out, "| Infra-suppressed | {} |", payload.suppressed_count)
        .expect("write to String never fails");
    writeln!(out, "| Auto-fixable | {} |", payload.auto_fixable_count)
        .expect("write to String never fails");
    writeln!(out, "| Policy calls | {} |", payload.policy_count)
        .expect("write to String never fails");
    writeln!(out, "| Needs investigation | {} |", payload.investigation_count)
        .expect("write to String never fails");
    writeln!(out, "| Escalations | {} |\n", payload.escalation_count)
        .expect("write to String never fails");

    // Escalations first — highest priority
    let escalations: Vec<&FailureVerdict> =
        payload.verdicts.iter().filter(|v| v.decision == Decision::Escalate).collect();
    if !escalations.is_empty() {
        writeln!(out, "## Deadlock Escalations\n").expect("write to String never fails");
        for v in &escalations {
            writeln!(
                out,
                "- **{}** / `{}` — {}",
                v.project,
                v.gate,
                v.evidence.lines().next().unwrap_or("")
            )
            .expect("write to String never fails");
        }
        writeln!(out).expect("write to String never fails");
    }

    // Auto-fixable proposals
    let auto_fixable: Vec<&FailureVerdict> = payload
        .verdicts
        .iter()
        .filter(|v| v.decision == Decision::AutoFixable)
        .collect();
    if !auto_fixable.is_empty() {
        writeln!(out, "## Auto-fixable Proposals\n").expect("write to String never fails");
        for v in &auto_fixable {
            let cmd = v.proposed_command.as_deref().unwrap_or("(no fix command available)");
            writeln!(out, "- **{}** / `{}` — run `{cmd}`", v.project, v.gate)
                .expect("write to String never fails");
        }
        writeln!(out).expect("write to String never fails");
    }

    // Infra-suppressed incidents
    if !payload.infra_incidents.is_empty() {
        writeln!(out, "## Infra-suppressed (Correlated)\n").expect("write to String never fails");
        for incident in &payload.infra_incidents {
            let projects = incident.projects.join(", ");
            writeln!(
                out,
                "- **{}** — projects: {} — sample: `{}`",
                incident.signature,
                projects,
                incident.sample_evidence.lines().next().unwrap_or("")
            )
            .expect("write to String never fails");
        }
        writeln!(out).expect("write to String never fails");
    }

    // Policy calls
    let policy: Vec<&FailureVerdict> =
        payload.verdicts.iter().filter(|v| v.decision == Decision::PolicyCall).collect();
    if !policy.is_empty() {
        writeln!(out, "## Policy Calls\n").expect("write to String never fails");
        for v in &policy {
            writeln!(
                out,
                "- **{}** / `{}` ({}) — {}",
                v.project,
                v.gate,
                v.class,
                v.evidence.lines().next().unwrap_or("")
            )
            .expect("write to String never fails");
        }
        writeln!(out).expect("write to String never fails");
    }

    // Needs investigation
    let investigation: Vec<&FailureVerdict> = payload
        .verdicts
        .iter()
        .filter(|v| v.decision == Decision::NeedsInvestigation)
        .collect();
    if !investigation.is_empty() {
        writeln!(out, "## Needs Investigation\n").expect("write to String never fails");
        for v in &investigation {
            writeln!(
                out,
                "- **{}** / `{}` ({}) — {}",
                v.project,
                v.gate,
                v.class,
                v.evidence.lines().next().unwrap_or("")
            )
            .expect("write to String never fails");
        }
        writeln!(out).expect("write to String never fails");
    }

    // Benign / reclassified (informational)
    let benign: Vec<&FailureVerdict> = payload
        .verdicts
        .iter()
        .filter(|v| v.decision == Decision::ReclassifyBenign)
        .collect();
    if !benign.is_empty() {
        writeln!(out, "## Reclassified as Benign\n").expect("write to String never fails");
        for v in &benign {
            writeln!(out, "- **{}** / `{}`", v.project, v.gate)
                .expect("write to String never fails");
        }
        writeln!(out).expect("write to String never fails");
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use foundry_sdk::event::EventType;
    use foundry_sdk::triage::{Decision, FailureClass, FailureVerdict, InfraIncident};

    use super::*;

    fn sample_payload(total: u64) -> MaintenanceTriageCompletedPayload {
        MaintenanceTriageCompletedPayload {
            success: true,
            skipped: false,
            digest_path: None,
            verdicts: vec![
                FailureVerdict {
                    project: "alpha".to_string(),
                    gate: "fmt".to_string(),
                    class: FailureClass::FormatAndLintDrift,
                    decision: Decision::AutoFixable,
                    evidence: "rustfmt diff".to_string(),
                    proposed_command: Some("cargo fmt".to_string()),
                },
                FailureVerdict {
                    project: "beta".to_string(),
                    gate: "test".to_string(),
                    class: FailureClass::TestBreakage,
                    decision: Decision::NeedsInvestigation,
                    evidence: "3 test failures".to_string(),
                    proposed_command: None,
                },
            ],
            infra_incidents: vec![InfraIncident {
                signature: "os_error_2".to_string(),
                decision: Decision::SuppressInfra,
                projects: vec!["x".to_string(), "y".to_string(), "z".to_string()],
                sample_evidence: "os error 2: No such file".to_string(),
            }],
            total_failures: total,
            suppressed_count: 3,
            auto_fixable_count: 1,
            policy_count: 0,
            investigation_count: 1,
            escalation_count: 0,
        }
    }

    fn trigger_with(payload: &MaintenanceTriageCompletedPayload, throttle: Throttle) -> Event {
        foundry_sdk::event::Event::new(
            EventType::MaintenanceTriageCompleted,
            "system".to_string(),
            throttle,
            serde_json::to_value(payload).unwrap(),
        )
    }

    fn today_path(dir: &std::path::Path) -> PathBuf {
        super::super::today_dated_path(dir).1
    }

    #[test]
    fn render_document_includes_summary_and_sections() {
        let payload = sample_payload(5);
        let rendered = render_document("2026-06-12", &payload);
        assert!(rendered.starts_with("# Maintenance Triage — 2026-06-12"));
        assert!(rendered.contains("## Auto-fixable Proposals"));
        assert!(rendered.contains("## Infra-suppressed (Correlated)"));
        assert!(rendered.contains("## Needs Investigation"));
        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("cargo fmt"));
        assert!(rendered.contains("os_error_2"));
    }

    #[test]
    fn render_document_includes_escalations_section_when_present() {
        let mut payload = sample_payload(3);
        payload.verdicts.push(FailureVerdict {
            project: "gamma".to_string(),
            gate: "test".to_string(),
            class: FailureClass::ChronicDeadlock,
            decision: Decision::Escalate,
            evidence: "N>=3 consecutive failures".to_string(),
            proposed_command: None,
        });
        payload.escalation_count = 1;
        let rendered = render_document("2026-06-12", &payload);
        assert!(rendered.contains("## Deadlock Escalations"));
        assert!(rendered.contains("gamma"));
    }

    #[tokio::test]
    async fn dry_run_skips_file_write() {
        let dir = tempfile::tempdir().unwrap();
        let block = WriteTriageDigest::new(dir.path());
        let payload = sample_payload(5);
        let result = block.execute(&trigger_with(&payload, Throttle::DryRun)).await.unwrap();
        assert!(result.success);
        assert!(!today_path(dir.path()).exists(), "dry-run must not write the file");
    }

    #[tokio::test]
    async fn full_throttle_writes_dated_file() {
        let dir = tempfile::tempdir().unwrap();
        let block = WriteTriageDigest::new(dir.path());
        let payload = sample_payload(5);
        let result = block.execute(&trigger_with(&payload, Throttle::Full)).await.unwrap();
        assert!(result.success);
        let expected = today_path(dir.path());
        assert!(expected.exists(), "file should have been written");

        let contents = std::fs::read_to_string(&expected).unwrap();
        assert!(contents.starts_with("# Maintenance Triage — "));
        assert!(contents.contains("Auto-fixable"));

        // Check the emitted event carries the digest_path.
        assert_eq!(result.events.len(), 1);
        // Loop-prevention: the terminal event MUST be a distinct type from the
        // one this block sinks on, or it would re-trigger itself unbounded.
        assert_eq!(result.events[0].event_type, EventType::MaintenanceTriageDigestWritten);
        assert_ne!(result.events[0].event_type, EventType::MaintenanceTriageCompleted);
        let out: MaintenanceTriageCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(out.digest_path.is_some());
        assert!(
            std::path::Path::new(&out.digest_path.unwrap())
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        );
    }

    #[tokio::test]
    async fn skipped_payload_returns_success_with_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let block = WriteTriageDigest::new(dir.path());
        let payload = MaintenanceTriageCompletedPayload {
            success: true,
            skipped: true,
            ..MaintenanceTriageCompletedPayload::default()
        };
        let result = block.execute(&trigger_with(&payload, Throttle::Full)).await.unwrap();
        assert!(result.success);
        assert!(!today_path(dir.path()).exists());
        assert!(result.events.is_empty(), "skipped case emits no new event");
    }

    #[tokio::test]
    async fn unwritable_dir_emits_failure_event() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"i am a file").unwrap();
        let unwritable = blocker.join("nested");
        let block = WriteTriageDigest::new(&unwritable);
        let payload = sample_payload(5);
        let result = block.execute(&trigger_with(&payload, Throttle::Full)).await.unwrap();
        let out: MaintenanceTriageCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(!out.success);
        assert!(out.digest_path.is_none());
        assert!(result.summary.contains("failed to write"));
    }
}
