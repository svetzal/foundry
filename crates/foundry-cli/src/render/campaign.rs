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
