use std::fmt::Write as _;

use comfy_table::{ContentArrangement, Table};
use foundry_sdk::campaign::{Campaign, DoneEvidence};

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
            DoneEvidence::Gate { command, required } => {
                let _ = writeln!(
                    out,
                    "  gate [{}]: {command}",
                    if *required { "required" } else { "optional" }
                );
            }
            DoneEvidence::Review { statement } => {
                let _ = writeln!(out, "  review: {statement}");
            }
        }
    }
    out
}
