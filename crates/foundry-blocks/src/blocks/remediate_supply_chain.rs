//! `RemediateSupplyChain` — branched remediation step of the supply-chain
//! formation (EXP-003 Phase 2, Slice 2).
//!
//! Sinks on `SupplyChainScanned`. Sits between the scan and the digest: it
//! triages every live finding by *fix availability* and emits
//! `SupplyChainRemediated`, carrying the scan through verbatim so the digest
//! still renders its findings/lapsed/accepted/not-scanned sections.
//!
//! ## Current increment — triage classifier (non-mutating)
//!
//! This block does **not** mutate any working tree yet. It only classifies each
//! live finding against the bright line the Maintenance triage framework already
//! uses: a *populated* `fix_version` → mechanically **fixable** (a future
//! mutating pass can auto-bump); an *empty* `fix_version` → a **policy call**
//! (an exploitability judgement about our usage that must stay human). The split
//! is surfaced in the digest so a project's advisories read as "auto-fixable" vs
//! "needs your decision" rather than one undifferentiated list.
//!
//! ## Next increment — the mutating half (shipped dark/gated)
//!
//! The actual fixes — in-range auto-bump, override-pin manifest rewrite with
//! gate-verify-and-rollback, and the no-fix policy surface — land here behind an
//! explicit env gate so they stay inert until enabled, even under `Full`
//! throttle. `remediated_count` will then report what was applied; today it is
//! always `0`.

use std::pin::Pin;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{SupplyChainRemediatedPayload, SupplyChainScannedPayload};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Triages each live supply-chain finding by fix availability. Observer — it
/// only reads the scan payload and reclassifies; it never touches a repo.
pub struct RemediateSupplyChain;

impl TaskBlock for RemediateSupplyChain {
    task_block_meta! {
        name: "Remediate Supply Chain",
        kind: Observer,
        sinks_on: [SupplyChainScanned],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let scan = parse_payload!(trigger, SupplyChainScannedPayload);
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        Box::pin(async move {
            // Count the fixable / policy-call split across every live finding.
            let mut fixable_count: u64 = 0;
            let mut no_fix_count: u64 = 0;
            for proj in &scan.projects {
                for finding in &proj.findings {
                    if finding.fix_version.is_some() {
                        fixable_count += 1;
                    } else {
                        no_fix_count += 1;
                    }
                }
            }

            tracing::info!(
                finding_count = scan.finding_count,
                fixable_count,
                no_fix_count,
                "supply-chain remediation triage complete (classifier only — no mutation)"
            );

            super::emit_result(
                format!(
                    "Supply-chain triage: {fixable_count} auto-fixable, {no_fix_count} policy-call of {total} finding(s)",
                    total = scan.finding_count
                ),
                EventType::SupplyChainRemediated,
                &project,
                throttle,
                &SupplyChainRemediatedPayload {
                    projects: scan.projects,
                    project_count: scan.project_count,
                    finding_count: scan.finding_count,
                    affected_project_count: scan.affected_project_count,
                    fixable_count,
                    no_fix_count,
                    // No mutation yet — the auto-fix engine ships dark in the
                    // next increment.
                    remediated_count: 0,
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::payload::{ProjectSupplyChainScan, SupplyChainFinding};
    use foundry_sdk::throttle::Throttle;

    use super::*;

    fn finding(cve: &str, fix: Option<&str>) -> SupplyChainFinding {
        SupplyChainFinding {
            cve: cve.to_string(),
            package: "pkg".to_string(),
            severity: Some("high".to_string()),
            version: Some("0.1.0".to_string()),
            fix_version: fix.map(str::to_string),
        }
    }

    fn project(name: &str, findings: Vec<SupplyChainFinding>) -> ProjectSupplyChainScan {
        ProjectSupplyChainScan {
            project: name.to_string(),
            stack: "rust".to_string(),
            findings,
            suppressed: vec![],
            scan_error: None,
        }
    }

    fn scanned(projects: Vec<ProjectSupplyChainScan>) -> SupplyChainScannedPayload {
        let finding_count: u64 = projects.iter().map(|p| p.findings.len() as u64).sum();
        let affected = projects.iter().filter(|p| !p.findings.is_empty()).count() as u64;
        SupplyChainScannedPayload {
            project_count: projects.len() as u64,
            finding_count,
            affected_project_count: affected,
            projects,
        }
    }

    fn trigger(p: &SupplyChainScannedPayload) -> Event {
        Event::new(
            EventType::SupplyChainScanned,
            "system".to_string(),
            Throttle::Full,
            serde_json::to_value(p).unwrap(),
        )
    }

    fn remediated(result: &TaskBlockResult) -> SupplyChainRemediatedPayload {
        result.events[0].parse_payload().unwrap()
    }

    assert_block_meta!(
        RemediateSupplyChain,
        kind: Observer,
        sinks_on: [SupplyChainScanned],
    );

    #[tokio::test]
    async fn classifies_fixable_and_policy_call_findings() {
        let p = scanned(vec![project(
            "alpha",
            vec![
                finding("CVE-1", Some("1.2.3")),  // fixable
                finding("CVE-2", None),           // policy call
                finding("CVE-3", Some("0.28.1")), // fixable
            ],
        )]);
        let result = RemediateSupplyChain.execute(&trigger(&p)).await.unwrap();
        assert!(result.success);

        let out = remediated(&result);
        assert_eq!(out.fixable_count, 2);
        assert_eq!(out.no_fix_count, 1);
        assert_eq!(out.remediated_count, 0, "classifier applies no fixes");
        assert_eq!(out.finding_count, 3);
    }

    #[tokio::test]
    async fn carries_scan_projects_through_for_the_digest() {
        let p = scanned(vec![project("alpha", vec![finding("CVE-1", Some("1.0.0"))])]);
        let result = RemediateSupplyChain.execute(&trigger(&p)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.projects.len(), 1);
        assert_eq!(out.projects[0].project, "alpha");
        assert_eq!(out.projects[0].findings[0].cve, "CVE-1");
        assert_eq!(out.projects[0].findings[0].fix_version.as_deref(), Some("1.0.0"));
    }

    #[tokio::test]
    async fn clean_scan_emits_zero_counts() {
        let p = scanned(vec![project("alpha", vec![])]);
        let result = RemediateSupplyChain.execute(&trigger(&p)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.fixable_count, 0);
        assert_eq!(out.no_fix_count, 0);
        assert_eq!(out.finding_count, 0);
    }

    #[tokio::test]
    async fn emits_supply_chain_remediated_event() {
        let p = scanned(vec![project("alpha", vec![finding("CVE-1", None)])]);
        let result = RemediateSupplyChain.execute(&trigger(&p)).await.unwrap();
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::SupplyChainRemediated);
    }
}
