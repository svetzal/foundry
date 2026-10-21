use std::fmt::Write as _;

use comfy_table::{ContentArrangement, Table};
use foundry_sdk::campaign::{Campaign, CampaignStatus, DoneEvidence, OwnerOption, owner_options};
use serde::Serialize;

/// Render a `CampaignDetail` received from the daemon gRPC response.
///
/// Takes the proto wire type rather than the SDK type, so callers on the online
/// code path do not need to re-read the campaign store after a successful RPC.
pub fn campaign_detail_proto(detail: &crate::proto::CampaignDetail) -> String {
    let mut out = String::new();
    if let Some(recovery) = &detail.recovery {
        out.push_str(&super::recovery::detail("campaign", &detail.name, recovery));
    }
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
    if let Some(initiative) = &detail.initiative {
        let _ = writeln!(out, "Initiative: {initiative}");
    }
    if let Some(providers) = &detail.providers {
        out.push_str(&provider_overrides([
            ("formation", providers.formation.as_deref()),
            ("execution", providers.execution.as_deref()),
            ("review", providers.review.as_deref()),
        ]));
    }
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
    if detail.status == CampaignStatus::Escalated.to_string() {
        out.push_str(&escalation_section(detail.open_escalation.as_ref()));
    }
    out.push_str(&options_section(&detail_options(detail)));
    out
}

/// The line under the `Escalation:` heading when the campaign is escalated but
/// no reason was ever recorded, neither on the campaign nor in the event log.
pub const REASON_NOT_RECORDED: &str = "(not recorded)";

/// Why an escalated campaign waits on its owner, under an `Escalation:`
/// heading: the reason, when and at which cycle, the agent's question and
/// choices when a task asked one, and a note when the record was recovered
/// from the event log.
#[must_use]
pub fn escalation_section(escalation: Option<&crate::proto::CampaignEscalation>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Escalation:");
    let Some(escalation) = escalation else {
        let _ = writeln!(out, "  Reason:     {REASON_NOT_RECORDED}");
        return out;
    };
    let _ = writeln!(out, "  Reason:     {}", escalation.reason);
    let _ = writeln!(
        out,
        "  Escalated:  {} at {} cycles ({} landed)",
        escalation.escalated_at, escalation.cycles_completed, escalation.cycles_landed
    );
    if let Some(question) = &escalation.question {
        let _ = writeln!(out, "  Question:   {question}");
    }
    for choice in &escalation.choices {
        let _ = writeln!(out, "  Choice:     {choice}");
    }
    if escalation.from_event_log {
        let _ = writeln!(
            out,
            "  Recorded:   recovered from the event log; the campaign escalated before Foundry kept the reason on the campaign"
        );
    }
    out
}

/// Wire form of an open escalation, matching what the daemon sends, so the
/// offline paths render through the same functions as the online ones.
#[must_use]
pub fn escalation_to_proto(
    escalation: &foundry_sdk::campaign::CampaignEscalation,
) -> crate::proto::CampaignEscalation {
    crate::proto::CampaignEscalation {
        reason: escalation.reason.clone(),
        escalated_at: escalation.escalated_at.to_rfc3339(),
        cycles_completed: escalation.cycles_completed,
        cycles_landed: escalation.cycles_landed,
        question: escalation.question.clone(),
        choices: escalation.choices.clone(),
        from_event_log: escalation.from_event_log,
    }
}

/// What the owner can run now, one command line per row with its effect, under
/// an `Owner options:` heading. Empty when there is nothing to run (a
/// completed or cancelled campaign).
#[must_use]
pub fn options_section(options: &[OwnerOption]) -> String {
    let mut out = String::new();
    if options.is_empty() {
        return out;
    }
    let width = options.iter().map(|o| o.command.len()).max().unwrap_or(0);
    let _ = writeln!(out, "Owner options:");
    for option in options {
        let _ = writeln!(out, "  {:<width$}  {}", option.command, option.effect);
    }
    out
}

/// The owner options for a campaign as the daemon describes it. A status this
/// build does not know offers nothing rather than guessing.
#[must_use]
pub fn campaign_options(
    name: &str,
    status: &str,
    authorized_by: &str,
    cycles_completed: u64,
    max_cycles: u64,
) -> Vec<OwnerOption> {
    CampaignStatus::from_tag(status).map_or_else(Vec::new, |status| {
        owner_options(name, status, !authorized_by.is_empty(), cycles_completed >= max_cycles)
    })
}

fn detail_options(detail: &crate::proto::CampaignDetail) -> Vec<OwnerOption> {
    let mut options = campaign_options(
        &detail.name,
        &detail.status,
        &detail.authorized_by,
        detail.cycles_completed,
        detail.max_cycles,
    );
    if let Some(initiative) = &detail.initiative {
        options.retain(|option| !option.command.contains("--add-cycles"));
        if !matches!(detail.status.as_str(), "completed" | "cancelled") {
            options.push(OwnerOption {
                control: "show".into(),
                command: format!("foundry initiative show {initiative}"),
                effect: "Inspect the initiative that owns this member's cycle budget".into(),
            });
        }
    }
    options
}

fn provider_overrides<const N: usize>(overrides: [(&str, Option<&str>); N]) -> String {
    let overrides = overrides
        .into_iter()
        .filter_map(|(stage, provider)| provider.map(|provider| format!("{stage}={provider}")))
        .collect::<Vec<_>>();
    if overrides.is_empty() {
        String::new()
    } else {
        format!("Providers:  {}\n", overrides.join(", "))
    }
}

#[derive(Serialize)]
struct JsonStageProviders<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    formation: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    review: Option<&'a str>,
}

/// JSON projection of an open escalation, shared by `campaign show --json` and
/// the queue's escalated-campaign rows. Optional keys are absent when unset.
#[derive(Serialize)]
pub(crate) struct JsonEscalation<'a> {
    reason: &'a str,
    escalated_at: &'a str,
    cycles_completed: u64,
    cycles_landed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    question: Option<&'a str>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    choices: &'a [String],
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    from_event_log: bool,
}

impl<'a> From<&'a crate::proto::CampaignEscalation> for JsonEscalation<'a> {
    fn from(escalation: &'a crate::proto::CampaignEscalation) -> Self {
        Self {
            reason: &escalation.reason,
            escalated_at: &escalation.escalated_at,
            cycles_completed: escalation.cycles_completed,
            cycles_landed: escalation.cycles_landed,
            question: escalation.question.as_deref(),
            choices: &escalation.choices,
            from_event_log: escalation.from_event_log,
        }
    }
}

#[derive(Serialize)]
struct JsonDoneEvidence<'a> {
    kind: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    command: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    required: Option<bool>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    artifacts: &'a [String],
    #[serde(skip_serializing_if = "str::is_empty")]
    statement: &'a str,
}

#[derive(Serialize)]
struct JsonOwnerDecision<'a> {
    decision: &'a str,
    authorized_by: &'a str,
    decided_at: &'a str,
}

/// `campaign show --json`: the campaign, its open escalation, the owner's
/// options and its ledger cycles, from the same fetched data as the human form.
#[derive(Serialize)]
struct JsonCampaignShow<'a> {
    name: &'a str,
    project: &'a str,
    mission: &'a str,
    status: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    authorized_by: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    agent_provider: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    initiative: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    providers: Option<JsonStageProviders<'a>>,
    cycles_completed: u64,
    cycles_landed: u64,
    max_cycles: u64,
    intent_refs: &'a [String],
    context_paths: &'a [String],
    done_evidence: Vec<JsonDoneEvidence<'a>>,
    escalation: &'a [String],
    owner_decisions: Vec<JsonOwnerDecision<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_escalation: Option<JsonEscalation<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<&'a crate::proto::RecoveryState>,
    owner_options: Vec<OwnerOption>,
    cycles: Vec<super::queue::JsonItem<'a>>,
}

/// `campaign show --json` as a pretty-printed object.
///
/// Keys follow the campaign definition (`escalation` is the definition's
/// escalation rules); `open_escalation` is absent unless the campaign is
/// escalated with a known reason, `owner_options` lists what the owner can run
/// now, and `cycles` carries the ledger records `queue --json` would print.
#[must_use]
pub fn campaign_show_json(
    detail: &crate::proto::CampaignDetail,
    items: &[crate::proto::WorkItem],
) -> String {
    let show = JsonCampaignShow {
        recovery: detail.recovery.as_ref(),
        name: &detail.name,
        project: &detail.project,
        mission: &detail.mission,
        status: &detail.status,
        authorized_by: &detail.authorized_by,
        agent_provider: &detail.agent_provider,
        initiative: detail.initiative.as_deref(),
        providers: detail
            .providers
            .as_ref()
            .filter(|providers| {
                providers.formation.is_some()
                    || providers.execution.is_some()
                    || providers.review.is_some()
            })
            .map(|providers| JsonStageProviders {
                formation: providers.formation.as_deref(),
                execution: providers.execution.as_deref(),
                review: providers.review.as_deref(),
            }),
        cycles_completed: detail.cycles_completed,
        cycles_landed: detail.cycles_landed,
        max_cycles: detail.max_cycles,
        intent_refs: &detail.intent_refs,
        context_paths: &detail.context_paths,
        done_evidence: detail
            .done_evidence
            .iter()
            .map(|e| JsonDoneEvidence {
                kind: &e.kind,
                command: &e.command,
                required: (e.kind == "gate").then_some(e.required),
                artifacts: &e.artifacts,
                statement: &e.statement,
            })
            .collect(),
        escalation: &detail.escalation,
        owner_decisions: detail
            .owner_decisions
            .iter()
            .map(|d| JsonOwnerDecision {
                decision: &d.decision,
                authorized_by: &d.authorized_by,
                decided_at: &d.decided_at,
            })
            .collect(),
        open_escalation: detail.open_escalation.as_ref().map(JsonEscalation::from),
        owner_options: detail_options(detail),
        cycles: items.iter().map(super::queue::JsonItem::from).collect(),
    };
    super::queue::to_pretty(&show)
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
    if let Some(initiative) = &campaign.initiative {
        let _ = writeln!(out, "Initiative: {initiative}");
    }
    out.push_str(&provider_overrides([
        (
            "formation",
            campaign.providers.formation.map(foundry_sdk::gateway::AgentProvider::as_str),
        ),
        (
            "execution",
            campaign.providers.execution.map(foundry_sdk::gateway::AgentProvider::as_str),
        ),
        (
            "review",
            campaign.providers.review.map(foundry_sdk::gateway::AgentProvider::as_str),
        ),
    ]));
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

fn provider_usage_report(report: &foundry_sdk::campaign::report::CampaignReport) -> String {
    let mut out = String::new();
    for (stage, providers) in &report.providers_by_stage {
        for (provider, usage) in providers {
            let _ = writeln!(
                out,
                "  {stage} / {provider}: {} sessions, {} fresh input, {} cached input, {} output; {} running, {} unmeasured",
                usage.sessions,
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.output_tokens,
                usage.running_sessions,
                usage.unmeasured_sessions
            );
        }
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
    out.push_str(&provider_usage_report(report));
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
    if report.status == CampaignStatus::Escalated.to_string() {
        let wire = report.open_escalation.as_ref().map(escalation_to_proto);
        out.push_str(&escalation_section(wire.as_ref()));
    }
    out.push_str(&options_section(&report.owner_options));
    let _ = writeln!(
        out,
        "Legacy stage inference: {} sessions; incomplete log lines: {}",
        report.inferred_stage_sessions, report.incomplete_log_lines
    );
    out
}

/// The line `foundry campaign advance` prints when the daemon refused to
/// duplicate work already in flight for the campaign.
///
/// Names the existing item and the command that inspects it. The advance was a
/// no-op, so there is nothing of this client's to watch.
pub fn advance_in_flight(campaign: &str, in_flight: &crate::proto::CampaignWorkInFlight) -> String {
    let article = if in_flight.kind.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    let mut out = String::new();
    let _ = write!(
        out,
        "Campaign '{campaign}' already has {article} {} in flight: ",
        in_flight.kind
    );
    if in_flight.work_item_id.is_empty() {
        let _ = writeln!(
            out,
            "event {} ({}, not yet in the ledger). Nothing dispatched.",
            in_flight.event_id, in_flight.state
        );
        let _ = writeln!(out, "  next: foundry status");
    } else {
        let _ = writeln!(
            out,
            "work item {} ({}). Nothing dispatched.",
            in_flight.work_item_id, in_flight.state
        );
        let _ = writeln!(out, "  next: foundry queue show {}", in_flight.work_item_id);
    }
    out
}

#[cfg(test)]
mod tests {
    use foundry_sdk::campaign::{CampaignBudget, CampaignStatus, OwnerDecision};

    use super::*;

    fn in_flight(kind: &str, id: &str, state: &str) -> crate::proto::CampaignWorkInFlight {
        crate::proto::CampaignWorkInFlight {
            kind: kind.to_string(),
            work_item_id: id.to_string(),
            state: state.to_string(),
            event_id: "evt_root".to_string(),
        }
    }

    #[test]
    fn an_in_flight_cycle_names_its_work_item_and_how_to_inspect_it() {
        assert_eq!(
            advance_in_flight("probe", &in_flight("cycle", "wi_1", "running")),
            "Campaign 'probe' already has a cycle in flight: work item wi_1 (running). Nothing dispatched.\n  next: foundry queue show wi_1\n"
        );
    }

    #[test]
    fn an_advance_not_yet_in_the_ledger_names_its_event() {
        assert_eq!(
            advance_in_flight("probe", &in_flight("advance", "", "submitted")),
            "Campaign 'probe' already has an advance in flight: event evt_root (submitted, not yet in the ledger). Nothing dispatched.\n  next: foundry status\n"
        );
    }

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
            recovery: None,
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
            providers: foundry_sdk::gateway::StageProviders::default(),
            initiative: None,
            last_run_event_id: None,
            owner_decisions: vec![],
            pending_run_result: None,
            objective_history: vec![],
            open_escalation: None,
            writable_repositories: vec![],
        }
    }

    fn proto_detail_fixture() -> crate::proto::CampaignDetail {
        crate::proto::CampaignDetail {
            recovery: None,
            name: "harden-auth".to_string(),
            project: "widgetco".to_string(),
            mission: "Harden the auth module".to_string(),
            status: "active".to_string(),
            cycles_completed: 2,
            cycles_landed: 1,
            max_cycles: 5,
            authorized_by: "stacey".to_string(),
            agent_provider: String::new(),
            providers: None,
            initiative: None,
            last_run_event_id: String::new(),
            intent_refs: vec!["intent-1".to_string()],
            context_paths: vec!["src/auth".to_string()],
            done_evidence: vec![],
            escalation: vec![],
            owner_decisions: vec![],
            open_escalation: None,
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
            continues: Vec::new(),
            follow_up: None,
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

    // -- escalation and owner options --

    fn escalated_detail(
        escalation: Option<crate::proto::CampaignEscalation>,
    ) -> crate::proto::CampaignDetail {
        crate::proto::CampaignDetail {
            status: "escalated".to_string(),
            cycles_completed: 5,
            open_escalation: escalation,
            ..proto_detail_fixture()
        }
    }

    fn logged_escalation() -> crate::proto::CampaignEscalation {
        crate::proto::CampaignEscalation {
            reason: "campaign cycle budget exhausted (5)".to_string(),
            escalated_at: "2026-10-11T03:00:00+00:00".to_string(),
            cycles_completed: 5,
            cycles_landed: 5,
            question: None,
            choices: vec![],
            from_event_log: true,
        }
    }

    #[test]
    fn escalated_detail_prints_reason_provenance_and_owner_options() {
        let out = campaign_detail_proto(&escalated_detail(Some(logged_escalation())));
        assert!(
            out.contains("Escalation:\n  Reason:     campaign cycle budget exhausted (5)\n"),
            "{out}"
        );
        assert!(
            out.contains("Escalated:  2026-10-11T03:00:00+00:00 at 5 cycles (5 landed)"),
            "{out}"
        );
        assert!(out.contains("Recorded:   recovered from the event log"), "{out}");
        assert!(!out.contains("Question:"), "{out}");
        // Five of five cycles: resume needs --add-cycles, so that is what is offered.
        assert!(out.contains("foundry campaign resume harden-auth --add-cycles <n>"), "{out}");
        let decide = out.find("decide harden-auth").expect("decide offered");
        let cancel = out.find("cancel harden-auth").expect("cancel offered");
        assert!(decide < cancel, "options follow the status table order:\n{out}");
    }

    #[test]
    fn escalated_detail_without_any_recorded_reason_says_so() {
        let out = campaign_detail_proto(&escalated_detail(None));
        assert!(
            out.contains(&format!("Escalation:\n  Reason:     {REASON_NOT_RECORDED}\n")),
            "{out}"
        );
        assert!(out.contains("Owner options:"), "{out}");
    }

    #[test]
    fn a_non_escalated_campaign_prints_no_escalation_but_its_own_options() {
        let out = campaign_detail_proto(&proto_detail_fixture());
        assert!(!out.contains("Escalation:"), "{out}");
        assert!(out.contains("foundry campaign advance harden-auth"), "{out}");
        let done = crate::proto::CampaignDetail {
            status: "completed".to_string(),
            ..proto_detail_fixture()
        };
        assert!(!campaign_detail_proto(&done).contains("Owner options:"));
    }

    #[test]
    fn show_json_carries_the_escalation_options_and_cycles_from_the_same_data() {
        let parsed: serde_json::Value = serde_json::from_str(&campaign_show_json(
            &escalated_detail(Some(logged_escalation())),
            &[],
        ))
        .expect("parses");
        assert_eq!(parsed["open_escalation"]["reason"], "campaign cycle budget exhausted (5)");
        assert_eq!(parsed["open_escalation"]["from_event_log"], true);
        assert!(parsed["open_escalation"].get("question").is_none());
        assert_eq!(
            parsed["owner_options"][1]["command"],
            "foundry campaign resume harden-auth --add-cycles <n>"
        );
        assert_eq!(parsed["cycles"], serde_json::json!([]));
        assert_eq!(parsed["authorized_by"], "stacey");
        assert!(parsed.get("agent_provider").is_none(), "an empty optional is absent");

        let active: serde_json::Value =
            serde_json::from_str(&campaign_show_json(&proto_detail_fixture(), &[])).unwrap();
        assert!(active.get("open_escalation").is_none());
    }

    #[test]
    fn member_details_expose_stage_overrides_and_the_initiative_budget_owner() {
        let mut detail = escalated_detail(Some(logged_escalation()));
        detail.initiative = Some("shared-outcome".into());
        detail.providers = Some(crate::proto::StageProviders {
            formation: Some("claude".into()),
            execution: Some("codex".into()),
            review: None,
        });
        let output = campaign_detail_proto(&detail);
        assert!(output.contains("Initiative: shared-outcome"));
        assert!(output.contains("Providers:  formation=claude, execution=codex"));
        assert!(!output.contains("--add-cycles"));
        assert!(output.contains("foundry initiative show shared-outcome"));
        let json: serde_json::Value =
            serde_json::from_str(&campaign_show_json(&detail, &[])).unwrap();
        assert_eq!(json["initiative"], "shared-outcome");
        assert_eq!(json["providers"]["formation"], "claude");
        assert_eq!(json["providers"]["execution"], "codex");
        assert!(json["providers"].get("review").is_none());
        let independent = campaign_detail_proto(&escalated_detail(Some(logged_escalation())));
        assert!(independent.contains("--add-cycles"));
        assert!(!independent.contains("initiative show"));
    }

    #[test]
    fn report_names_provider_token_totals_within_each_stage() {
        let mut data = foundry_sdk::campaign::report::aggregate(&campaign_fixture(), &[]);
        let claude = foundry_sdk::campaign::report::StageReport {
            sessions: 1,
            input_tokens: 11,
            output_tokens: 7,
            ..Default::default()
        };
        let codex = foundry_sdk::campaign::report::StageReport {
            sessions: 2,
            input_tokens: 22,
            output_tokens: 9,
            ..Default::default()
        };
        data.providers_by_stage.insert(
            "execution".into(),
            std::collections::BTreeMap::from([("claude".into(), claude), ("codex".into(), codex)]),
        );
        let output = report(&data);
        assert!(
            output.contains(
                "execution / claude: 1 sessions, 11 fresh input, 0 cached input, 7 output"
            )
        );
        assert!(
            output.contains(
                "execution / codex: 2 sessions, 22 fresh input, 0 cached input, 9 output"
            )
        );
    }

    #[test]
    fn report_prints_the_escalation_and_options_of_an_escalated_campaign() {
        use foundry_sdk::campaign::report::CampaignReport;
        let mut data: CampaignReport = serde_json::from_value(serde_json::json!({
            "name":"stuck", "status":"escalated", "cycles_dispatched":5, "cycles_landed":5,
            "stage_limits":{}, "writable_repositories":[], "external_completion_reasons":[],
            "inferred_stage_sessions":0, "incomplete_log_lines":0, "stages":{},
            "open_escalation": {"reason":"owner decision required",
                "escalated_at":"2026-10-11T03:00:00Z","cycles_completed":5,"cycles_landed":5},
            "owner_options": [{"command":"foundry campaign cancel stuck --reason \"<text>\"",
                "effect":"stop it permanently"}]
        }))
        .unwrap();
        let out = report(&data);
        assert!(out.contains("Reason:     owner decision required"), "{out}");
        assert!(out.contains("foundry campaign cancel stuck"), "{out}");
        data.status = "active".to_string();
        assert!(!report(&data).contains("Escalation:"));
    }
}
