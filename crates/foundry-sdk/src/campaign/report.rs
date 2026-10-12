//! Campaign accounting from durable events, with unknown spend kept explicit.
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Campaign;

/// Recorded activity in one agent stage. Cached tokens are separate from fresh input.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StageReport {
    pub sessions: u64,
    pub running_sessions: u64,
    pub unmeasured_sessions: u64,
    pub elapsed_ms: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub known_list_usd: f64,
    pub unpriced_models: BTreeSet<String>,
    #[serde(default)]
    pub pricing_limitations: BTreeSet<String>,
}

mod formation;
pub use formation::{
    FormationDecisionReport, FormationSessionReport, FormationToolActivity,
    NativeFormationObservation,
};
use formation::{find_native_log, read_native_observation, read_tool_activity};

/// Accounting for the campaign, excluding unrelated digest sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignReport {
    pub name: String,
    pub status: String,
    pub cycles_dispatched: u64,
    pub stage_limits: super::StageBudget,
    pub writable_repositories: Vec<String>,
    pub cycles_landed: u64,
    pub external_completion_reasons: Vec<String>,
    pub inferred_stage_sessions: u64,
    pub incomplete_log_lines: u64,
    pub stages: BTreeMap<String, StageReport>,
    /// The effective provider for each stage, including explicit unknown legacy records.
    #[serde(default)]
    pub providers_by_stage: BTreeMap<String, BTreeMap<String, StageReport>>,
    #[serde(default)]
    pub formation_sessions: Vec<FormationSessionReport>,
    #[serde(default)]
    pub formation_decisions: Vec<FormationDecisionReport>,
    /// Why an `escalated` campaign waits on its owner: the campaign record,
    /// or for an escalation that predates the record, the newest
    /// `campaign_escalated` event. Absent for any other status, and when no
    /// reason was ever recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_escalation: Option<super::CampaignEscalation>,
    /// The controls the owner can run on the campaign now.
    #[serde(default)]
    pub owner_options: Vec<super::OwnerOption>,
}

/// Read the daemon's event logs. Invalid complete records are errors, not zero spend.
pub fn read_report(campaign: &Campaign, directory: &Path) -> anyhow::Result<CampaignReport> {
    let mut events = Vec::new();
    let mut incomplete = 0;
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(entries) = entries {
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let mut reader = std::io::BufReader::new(std::fs::File::open(&path)?);
            let mut line = String::new();
            while reader.read_line(&mut line)? != 0 {
                match serde_json::from_str::<Value>(&line) {
                    Ok(event) => {
                        if event["project"] == campaign.project
                            || event["payload"]["campaign"] == campaign.name
                        {
                            events.push(event);
                        }
                    }
                    Err(_) if !line.ends_with('\n') => incomplete += 1,
                    Err(error) => {
                        return Err(anyhow::anyhow!(
                            "invalid event in {}: {error}",
                            path.display()
                        ));
                    }
                }
                line.clear();
            }
        }
    }
    let mut report = aggregate(campaign, &events);
    report.incomplete_log_lines = incomplete;
    for session in &mut report.formation_sessions {
        match read_tool_activity(Path::new(&session.source_log_path)) {
            Ok(activity) => session.tool_activity = Some(activity),
            Err(error) => session.transcript_error = Some(error.to_string()),
        }
    }
    let native_root = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".codex"))
        })
        .map(|root| root.join("sessions"));
    for session in &mut report.formation_sessions {
        let observed = session
            .tool_activity
            .as_ref()
            .and_then(|activity| activity.native_thread_id.as_deref());
        if let (Some(root), Some(id)) = (&native_root, observed) {
            match find_native_log(root, id).and_then(|path| read_native_observation(&path)) {
                Ok(value) => session.native_observation = Some(value),
                Err(error) => session.native_observation_error = Some(error.to_string()),
            }
        }
    }
    Ok(report)
}

fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// Fold events independent of file order. Session IDs prevent duplicated usage.
#[must_use]
pub fn aggregate(campaign: &Campaign, events: &[Value]) -> CampaignReport {
    let traces: BTreeSet<&str> = events
        .iter()
        .filter(|e| e["payload"]["campaign"] == campaign.name)
        .filter_map(|e| e["trace_id"].as_str())
        .collect();
    let ends: BTreeMap<&str, &Value> = events
        .iter()
        .filter(|e| e["event_type"] == "agent_session_ended")
        .filter_map(|e| e["payload"]["session_id"].as_str().map(|id| (id, e)))
        .collect();
    let mut report = initial_report(campaign, events);
    let mut seen = BTreeSet::new();
    for event in events {
        if event["event_type"] != "agent_session_started"
            || event["project"] != campaign.project
            || !event["trace_id"].as_str().is_some_and(|t| traces.contains(t))
        {
            continue;
        }
        let p = &event["payload"];
        let Some(id) = p["session_id"].as_str() else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        let role = p["stage"].as_str().filter(|s| !s.is_empty()).unwrap_or_else(|| {
            report.inferred_stage_sessions += 1;
            if p["access"] == "full" {
                "execution"
            } else if p["tier"] == "deep" {
                "review"
            } else {
                "formation"
            }
        });
        if role == "formation" {
            let finish = ends.get(id).map(|e| &e["payload"]);
            report.formation_sessions.push(FormationSessionReport {
                session_id: id.into(),
                trace_id: text(&event["trace_id"]),
                started_at: text(&p["started_at"]),
                ended_at: finish.and_then(|e| e["ended_at"].as_str().map(str::to_owned)),
                status: finish.map_or_else(|| "running".into(), |e| text(&e["status"])),
                source_log_path: text(&p["source_log_path"]),
                prompt_bytes: p["prompt_bytes"].as_u64(),
                usage: finish.and_then(|e| serde_json::from_value(e["usage"].clone()).ok()),
                tool_activity: None,
                transcript_error: None,
                native_observation: None,
                native_observation_error: None,
            });
        }
        let finish = ends.get(id).map(|event| &event["payload"]);
        account_session(report.stages.entry(role.into()).or_default(), p, finish);
        let provider = p["provider"].as_str().unwrap_or_else(|| match p["agent_type"].as_str() {
            Some("claude-code") => "claude",
            Some("codex") => "codex",
            Some("opencode") => "opencode",
            _ => "unknown",
        });
        account_session(
            report
                .providers_by_stage
                .entry(role.into())
                .or_default()
                .entry(provider.into())
                .or_default(),
            p,
            finish,
        );
    }
    report
        .formation_sessions
        .sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.session_id.cmp(&b.session_id)));
    report
        .formation_decisions
        .sort_by(|a, b| a.occurred_at.cmp(&b.occurred_at).then(a.event_id.cmp(&b.event_id)));
    report
}

fn initial_report(campaign: &Campaign, events: &[Value]) -> CampaignReport {
    CampaignReport {
        name: campaign.name.clone(),
        stage_limits: campaign.budget.stages.clone(),
        writable_repositories: if campaign.writable_repositories.is_empty() {
            vec![campaign.project.clone()]
        } else {
            campaign.writable_repositories.clone()
        },
        status: campaign.status.to_string(),
        cycles_dispatched: campaign.cycles_completed,
        cycles_landed: campaign.cycles_landed,
        external_completion_reasons: campaign
            .owner_decisions
            .iter()
            .filter(|d| d.decision.starts_with("Completed externally:"))
            .map(|d| d.decision.clone())
            .collect(),
        inferred_stage_sessions: 0,
        incomplete_log_lines: 0,
        open_escalation: open_escalation(campaign, events),
        owner_options: campaign.owner_options(),
        stages: BTreeMap::new(),
        providers_by_stage: BTreeMap::new(),
        formation_sessions: Vec::new(),
        formation_decisions: events
            .iter()
            .filter(|e| {
                e["event_type"] == "campaign_advance_completed"
                    && e["payload"]["campaign"] == campaign.name
            })
            .map(|e| {
                let p = &e["payload"];
                FormationDecisionReport {
                    event_id: text(&e["id"]),
                    trace_id: text(&e["trace_id"]),
                    occurred_at: text(&e["occurred_at"]),
                    decision: text(&p["decision"]),
                    reason: text(&p["reason"]),
                    last_task_run_event_id: p["last_task_run_event_id"].as_str().map(str::to_owned),
                    prompt_bytes: p["prompt"].as_str().map(str::len),
                }
            })
            .collect(),
    }
}

/// The open escalation of an `escalated` campaign, with the same event-log
/// fallback [`super::escalation::resolve_open_escalations`] applies.
fn open_escalation(campaign: &Campaign, events: &[Value]) -> Option<super::CampaignEscalation> {
    if campaign.status != super::CampaignStatus::Escalated {
        return None;
    }
    if campaign.open_escalation.is_some() {
        return campaign.open_escalation.clone();
    }
    let logged = events
        .iter()
        .filter(|e| e["event_type"] == "campaign_escalated")
        .filter_map(|e| serde_json::from_value::<crate::event::Event>(e.clone()).ok());
    let name = BTreeSet::from([campaign.name.clone()]);
    super::escalation::newest_escalations(logged, &name).remove(&campaign.name)
}

fn account_session(stage: &mut StageReport, start: &Value, finish: Option<&Value>) {
    stage.sessions += 1;
    let Some(finish) = finish else {
        stage.running_sessions += 1;
        return;
    };
    if let (Some(start), Some(end)) =
        (timestamp(&start["started_at"]), timestamp(&finish["ended_at"]))
    {
        stage.elapsed_ms += u64::try_from((end - start).num_milliseconds()).unwrap_or(0);
    }
    add_terminal_accounting(stage, finish);
}

fn add_terminal_accounting(stage: &mut StageReport, finish: &Value) {
    if let Some(models) = finish["usage"]["models"].as_array() {
        for model in models {
            stage.input_tokens += model["input_tokens"].as_u64().unwrap_or(0);
            stage.cached_input_tokens += model["cache_read_tokens"].as_u64().unwrap_or(0);
            stage.cache_write_tokens += model["cache_write_tokens"].as_u64().unwrap_or(0);
            stage.output_tokens += model["output_tokens"].as_u64().unwrap_or(0);
        }
    } else {
        stage.unmeasured_sessions += 1;
    }
    if finish["cost"]["list_usd"].as_f64().is_none()
        && let Some(models) = finish["usage"]["models"].as_array()
    {
        stage.unpriced_models.extend(
            models
                .iter()
                .map(|model| model["model"].as_str().unwrap_or("unknown").to_string()),
        );
    }
    stage.known_list_usd += finish["cost"]["list_usd"].as_f64().unwrap_or(0.0);
    if let Some(limitations) = finish["cost"]["pricing_limitations"].as_array() {
        stage
            .pricing_limitations
            .extend(limitations.iter().filter_map(Value::as_str).map(str::to_string));
    }
    if let Some(models) = finish["cost"]["unpriced_models"].as_array() {
        stage
            .unpriced_models
            .extend(models.iter().filter_map(Value::as_str).map(str::to_string));
    }
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn same_stage_accounts_actual_providers_separately() {
        let events = vec![
            serde_json::json!({"project":"repo","trace_id":"trace","event_type":"campaign_cycle_dispatched","payload":{"campaign":"test"}}),
            serde_json::json!({"project":"repo","trace_id":"trace","event_type":"agent_session_started","payload":{"session_id":"a","stage":"execution","provider":"claude","agent_type":"codex"}}),
            serde_json::json!({"project":"repo","trace_id":"trace","event_type":"agent_session_started","payload":{"session_id":"b","stage":"execution","provider":"codex"}}),
            serde_json::json!({"event_type":"agent_session_ended","payload":{"session_id":"a","usage":{"models":[{"input_tokens":11,"output_tokens":7}]}}}),
            serde_json::json!({"event_type":"agent_session_ended","payload":{"session_id":"b","usage":{"models":[{"input_tokens":22,"output_tokens":9}]}}}),
        ];
        let mut campaign = campaign();
        campaign.name = "test".into();
        campaign.project = "repo".into();
        let report = aggregate(&campaign, &events);
        assert_eq!(report.stages["execution"].input_tokens, 33);
        assert_eq!(report.providers_by_stage["execution"]["claude"].input_tokens, 11);
        assert_eq!(report.providers_by_stage["execution"]["codex"].input_tokens, 22);
    }

    use super::*;
    use serde_json::json;
    fn campaign() -> Campaign {
        serde_json::from_value(json!({"name":"c","project":"p","mission":"ship",
            "done_evidence":[{"kind":"review","statement":"shipped"}]}))
        .unwrap()
    }
    #[test]
    fn counts_sessions_once_excludes_digests_and_preserves_unknown_spend() {
        let start = json!({"event_type":"agent_session_started","project":"p","trace_id":"t",
            "payload":{"session_id":"s","stage":"formation","started_at":"2026-09-30T12:00:00Z"}});
        let events = vec![
            json!({"trace_id":"t","payload":{"campaign":"c"}}),
            start.clone(),
            start,
            json!({"event_type":"agent_session_ended","payload":{"session_id":"s","ended_at":"2026-09-30T12:00:02Z",
                "usage":{"models":[{"input_tokens":10,"cache_read_tokens":100,"output_tokens":4,"reasoning_tokens":2}]},
                "cost":{"list_usd":0.125,"unpriced_models":["unknown-model"],"pricing_limitations":["processing tier unknown"]}}}),
            json!({"event_type":"agent_session_started","project":"system","trace_id":"t","payload":{"session_id":"digest","tier":"deep"}}),
            json!({"event_type":"agent_session_started","project":"p","trace_id":"t","payload":{"session_id":"missing","access":"full"}}),
            json!({"event_type":"agent_session_ended","payload":{"session_id":"missing"}}),
        ];
        let report = aggregate(&campaign(), &events);
        let formation = &report.stages["formation"];
        assert_eq!(formation.sessions, 1);
        assert_eq!(formation.elapsed_ms, 2000);
        assert_eq!(
            (formation.input_tokens, formation.cached_input_tokens, formation.output_tokens),
            (10, 100, 4)
        );
        assert!(formation.unpriced_models.contains("unknown-model"));
        assert!((formation.known_list_usd - 0.125).abs() < f64::EPSILON);
        assert!(formation.pricing_limitations.contains("processing tier unknown"));
        assert_eq!(report.stages["execution"].unmeasured_sessions, 1);
        assert_eq!(report.inferred_stage_sessions, 1);
        assert!(!report.stages.contains_key("review"));
    }
    #[test]
    fn an_escalated_report_carries_its_reason_from_the_record_or_the_log() {
        let mut escalated = campaign();
        escalated.status = crate::campaign::CampaignStatus::Escalated;
        let mut logged = crate::event::Event::new(
            crate::event::EventType::CampaignEscalated,
            "p".to_string(),
            crate::throttle::Throttle::Full,
            json!({"campaign":"c","project":"p","reason":"budget exhausted (6)",
                "cycles_completed":6,"cycles_landed":4}),
        );
        logged.occurred_at = DateTime::from_timestamp(100, 0).unwrap();
        let events = vec![serde_json::to_value(&logged).unwrap()];

        let from_log = aggregate(&escalated, &events).open_escalation.unwrap();
        assert_eq!(from_log.reason, "budget exhausted (6)");
        assert!(from_log.from_event_log);

        escalated.open_escalation = Some(crate::campaign::CampaignEscalation {
            from_event_log: false,
            reason: "recorded".to_string(),
            ..from_log
        });
        assert_eq!(aggregate(&escalated, &events).open_escalation.unwrap().reason, "recorded");

        let report = aggregate(&escalated, &events);
        assert!(
            report
                .owner_options
                .iter()
                .any(|o| o.command.starts_with("foundry campaign cancel c")),
            "{:?}",
            report.owner_options
        );

        let active = campaign();
        assert!(aggregate(&active, &events).open_escalation.is_none());
    }

    #[test]
    fn a_stand_down_is_listed_as_stood_down_and_counts_no_cycle() {
        let events = vec![
            json!({"id":"e1","trace_id":"t1","occurred_at":"2026-10-11T10:00:00Z",
                "event_type":"campaign_advance_completed",
                "payload":{"campaign":"c","project":"p","cycles_completed":1,"cycles_landed":1,
                    "success":true,"decision":"advance","objective":"o","reason":"gap"}}),
            json!({"id":"e2","trace_id":"t2","occurred_at":"2026-10-11T11:00:00Z",
                "event_type":"campaign_advance_completed",
                "payload":{"campaign":"c","project":"p","cycles_completed":1,"cycles_landed":1,
                    "success":true,"decision":"stood_down","campaign_status":"paused",
                    "reason":"campaign 'c' became paused while it formed"}}),
        ];
        let mut stopped = campaign();
        stopped.cycles_completed = 1;
        let report = aggregate(&stopped, &events);
        let decisions: Vec<_> = report
            .formation_decisions
            .iter()
            .map(|d| (d.decision.as_str(), d.reason.as_str()))
            .collect();
        assert_eq!(
            decisions,
            [
                ("advance", "gap"),
                ("stood_down", "campaign 'c' became paused while it formed")
            ]
        );
        assert_eq!(report.cycles_dispatched, 1, "cycles come from the record, not the events");
    }

    #[test]
    fn damaged_complete_logs_fail_but_incomplete_appends_are_visible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-09.jsonl");
        std::fs::write(&path, "broken\n").unwrap();
        assert!(read_report(&campaign(), dir.path()).is_err());
        std::fs::write(&path, "{\"event_type\":").unwrap();
        assert_eq!(read_report(&campaign(), dir.path()).unwrap().incomplete_log_lines, 1);
    }
}
