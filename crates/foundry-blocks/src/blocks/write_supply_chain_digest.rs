//! `WriteSupplyChainDigest` — terminal block of the supply-chain formation.
//!
//! Sinks on `SupplyChainRemediated`. Renders a *deterministic* advisory digest
//! (no agent — CVE identifiers must never be paraphrased or hallucinated) and
//! atomically writes it to `{supply_chain_dir}/{YYYY-MM-DD}.md`. On a dry-run
//! firing neither the file is written nor a path returned; the chain still
//! terminates cleanly.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    ProjectSupplyChainScan, RemediationOutcome, SupplyChainRemediatedPayload,
    SupplyChainScanCompletedPayload,
};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::throttle::Throttle;

/// Writes the rendered supply-chain advisory digest and emits the terminal event.
pub struct WriteSupplyChainDigest {
    supply_chain_dir: PathBuf,
}

impl WriteSupplyChainDigest {
    pub fn new<P: Into<PathBuf>>(supply_chain_dir: P) -> Self {
        Self {
            supply_chain_dir: supply_chain_dir.into(),
        }
    }
}

impl TaskBlock for WriteSupplyChainDigest {
    task_block_meta! {
        name: "Write Supply Chain Digest",
        kind: Observer,
        sinks_on: [SupplyChainRemediated],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let p = parse_payload!(trigger, SupplyChainRemediatedPayload);
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let dir = self.supply_chain_dir.clone();

        Box::pin(async move { write(&project, throttle, &p, &dir) })
    }
}

fn write(
    project: &str,
    throttle: Throttle,
    scan: &SupplyChainRemediatedPayload,
    dir: &Path,
) -> anyhow::Result<TaskBlockResult> {
    let (date, _) = super::today_dated_path(dir);
    let rendered = render_document(&date, scan);
    super::write_digest_and_emit(
        "supply-chain",
        EventType::SupplyChainScanCompleted,
        project,
        throttle,
        dir,
        &rendered,
        |success, digest_path| SupplyChainScanCompletedPayload {
            success,
            skipped: false,
            digest_path,
            project_count: scan.project_count,
            finding_count: scan.finding_count,
        },
        || {},
    )
}

/// Render the deterministic advisory digest markdown.
fn render_document(date: &str, scan: &SupplyChainRemediatedPayload) -> String {
    let mut out = String::with_capacity(512);
    writeln!(out, "# Supply-Chain Advisory Scan — {date}\n").expect("write to String never fails");
    writeln!(
        out,
        "_{findings} live finding{fp} across {affected} of {total} project{tp}._\n",
        findings = scan.finding_count,
        fp = plural(scan.finding_count),
        affected = scan.affected_project_count,
        total = scan.project_count,
        tp = plural(scan.project_count),
    )
    .expect("write to String never fails");

    if scan.finding_count == 0 {
        writeln!(out, "No live supply-chain advisories. ✅\n").expect("write never fails");
    } else {
        // Triage split: which findings carry a fix (mechanically auto-fixable)
        // vs need a human exploitability judgement (no fix available).
        writeln!(
            out,
            "_Triage: **{fixable} auto-fixable** · **{nofix} policy-call**{remediated}._\n",
            fixable = scan.fixable_count,
            nofix = scan.no_fix_count,
            remediated = if scan.remediated_count > 0 {
                format!(" · {} auto-fixed this run", scan.remediated_count)
            } else {
                String::new()
            },
        )
        .expect("write never fails");
    }

    // Remediation — only when the auto-fix engine actually ran (gated on).
    render_remediation(&mut out, &scan.outcomes);

    // Live findings — one section per affected project.
    let affected: Vec<&ProjectSupplyChainScan> =
        scan.projects.iter().filter(|p| !p.findings.is_empty()).collect();
    if !affected.is_empty() {
        writeln!(out, "## Live findings\n").expect("write never fails");
        for proj in affected {
            render_project_findings(&mut out, proj);
        }
    }

    // Lapsed acceptances — allowlist entries whose expiry has passed.
    let lapsed: Vec<(&str, &foundry_sdk::payload::SuppressedFinding)> = scan
        .projects
        .iter()
        .flat_map(|p| {
            p.suppressed
                .iter()
                .filter(|s| s.status == "expired")
                .map(move |s| (p.project.as_str(), s))
        })
        .collect();
    if !lapsed.is_empty() {
        writeln!(out, "## Lapsed acceptances (now live — re-decide)\n").expect("write never fails");
        for (project, s) in lapsed {
            writeln!(
                out,
                "- **{project}** · `{cve}` — acceptance expired {expired}: {reason}",
                cve = s.cve,
                expired = s.expired_on.as_deref().unwrap_or("?"),
                reason = s.reason,
            )
            .expect("write never fails");
        }
        out.push('\n');
    }

    // Active acceptances — recorded for transparency, collapsed to a count + list.
    let active: Vec<(&str, &foundry_sdk::payload::SuppressedFinding)> = scan
        .projects
        .iter()
        .flat_map(|p| {
            p.suppressed
                .iter()
                .filter(|s| s.status == "allowlisted")
                .map(move |s| (p.project.as_str(), s))
        })
        .collect();
    if !active.is_empty() {
        writeln!(out, "## Accepted (allowlisted)\n").expect("write never fails");
        for (project, s) in active {
            writeln!(out, "- **{project}** · `{cve}`: {reason}", cve = s.cve, reason = s.reason)
                .expect("write never fails");
        }
        out.push('\n');
    }

    // Scan errors — tools unavailable / no lockfile. Reported, never failed.
    let errored: Vec<&ProjectSupplyChainScan> =
        scan.projects.iter().filter(|p| p.scan_error.is_some()).collect();
    if !errored.is_empty() {
        writeln!(out, "## Not scanned\n").expect("write never fails");
        for proj in errored {
            writeln!(
                out,
                "- **{project}** ({stack}) — {err}",
                project = proj.project,
                stack = proj.stack,
                err = proj.scan_error.as_deref().unwrap_or("unknown error"),
            )
            .expect("write never fails");
        }
        out.push('\n');
    }

    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Render the remediation section — what the auto-fix engine did this run.
/// Nothing is written when remediation is gated off (no outcomes), so the
/// digest is unchanged on the default, classifier-only path.
fn render_remediation(out: &mut String, outcomes: &[RemediationOutcome]) {
    if outcomes.is_empty() {
        return;
    }

    let applied: Vec<&RemediationOutcome> =
        outcomes.iter().filter(|o| o.status == "applied").collect();
    let reverted: Vec<&RemediationOutcome> =
        outcomes.iter().filter(|o| o.status == "rolled_back").collect();
    let unapplied: Vec<&RemediationOutcome> = outcomes
        .iter()
        .filter(|o| matches!(o.status.as_str(), "apply_failed" | "no_fixer" | "skipped"))
        .collect();

    writeln!(out, "## Remediation\n").expect("write never fails");

    if !applied.is_empty() {
        writeln!(out, "**Auto-fixed (verified by gates, committed):**\n")
            .expect("write never fails");
        for o in &applied {
            writeln!(
                out,
                "- **{project}** · `{cve}` — `{package}` → {fix}",
                project = o.project,
                cve = o.cve,
                package = o.package,
                fix = o.fix_version.as_deref().unwrap_or("?"),
            )
            .expect("write never fails");
        }
        out.push('\n');
    }

    if !reverted.is_empty() {
        writeln!(out, "**Reverted (fix broke the gates):**\n").expect("write never fails");
        for o in &reverted {
            writeln!(
                out,
                "- **{project}** · `{cve}` — `{package}`: {detail}",
                project = o.project,
                cve = o.cve,
                package = o.package,
                detail = o.detail.as_deref().unwrap_or("reverted"),
            )
            .expect("write never fails");
        }
        out.push('\n');
    }

    if !unapplied.is_empty() {
        writeln!(out, "**Not auto-fixed (needs attention):**\n").expect("write never fails");
        for o in &unapplied {
            writeln!(
                out,
                "- **{project}** · `{cve}` — `{package}`: {detail}",
                project = o.project,
                cve = o.cve,
                package = o.package,
                detail = o.detail.as_deref().unwrap_or(o.status.as_str()),
            )
            .expect("write never fails");
        }
        out.push('\n');
    }
}

/// Render one project's live-findings table.
fn render_project_findings(out: &mut String, proj: &ProjectSupplyChainScan) {
    writeln!(out, "### {} ({})\n", proj.project, proj.stack).expect("write never fails");
    writeln!(out, "| Advisory | Package | Severity | Version | Fix |").expect("write never fails");
    writeln!(out, "|----------|---------|----------|---------|-----|").expect("write never fails");
    for f in &proj.findings {
        writeln!(
            out,
            "| `{cve}` | {package} | {severity} | {version} | {fix} |",
            cve = f.cve,
            package = f.package,
            severity = f.severity.as_deref().unwrap_or("—"),
            version = f.version.as_deref().unwrap_or("—"),
            fix = f.fix_version.as_deref().unwrap_or("policy call"),
        )
        .expect("write never fails");
    }
    out.push('\n');
}

const fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::payload::{ProjectSupplyChainScan, SupplyChainFinding, SuppressedFinding};

    use super::*;

    fn finding(cve: &str) -> SupplyChainFinding {
        SupplyChainFinding {
            cve: cve.to_string(),
            package: "vulnerable-pkg".to_string(),
            severity: Some("high".to_string()),
            version: Some("0.1.0".to_string()),
            fix_version: Some("0.2.0".to_string()),
        }
    }

    fn policy_call_finding(cve: &str) -> SupplyChainFinding {
        SupplyChainFinding {
            fix_version: None,
            ..finding(cve)
        }
    }

    fn project_with_findings(
        name: &str,
        findings: Vec<SupplyChainFinding>,
    ) -> ProjectSupplyChainScan {
        ProjectSupplyChainScan {
            project: name.to_string(),
            stack: "typescript".to_string(),
            findings,
            suppressed: vec![],
            scan_error: None,
        }
    }

    fn payload(projects: Vec<ProjectSupplyChainScan>) -> SupplyChainRemediatedPayload {
        let finding_count: u64 = projects.iter().map(|p| p.findings.len() as u64).sum();
        let affected = projects.iter().filter(|p| !p.findings.is_empty()).count() as u64;
        let fixable_count: u64 = projects
            .iter()
            .flat_map(|p| &p.findings)
            .filter(|f| f.fix_version.is_some())
            .count() as u64;
        SupplyChainRemediatedPayload {
            project_count: projects.len() as u64,
            finding_count,
            affected_project_count: affected,
            fixable_count,
            no_fix_count: finding_count - fixable_count,
            remediated_count: 0,
            outcomes: vec![],
            projects,
        }
    }

    fn trigger(p: &SupplyChainRemediatedPayload, throttle: Throttle) -> Event {
        Event::new(
            EventType::SupplyChainRemediated,
            "system".to_string(),
            throttle,
            serde_json::to_value(p).unwrap(),
        )
    }

    fn today_path(dir: &std::path::Path) -> PathBuf {
        super::super::today_dated_path(dir).1
    }

    #[test]
    fn render_clean_scan_says_no_advisories() {
        let p = payload(vec![project_with_findings("alpha", vec![])]);
        let doc = render_document("2026-06-15", &p);
        assert!(doc.starts_with("# Supply-Chain Advisory Scan — 2026-06-15\n"));
        assert!(doc.contains("No live supply-chain advisories"));
        assert!(doc.ends_with('\n'));
    }

    #[test]
    fn render_includes_findings_table_with_cve() {
        let p = payload(vec![project_with_findings(
            "alpha",
            vec![finding("CVE-2026-1234")],
        )]);
        let doc = render_document("2026-06-15", &p);
        assert!(doc.contains("## Live findings"));
        assert!(doc.contains("### alpha (typescript)"));
        assert!(doc.contains("`CVE-2026-1234`"));
        assert!(doc.contains("vulnerable-pkg"));
        assert!(doc.contains("1 live finding "), "singular noun for one finding");
        assert!(
            doc.contains("| Advisory | Package | Severity | Version | Fix |"),
            "fix column header"
        );
        assert!(doc.contains("| 0.2.0 |"), "fixable finding shows its fix version");
    }

    #[test]
    fn render_triage_split_and_policy_call_marker() {
        let p = payload(vec![project_with_findings(
            "alpha",
            vec![finding("CVE-1"), policy_call_finding("CVE-2")],
        )]);
        let doc = render_document("2026-06-15", &p);
        assert!(
            doc.contains("**1 auto-fixable** · **1 policy-call**"),
            "triage line splits fixable from policy-call"
        );
        assert!(doc.contains("| policy call |"), "no-fix finding marked as a policy call");
        assert!(!doc.contains("auto-fixed this run"), "nothing applied by the classifier");
        assert!(!doc.contains("## Remediation"), "no remediation section without outcomes");
    }

    #[test]
    fn render_remediation_section_when_outcomes_present() {
        let mut p = payload(vec![project_with_findings("alpha", vec![finding("CVE-1")])]);
        p.remediated_count = 1;
        p.outcomes = vec![
            RemediationOutcome {
                project: "alpha".to_string(),
                cve: "CVE-1".to_string(),
                package: "vulnerable-pkg".to_string(),
                fix_version: Some("0.2.0".to_string()),
                status: "applied".to_string(),
                detail: Some("verified by gates, committed 0.2.0".to_string()),
            },
            RemediationOutcome {
                project: "beta".to_string(),
                cve: "CVE-2".to_string(),
                package: "pinned-pkg".to_string(),
                fix_version: Some("3.0.0".to_string()),
                status: "apply_failed".to_string(),
                detail: Some("version out of range — override-pin rewrite case".to_string()),
            },
        ];
        let doc = render_document("2026-06-16", &p);
        assert!(doc.contains("## Remediation"));
        assert!(doc.contains("Auto-fixed (verified by gates, committed)"));
        assert!(doc.contains("**alpha** · `CVE-1` — `vulnerable-pkg` → 0.2.0"));
        assert!(doc.contains("Not auto-fixed (needs attention)"));
        assert!(doc.contains("override-pin rewrite case"));
        assert!(doc.contains("1 auto-fixed this run"), "triage line reflects applied count");
    }

    #[test]
    fn render_lapsed_acceptance_section() {
        let mut proj = project_with_findings("alpha", vec![finding("CVE-2026-1234")]);
        proj.suppressed = vec![SuppressedFinding {
            cve: "CVE-2026-1234".to_string(),
            reason: "was dev only".to_string(),
            status: "expired".to_string(),
            expired_on: Some("2020-01-01".to_string()),
        }];
        let p = payload(vec![proj]);
        let doc = render_document("2026-06-15", &p);
        assert!(doc.contains("## Lapsed acceptances"));
        assert!(doc.contains("acceptance expired 2020-01-01"));
    }

    #[test]
    fn render_accepted_and_not_scanned_sections() {
        let accepted = ProjectSupplyChainScan {
            project: "beta".to_string(),
            stack: "python".to_string(),
            findings: vec![],
            suppressed: vec![SuppressedFinding {
                cve: "CVE-1".to_string(),
                reason: "not exploitable".to_string(),
                status: "allowlisted".to_string(),
                expired_on: None,
            }],
            scan_error: None,
        };
        let errored = ProjectSupplyChainScan {
            project: "gamma".to_string(),
            stack: "rust".to_string(),
            findings: vec![],
            suppressed: vec![],
            scan_error: Some("cargo audit not installed".to_string()),
        };
        let p = payload(vec![accepted, errored]);
        let doc = render_document("2026-06-15", &p);
        assert!(doc.contains("## Accepted (allowlisted)"));
        assert!(doc.contains("**beta** · `CVE-1`: not exploitable"));
        assert!(doc.contains("## Not scanned"));
        assert!(doc.contains("**gamma** (rust) — cargo audit not installed"));
    }

    #[tokio::test]
    async fn dry_run_skips_file_write() {
        let dir = tempfile::tempdir().unwrap();
        let block = WriteSupplyChainDigest::new(dir.path());
        let p = payload(vec![project_with_findings(
            "alpha",
            vec![finding("CVE-2026-1234")],
        )]);
        let result = block.execute(&trigger(&p, Throttle::DryRun)).await.unwrap();
        let out: SupplyChainScanCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(out.success);
        assert!(out.digest_path.is_none());
        assert!(!today_path(dir.path()).exists(), "dry-run must not write the file");
    }

    #[tokio::test]
    async fn full_throttle_writes_dated_file() {
        let dir = tempfile::tempdir().unwrap();
        let block = WriteSupplyChainDigest::new(dir.path());
        let p = payload(vec![project_with_findings(
            "alpha",
            vec![finding("CVE-2026-1234")],
        )]);
        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();
        let out: SupplyChainScanCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(out.success);
        assert_eq!(out.finding_count, 1);
        let expected = today_path(dir.path());
        assert_eq!(out.digest_path.as_deref(), Some(expected.to_string_lossy().as_ref()));
        let contents = std::fs::read_to_string(&expected).unwrap();
        assert!(contents.contains("# Supply-Chain Advisory Scan — "));
        assert!(contents.contains("`CVE-2026-1234`"));
    }

    #[tokio::test]
    async fn unwritable_dir_emits_failure() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"i am a file").unwrap();
        let unwritable = blocker.join("nested");
        let block = WriteSupplyChainDigest::new(&unwritable);
        let p = payload(vec![project_with_findings("alpha", vec![finding("CVE-1")])]);
        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();
        let out: SupplyChainScanCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(!out.success);
        assert!(out.digest_path.is_none());
        assert!(result.summary.contains("failed to write"));
    }
}
