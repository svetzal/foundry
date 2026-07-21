use std::fmt::Write as _;

use comfy_table::{ContentArrangement, Table};
use foundry_sdk::campaign::{Campaign, DoneEvidence};

/// Render a `CampaignDetail` received from the daemon gRPC response.
///
/// Takes the proto wire type rather than the SDK type, so callers on the online
/// code path do not need to re-read the campaign store after a successful RPC.
pub fn campaign_detail_proto(detail: &crate::proto::CampaignDetail) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Name:       {}", detail.name);
    let _ = writeln!(out, "Project:    {}", detail.project);
    let _ = writeln!(out, "Status:     {}", detail.status);
    let auth = if detail.authorized_by.is_empty() {
        "no"
    } else {
        &detail.authorized_by
    };
    let _ = writeln!(out, "Authorized: {auth}");
    let agent = if detail.agent_provider.is_empty() {
        "default"
    } else {
        &detail.agent_provider
    };
    let _ = writeln!(out, "Agent:      {agent}");
    let _ = writeln!(
        out,
        "Cycles:     {}/{} ({} landed)",
        detail.cycles_completed, detail.max_cycles, detail.cycles_landed
    );
    let _ = writeln!(out, "Mission:    {}", detail.mission);
    let _ = writeln!(out, "Intent:     {}", detail.intent_refs.join(", "));
    let _ = writeln!(out, "Context:    {}", detail.context_paths.join(", "));
    let _ = writeln!(out, "Done evidence:");
    for evidence in &detail.done_evidence {
        match evidence.kind.as_str() {
            "gate" => {
                let artifacts_note = if evidence.artifacts.is_empty() {
                    String::new()
                } else {
                    format!(" (artifacts: {})", evidence.artifacts.join(", "))
                };
                let _ = writeln!(
                    out,
                    "  gate [{}]: {}{}",
                    if evidence.required {
                        "required"
                    } else {
                        "optional"
                    },
                    evidence.command,
                    artifacts_note,
                );
            }
            "review" => {
                let _ = writeln!(out, "  review: {}", evidence.statement);
            }
            other => {
                let _ = writeln!(out, "  {other}: (unknown kind)");
            }
        }
    }
    let _ = writeln!(out, "Owner decisions:");
    for decision in &detail.owner_decisions {
        let _ = writeln!(
            out,
            "  {} [{}] {}",
            decision.decided_at, decision.authorized_by, decision.decision
        );
    }
    out
}

pub fn campaign_table(campaigns: &[Campaign]) -> String {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["Name", "Project", "Status", "Cycles", "Landed", "Agent"]);
    for campaign in campaigns {
        table.add_row(vec![
            campaign.name.as_str(),
            campaign.project.as_str(),
            &campaign.status.to_string(),
            &format!("{}/{}", campaign.cycles_completed, campaign.budget.max_cycles),
            &campaign.cycles_landed.to_string(),
            campaign.agent_provider.as_deref().unwrap_or("default"),
        ]);
    }
    let mut out = String::new();
    let _ = writeln!(out, "{table}");
    out
}

pub fn campaign_detail(campaign: &Campaign) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Name:       {}", campaign.name);
    let _ = writeln!(out, "Project:    {}", campaign.project);
    let _ = writeln!(out, "Status:     {}", campaign.status);
    let _ = writeln!(out, "Authorized: {}", campaign.authorized_by.as_deref().unwrap_or("no"));
    let _ =
        writeln!(out, "Agent:      {}", campaign.agent_provider.as_deref().unwrap_or("default"));
    let _ = writeln!(
        out,
        "Cycles:     {}/{} ({} landed)",
        campaign.cycles_completed, campaign.budget.max_cycles, campaign.cycles_landed
    );
    let _ = writeln!(out, "Mission:    {}", campaign.mission);
    let _ = writeln!(out, "Intent:     {}", campaign.intent_refs.join(", "));
    let _ = writeln!(out, "Context:    {}", campaign.context_paths.join(", "));
    let _ = writeln!(out, "Done evidence:");
    for evidence in &campaign.done_evidence {
        match evidence {
            DoneEvidence::Gate {
                command,
                required,
                artifacts,
            } => {
                let artifacts = if artifacts.is_empty() {
                    String::new()
                } else {
                    format!(" (artifacts: {})", artifacts.join(", "))
                };
                let _ = writeln!(
                    out,
                    "  gate [{}]: {command}{artifacts}",
                    if *required { "required" } else { "optional" }
                );
            }
            DoneEvidence::Review { statement } => {
                let _ = writeln!(out, "  review: {statement}");
            }
        }
    }
    let _ = writeln!(out, "Owner decisions:");
    for decision in &campaign.owner_decisions {
        let _ = writeln!(
            out,
            "  {} [{}] {}",
            decision.decided_at.to_rfc3339(),
            decision.authorized_by,
            decision.decision
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use foundry_sdk::campaign::{CampaignBudget, CampaignStatus, OwnerDecision};

    use super::*;

    fn campaign_fixture() -> Campaign {
        Campaign {
            name: "harden-auth".to_string(),
            project: "widgetco".to_string(),
            mission: "Harden the auth module".to_string(),
            intent_refs: vec!["intent-1".to_string()],
            context_paths: vec!["src/auth".to_string()],
            done_evidence: vec![DoneEvidence::Review {
                statement: "auth is hardened".to_string(),
            }],
            budget: CampaignBudget { max_cycles: 5 },
            escalation: vec![],
            status: CampaignStatus::Active,
            cycles_completed: 2,
            cycles_landed: 1,
            authorized_by: Some("stacey".to_string()),
            agent_provider: None,
            last_run_event_id: None,
            owner_decisions: vec![],
            pending_run_result: None,
        }
    }

    fn proto_detail_fixture() -> crate::proto::CampaignDetail {
        crate::proto::CampaignDetail {
            name: "harden-auth".to_string(),
            project: "widgetco".to_string(),
            mission: "Harden the auth module".to_string(),
            status: "active".to_string(),
            cycles_completed: 2,
            cycles_landed: 1,
            max_cycles: 5,
            authorized_by: "stacey".to_string(),
            agent_provider: String::new(),
            last_run_event_id: String::new(),
            intent_refs: vec!["intent-1".to_string()],
            context_paths: vec!["src/auth".to_string()],
            done_evidence: vec![],
            escalation: vec![],
            owner_decisions: vec![],
        }
    }

    // -- campaign_table --

    #[test]
    fn campaign_table_renders_header_and_one_row() {
        let out = campaign_table(&[campaign_fixture()]);
        for header in ["Name", "Project", "Status", "Cycles", "Landed", "Agent"] {
            assert!(out.contains(header), "got: {out}");
        }
        assert!(out.contains("harden-auth"), "got: {out}");
        assert!(out.contains("widgetco"), "got: {out}");
    }

    #[test]
    fn campaign_table_shows_default_agent_when_none() {
        let campaign = Campaign {
            agent_provider: None,
            ..campaign_fixture()
        };
        let out = campaign_table(&[campaign]);
        assert!(out.contains("default"), "got: {out}");
    }

    #[test]
    fn campaign_table_formats_cycles_as_completed_over_max() {
        let out = campaign_table(&[campaign_fixture()]);
        assert!(out.contains("2/5"), "got: {out}");
    }

    // -- campaign_detail (SDK-typed path) --

    #[test]
    fn campaign_detail_renders_all_labels() {
        let out = campaign_detail(&campaign_fixture());
        for label in [
            "Name:",
            "Project:",
            "Status:",
            "Authorized:",
            "Agent:",
            "Cycles:",
            "Mission:",
            "Intent:",
            "Context:",
            "Done evidence:",
            "Owner decisions:",
        ] {
            assert!(out.contains(label), "missing {label} in: {out}");
        }
    }

    #[test]
    fn campaign_detail_shows_no_when_unauthorized() {
        let campaign = Campaign {
            authorized_by: None,
            ..campaign_fixture()
        };
        let out = campaign_detail(&campaign);
        assert!(out.contains("Authorized: no"), "got: {out}");
    }

    #[test]
    fn campaign_detail_renders_gate_evidence_with_artifacts() {
        let campaign = Campaign {
            done_evidence: vec![DoneEvidence::Gate {
                command: "cargo test".to_string(),
                required: true,
                artifacts: vec!["target/report.json".to_string()],
            }],
            ..campaign_fixture()
        };
        let out = campaign_detail(&campaign);
        assert!(out.contains("gate [required]: cargo test"), "got: {out}");
        assert!(out.contains("(artifacts: target/report.json)"), "got: {out}");
    }

    #[test]
    fn campaign_detail_renders_gate_optional_without_artifacts() {
        let campaign = Campaign {
            done_evidence: vec![DoneEvidence::Gate {
                command: "cargo clippy".to_string(),
                required: false,
                artifacts: vec![],
            }],
            ..campaign_fixture()
        };
        let out = campaign_detail(&campaign);
        assert!(out.contains("gate [optional]: cargo clippy"), "got: {out}");
        assert!(!out.contains("(artifacts:"), "got: {out}");
    }

    #[test]
    fn campaign_detail_renders_review_evidence() {
        let campaign = Campaign {
            done_evidence: vec![DoneEvidence::Review {
                statement: "it works end to end".to_string(),
            }],
            ..campaign_fixture()
        };
        let out = campaign_detail(&campaign);
        assert!(out.contains("review: it works end to end"), "got: {out}");
    }

    #[test]
    fn campaign_detail_renders_owner_decision() {
        let decided_at = chrono::DateTime::parse_from_rfc3339("2026-07-18T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let campaign = Campaign {
            owner_decisions: vec![OwnerDecision {
                decision: "Proceed with the gRPC path.".to_string(),
                authorized_by: "stacey".to_string(),
                decided_at,
            }],
            ..campaign_fixture()
        };
        let out = campaign_detail(&campaign);
        assert!(out.contains("2026-07-18T12:00:00+00:00"), "got: {out}");
        assert!(out.contains("[stacey]"), "got: {out}");
        assert!(out.contains("Proceed with the gRPC path."), "got: {out}");
    }

    // -- campaign_detail_proto (proto-typed path) --

    #[test]
    fn campaign_detail_proto_renders_all_labels() {
        let out = campaign_detail_proto(&proto_detail_fixture());
        for label in [
            "Name:",
            "Project:",
            "Status:",
            "Authorized:",
            "Agent:",
            "Cycles:",
            "Mission:",
            "Intent:",
            "Context:",
            "Done evidence:",
            "Owner decisions:",
        ] {
            assert!(out.contains(label), "missing {label} in: {out}");
        }
    }

    #[test]
    fn campaign_detail_proto_shows_no_when_authorized_by_empty() {
        let detail = crate::proto::CampaignDetail {
            authorized_by: String::new(),
            ..proto_detail_fixture()
        };
        let out = campaign_detail_proto(&detail);
        assert!(out.contains("Authorized: no"), "got: {out}");
    }

    #[test]
    fn campaign_detail_proto_shows_default_when_agent_empty() {
        let detail = crate::proto::CampaignDetail {
            agent_provider: String::new(),
            ..proto_detail_fixture()
        };
        let out = campaign_detail_proto(&detail);
        assert!(out.contains("Agent:      default"), "got: {out}");
    }

    #[test]
    fn campaign_detail_proto_renders_gate_review_and_unknown_kind() {
        let detail = crate::proto::CampaignDetail {
            done_evidence: vec![
                crate::proto::DoneEvidence {
                    kind: "gate".to_string(),
                    command: "cargo test".to_string(),
                    required: true,
                    statement: String::new(),
                    artifacts: vec!["target/report.json".to_string()],
                },
                crate::proto::DoneEvidence {
                    kind: "review".to_string(),
                    command: String::new(),
                    required: false,
                    statement: "it works end to end".to_string(),
                    artifacts: vec![],
                },
                crate::proto::DoneEvidence {
                    kind: "mystery".to_string(),
                    command: String::new(),
                    required: false,
                    statement: String::new(),
                    artifacts: vec![],
                },
            ],
            ..proto_detail_fixture()
        };
        let out = campaign_detail_proto(&detail);
        assert!(
            out.contains("gate [required]: cargo test (artifacts: target/report.json)"),
            "got: {out}"
        );
        assert!(out.contains("review: it works end to end"), "got: {out}");
        assert!(out.contains("mystery: (unknown kind)"), "got: {out}");
    }
}
