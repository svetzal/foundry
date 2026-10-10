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

/// The heading above a campaign's cycles as the work-item ledger records them.
pub const CYCLES_HEADING: &str = "Cycles in the ledger:";

/// The line printed under [`CYCLES_HEADING`] when the ledger records no cycle
/// for the campaign: a campaign never advanced, or one whose cycles predate
/// the typed source.
pub const NO_CYCLES_LINE: &str = "  (none recorded)";

/// A campaign's cycles as the ledger records them, one line each in the order
/// they arrived: the cycle number, the item id, its state, the timestamp of
/// its settlement or start, and its one-line reason.
///
/// `items` is the `ListWorkItems` response for `source campaign:<name>`,
/// already in the daemon's reading order (running first, then open, then
/// settled newest first); nothing here re-sorts it.
#[must_use]
pub fn campaign_cycles(items: &[crate::proto::WorkItem]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{CYCLES_HEADING}");
    if items.is_empty() {
        let _ = writeln!(out, "{NO_CYCLES_LINE}");
        return out;
    }
    for item in items {
        let cycle = item
            .source
            .as_ref()
            .and_then(|source| source.cycle)
            .map_or_else(|| "cycle ?".to_string(), |cycle| format!("cycle {cycle}"));
        let stamp = item.settled_at.as_deref().or(item.started_at.as_deref()).unwrap_or("-");
        let _ =
            writeln!(out, "  {cycle:<9} {}  {:<14} {stamp}  {}", item.id, item.state, item.reason);
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

pub fn campaign_table_proto(campaigns: &[crate::proto::Campaign]) -> String {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["Name", "Project", "Status", "Cycles", "Landed", "Agent"]);
    for campaign in campaigns {
        let agent = if campaign.agent_provider.is_empty() {
            "default"
        } else {
            campaign.agent_provider.as_str()
        };
        table.add_row(vec![
            campaign.name.as_str(),
            campaign.project.as_str(),
            campaign.status.as_str(),
            &format!("{}/{}", campaign.cycles_completed, campaign.max_cycles),
            &campaign.cycles_landed.to_string(),
            agent,
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

/// Show measured stage costs without presenting unpriced usage as free.
pub fn report(report: &foundry_sdk::campaign::report::CampaignReport) -> String {
    let mut out = format!(
        "{}: {}\nCycles: {} dispatched, {} landed\n",
        report.name, report.status, report.cycles_dispatched, report.cycles_landed
    );
    let _ = writeln!(out, "Writable repositories: {}", report.writable_repositories.join(", "));
    let _ = writeln!(
        out,
        "Limits: formation prompt {} bytes; no agent clock deadlines",
        report.stage_limits.formation_prompt_bytes
    );
    for (role, stage) in &report.stages {
        let _ = writeln!(
            out,
            "{role}: {} sessions, {}s, {} fresh input, {} cached input, {} output; {} running, {} unmeasured",
            stage.sessions,
            stage.elapsed_ms / 1000,
            stage.input_tokens,
            stage.cached_input_tokens,
            stage.output_tokens,
            stage.running_sessions,
            stage.unmeasured_sessions
        );
        let partial = stage.unmeasured_sessions > 0
            || stage.running_sessions > 0
            || !stage.unpriced_models.is_empty()
            || !stage.pricing_limitations.is_empty();
        let _ = writeln!(
            out,
            "  Known list estimate ${:.4}{}",
            stage.known_list_usd,
            if partial {
                " (partial, not total spend)"
            } else {
                ""
            }
        );
        if !stage.unpriced_models.is_empty() {
            let _ = writeln!(
                out,
                "  Unpriced: {}",
                stage.unpriced_models.iter().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        for limitation in &stage.pricing_limitations {
            let _ = writeln!(out, "  Pricing limitation: {limitation}");
        }
    }
    for session in &report.formation_sessions {
        let _ = writeln!(out, "Formation {}: {}", session.session_id, session.status);
        if let Some(tools) = &session.tool_activity {
            let _ = writeln!(
                out,
                "  {} commands, {} exact repeats, {} failed; {} captured output bytes (not model tokens)",
                tools.completed_commands,
                tools.repeated_commands,
                tools.failed_commands,
                tools.captured_output_bytes
            );
        }
        if let Some(observation) = &session.native_observation {
            let _ = writeln!(
                out,
                "  Native observation: {} input including {} cached, {} output; first request {} input (not added to final usage)",
                observation.input_tokens_including_cache,
                observation.cached_input_tokens,
                observation.output_tokens,
                observation.first_request_input_tokens
            );
            if session.usage.is_none() {
                let _ = writeln!(out, "  Partial observation only; final usage remains unmeasured");
            }
        }
        for error in [&session.transcript_error, &session.native_observation_error]
            .into_iter()
            .flatten()
        {
            let _ = writeln!(out, "  Audit unavailable: {error}");
        }
        let _ = writeln!(out, "  Transcript: {}", session.source_log_path);
    }
    for reason in &report.external_completion_reasons {
        let _ = writeln!(out, "{reason}");
    }
    let _ = writeln!(
        out,
        "Legacy stage inference: {} sessions; incomplete log lines: {}",
        report.inferred_stage_sessions, report.incomplete_log_lines
    );
    out
}

#[cfg(test)]
mod tests {
    use foundry_sdk::campaign::{CampaignBudget, CampaignStatus, OwnerDecision};

    use super::*;

    #[test]
    fn cost_reports_show_known_usd_and_missing_billing_dimensions() {
        use foundry_sdk::campaign::report::{CampaignReport, StageReport};
        let mut data: CampaignReport = serde_json::from_value(serde_json::json!({
            "name":"costs", "status":"active", "cycles_dispatched":1, "cycles_landed":0,
            "stage_limits":{}, "writable_repositories":[], "external_completion_reasons":[],
            "inferred_stage_sessions":0, "incomplete_log_lines":0, "stages":{}
        }))
        .unwrap();
        data.stages.insert(
            "formation".into(),
            StageReport {
                sessions: 1,
                known_list_usd: 2.34,
                pricing_limitations: ["request sizes unknown".into()].into(),
                ..StageReport::default()
            },
        );
        let rendered = report(&data);
        assert!(rendered.contains("Known list estimate $2.3400 (partial, not total spend)"));
        assert!(rendered.contains("Pricing limitation: request sizes unknown"));
        data.stages.get_mut("formation").unwrap().pricing_limitations.clear();
        assert!(!report(&data).contains("partial, not total spend"));
        data.stages.get_mut("formation").unwrap().unmeasured_sessions = 1;
        assert!(report(&data).contains("partial, not total spend"));
    }

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
            budget: CampaignBudget {
                max_cycles: 5,
                ..Default::default()
            },
            escalation: vec![],
            status: CampaignStatus::Active,
            cycles_completed: 2,
            cycles_landed: 1,
            authorized_by: Some("stacey".to_string()),
            agent_provider: None,
            last_run_event_id: None,
            owner_decisions: vec![],
            pending_run_result: None,
            objective_history: vec![],
            writable_repositories: vec![],
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

    // -- campaign_cycles (ledger-backed cycle list) --

    fn cycle_item(
        id: &str,
        cycle: Option<u64>,
        state: &str,
        settled: Option<&str>,
    ) -> crate::proto::WorkItem {
        crate::proto::WorkItem {
            id: id.to_string(),
            project: "p".to_string(),
            objective: "do the thing".to_string(),
            kind: "campaign_cycle".to_string(),
            lane: "campaign".to_string(),
            origin: "campaign c cycle n".to_string(),
            submitted_at: "2026-10-01T00:00:00+00:00".to_string(),
            started_at: Some("2026-10-01T00:00:01+00:00".to_string()),
            settled_at: settled.map(str::to_string),
            state: state.to_string(),
            reason: format!("{state} reason"),
            trace_id: None,
            verdict: None,
            landed_commit: None,
            preservation_ref: None,
            worktree: None,
            worktree_removed: None,
            operator_action: None,
            resumes: None,
            source: Some(crate::proto::WorkSource {
                kind: "campaign".to_string(),
                r#ref: "c".to_string(),
                cycle,
            }),
            depends_on: Vec::new(),
            not_before: None,
        }
    }

    #[test]
    fn campaign_cycles_lists_each_cycle_in_the_order_given_with_its_number_and_state() {
        let out = campaign_cycles(&[
            cycle_item("wi_c2", Some(2), "running", None),
            cycle_item("wi_c1", Some(1), "landed", Some("2026-10-01T01:00:00+00:00")),
        ]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], CYCLES_HEADING);
        assert!(lines[1].starts_with("  cycle 2"), "got: {}", lines[1]);
        assert!(lines[1].contains("wi_c2") && lines[1].contains("running"), "got: {}", lines[1]);
        assert!(lines[2].starts_with("  cycle 1"), "got: {}", lines[2]);
        assert!(lines[2].contains("2026-10-01T01:00:00+00:00"), "settled stamp: {}", lines[2]);
        assert!(lines[2].contains("landed reason"), "got: {}", lines[2]);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn campaign_cycles_says_so_when_the_ledger_records_none() {
        assert_eq!(campaign_cycles(&[]), format!("{CYCLES_HEADING}\n{NO_CYCLES_LINE}\n"));
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
