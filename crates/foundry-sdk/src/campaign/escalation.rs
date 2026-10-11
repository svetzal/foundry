//! Why an escalated campaign waits on its owner.
//!
//! A campaign that escalates stops and waits for a person. The person needs
//! the reason to act on it, so the reason is kept in two places:
//!
//! - **On the campaign record**, as [`Campaign::open_escalation`]. Formation
//!   writes it when it moves the campaign to `escalated`, in the same store
//!   save as the status. It is present only while the campaign is
//!   `escalated`: every transition that leaves `escalated` clears it, because
//!   a reason for a question already answered is stale. The durable history of
//!   every escalation stays in the event log.
//! - **In the event log**, as the `campaign_escalated` event Formation emits
//!   beside the status change.
//!
//! A campaign that escalated before the record existed has a status of
//! `escalated` and no record. [`resolve_open_escalations`] fills those from the
//! newest `campaign_escalated` event for the campaign, and marks the result
//! [`CampaignEscalation::from_event_log`]. It changes only the in-memory copy
//! the reader holds. It never writes the store, so a read stays a read.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Campaign, CampaignStatus};
use crate::event::{Event, EventType};
use crate::payload::CampaignTerminalPayload;
use crate::work_item_events::{EventLogReadError, log_files};

/// The reason Formation gives when a run result reaches a campaign that is
/// already `escalated`.
///
/// It restates the status, not why the campaign stopped, so it never replaces
/// the open escalation's reason and the event-log fallback skips it.
pub const REESCALATION_REASON: &str = "campaign is escalated and requires owner resumption";

/// Why a campaign is waiting on its owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CampaignEscalation {
    /// The reason Formation recorded, as the `campaign_escalated` event
    /// carries it.
    pub reason: String,
    /// When the campaign escalated.
    pub escalated_at: DateTime<Utc>,
    /// Dispatched cycles at the escalation.
    pub cycles_completed: u64,
    /// Cycles whose work had landed at the escalation.
    pub cycles_landed: u64,
    /// The task agent's question, when a task stopped with
    /// `blocked_on_decision`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// The choices the task agent offered with its question.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// Set on a read copy whose record was recovered from the event log
    /// because the campaign escalated before Foundry kept the record. Foundry
    /// never persists a recovered record.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_event_log: bool,
}

/// Fill [`Campaign::open_escalation`] for every `escalated` campaign that has
/// none, from the newest `campaign_escalated` event naming it.
///
/// Reads the event log only when such a campaign exists. A campaign the log
/// has no event for keeps `None`: the reason is unknown, and saying so is
/// better than inventing one.
///
/// # Errors
///
/// Returns [`EventLogReadError`] when the events directory exists but cannot
/// be listed, or one of its files cannot be read. A missing directory is an
/// empty log.
pub fn resolve_open_escalations(
    campaigns: &mut [Campaign],
    events_dir: &Path,
) -> Result<(), EventLogReadError> {
    let missing: BTreeSet<String> = campaigns
        .iter()
        .filter(|c| c.status == CampaignStatus::Escalated && c.open_escalation.is_none())
        .map(|c| c.name.clone())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut found = read_escalations_from_log(events_dir, &missing)?;
    for campaign in campaigns.iter_mut().filter(|c| missing.contains(&c.name)) {
        campaign.open_escalation = found.remove(&campaign.name);
    }
    Ok(())
}

/// The newest escalation per named campaign in every `*.jsonl` file of
/// `events_dir`.
///
/// # Errors
///
/// Returns [`EventLogReadError`] on an I/O fault, as
/// [`resolve_open_escalations`] describes.
pub fn read_escalations_from_log(
    events_dir: &Path,
    names: &BTreeSet<String>,
) -> Result<BTreeMap<String, CampaignEscalation>, EventLogReadError> {
    let tag = EventType::CampaignEscalated.as_str();
    let mut events = Vec::new();
    for path in log_files(events_dir)? {
        let bytes = std::fs::read(&path).map_err(|source| EventLogReadError {
            path: path.clone(),
            source,
        })?;
        for (index, raw) in bytes.split(|byte| *byte == b'\n').enumerate() {
            let line = String::from_utf8_lossy(raw);
            if !line.contains(tag.as_str()) {
                continue;
            }
            match parse_escalated(line.trim(), &tag) {
                Ok(Some(event)) => events.push(event),
                Ok(None) => {}
                Err(error) => {
                    // Best-effort: one damaged line must not hide the reason
                    // a well-formed line elsewhere in the log still carries.
                    tracing::warn!(
                        path = %path.display(),
                        line = index + 1,
                        %error,
                        "skipping malformed event log line while reading campaign escalations"
                    );
                }
            }
        }
    }
    Ok(newest_escalations(events, names))
}

/// Parse one line, returning it only when it is a `campaign_escalated` event.
/// Other event types that mention the name in their payload are not errors.
fn parse_escalated(line: &str, tag: &str) -> Result<Option<Event>, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(line)?;
    if value["event_type"] != tag {
        return Ok(None);
    }
    serde_json::from_value(value).map(Some)
}

/// The newest escalation per named campaign among `events`.
///
/// Selection is by exact payload `campaign`. The newest `occurred_at` wins;
/// on a tie the later event in iteration order wins. An event whose reason is
/// [`REESCALATION_REASON`] is skipped, because it does not say why the
/// campaign stopped.
#[must_use]
pub fn newest_escalations(
    events: impl IntoIterator<Item = Event>,
    names: &BTreeSet<String>,
) -> BTreeMap<String, CampaignEscalation> {
    let mut newest: BTreeMap<String, CampaignEscalation> = BTreeMap::new();
    for event in events {
        if event.event_type != EventType::CampaignEscalated {
            continue;
        }
        let Ok(payload) = event.parse_payload::<CampaignTerminalPayload>() else {
            continue;
        };
        if !names.contains(&payload.campaign) || payload.reason == REESCALATION_REASON {
            continue;
        }
        let candidate = CampaignEscalation {
            reason: payload.reason,
            escalated_at: event.occurred_at,
            cycles_completed: payload.cycles_completed,
            cycles_landed: payload.cycles_landed,
            question: None,
            choices: Vec::new(),
            from_event_log: true,
        };
        match newest.get(&payload.campaign) {
            Some(held) if held.escalated_at > candidate.escalated_at => {}
            _ => {
                newest.insert(payload.campaign, candidate);
            }
        }
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::throttle::Throttle;

    fn escalated_event(campaign: &str, reason: &str, second: i64) -> Event {
        let mut event = Event::new(
            EventType::CampaignEscalated,
            "proj".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignTerminalPayload {
                campaign: campaign.to_string(),
                project: "proj".to_string(),
                reason: reason.to_string(),
                cycles_completed: 3,
                cycles_landed: 2,
            })
            .unwrap(),
        );
        event.occurred_at = DateTime::from_timestamp(second, 0).unwrap();
        event
    }

    fn campaign(name: &str, status: CampaignStatus) -> Campaign {
        let mut campaign: Campaign = serde_json::from_value(serde_json::json!({
            "name": name, "project": "proj", "mission": "ship",
            "done_evidence": [{"kind": "review", "statement": "shipped"}]
        }))
        .unwrap();
        campaign.status = status;
        campaign
    }

    fn write_log(dir: &Path, file: &str, events: &[Event]) {
        let mut text = String::new();
        for event in events {
            text.push_str(&serde_json::to_string(event).unwrap());
            text.push('\n');
        }
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(file), text).unwrap();
    }

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn newest_escalation_wins_and_a_reescalation_never_replaces_the_reason() {
        let found = newest_escalations(
            vec![
                escalated_event("c", "budget exhausted (6)", 200),
                escalated_event("c", "first stop", 100),
                escalated_event("c", REESCALATION_REASON, 300),
                escalated_event("other", "not asked for", 400),
            ],
            &names(&["c"]),
        );
        assert_eq!(found.len(), 1);
        let held = &found["c"];
        assert_eq!(held.reason, "budget exhausted (6)");
        assert_eq!((held.cycles_completed, held.cycles_landed), (3, 2));
        assert!(held.from_event_log);
    }

    #[test]
    fn resolve_fills_only_escalated_campaigns_without_a_record_across_monthly_files() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("events");
        write_log(&events, "2026-09.jsonl", &[escalated_event("legacy", "older stop", 100)]);
        write_log(
            &events,
            "2026-10.jsonl",
            &[
                escalated_event("legacy", "budget exhausted (6)", 200),
                escalated_event("active", "an old escalation", 150),
            ],
        );
        let mut damaged = std::fs::read_to_string(events.join("2026-10.jsonl")).unwrap();
        damaged.push_str("{\"event_type\":\"campaign_escalated\", broken\n");
        std::fs::write(events.join("2026-10.jsonl"), damaged).unwrap();

        let recorded = CampaignEscalation {
            reason: "kept as recorded".to_string(),
            escalated_at: DateTime::from_timestamp(50, 0).unwrap(),
            cycles_completed: 1,
            cycles_landed: 0,
            question: None,
            choices: vec![],
            from_event_log: false,
        };
        let mut with_record = campaign("recorded", CampaignStatus::Escalated);
        with_record.open_escalation = Some(recorded.clone());
        let mut campaigns = vec![
            campaign("legacy", CampaignStatus::Escalated),
            campaign("active", CampaignStatus::Active),
            campaign("unknown", CampaignStatus::Escalated),
            with_record,
        ];

        resolve_open_escalations(&mut campaigns, &events).unwrap();

        let legacy = campaigns[0].open_escalation.as_ref().unwrap();
        assert_eq!(legacy.reason, "budget exhausted (6)");
        assert!(legacy.from_event_log);
        assert!(campaigns[1].open_escalation.is_none(), "only escalated campaigns resolve");
        assert!(campaigns[2].open_escalation.is_none(), "no event, no invented reason");
        assert_eq!(campaigns[3].open_escalation.as_ref(), Some(&recorded));
    }

    #[test]
    fn resolve_reads_nothing_when_no_campaign_needs_it_and_tolerates_a_missing_log() {
        let mut campaigns = vec![campaign("active", CampaignStatus::Active)];
        // An unreadable path would fail if it were read.
        resolve_open_escalations(&mut campaigns, Path::new("/dev/null/not-a-dir")).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut escalated = vec![campaign("c", CampaignStatus::Escalated)];
        resolve_open_escalations(&mut escalated, &dir.path().join("absent")).unwrap();
        assert!(escalated[0].open_escalation.is_none());
    }

    #[test]
    fn a_record_round_trips_and_omits_unset_fields() {
        let record = CampaignEscalation {
            reason: "r".to_string(),
            escalated_at: DateTime::from_timestamp(1, 0).unwrap(),
            cycles_completed: 1,
            cycles_landed: 0,
            question: None,
            choices: vec![],
            from_event_log: false,
        };
        let json = serde_json::to_value(&record).unwrap();
        assert!(json.get("question").is_none());
        assert!(json.get("choices").is_none());
        assert!(json.get("from_event_log").is_none());
        assert_eq!(serde_json::from_value::<CampaignEscalation>(json).unwrap(), record);
    }
}
