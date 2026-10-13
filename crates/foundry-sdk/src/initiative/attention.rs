//! Current unanswered questions and inactive initiatives are digest evidence, independent of event watermarks.
use chrono::{DateTime, Duration, Utc};
use std::fmt::Write;

use crate::campaign::{CampaignStatus, CampaignStore};
use crate::payload::OpsEventDigest;
use crate::work_item::{WorkItemKind, WorkItemStore};
use crate::work_source::WorkSourceKind;

impl CampaignStore {
    /// Read current facts on every digest tick; an old event watermark cannot hide an unanswered question.
    pub fn initiative_attention_events(
        &self,
        ledger: &WorkItemStore,
        now: DateTime<Utc>,
    ) -> super::Result<Vec<OpsEventDigest>> {
        let mut events = Vec::new();
        for record in &self.initiatives {
            if record.status.is_terminal() {
                continue;
            }
            let initiative = self.initiative_view(&record.name)?;
            for question in initiative.open_questions.iter().chain(&initiative.deferred_questions) {
                if now.signed_duration_since(question.opened_at) <= Duration::days(1) {
                    continue;
                }
                events.push(attention_event(
                    format!("initiative-question-{}", question.id),
                    "initiative_question_overdue", now,
                    format!("Initiative '{}' waits on its owner for {} hours: {} (members: {}). Answer with foundry initiative decide {} --decision '<answer>'{}",
                        initiative.name, now.signed_duration_since(question.opened_at).num_hours(),
                        question.question, question.members.join(", "), initiative.name,
                        question.members.iter().fold(String::new(), |mut args, member| { let _ = write!(args, " --member {member}"); args })),
                ));
            }
            let owned: Vec<_> = ledger
                .items
                .iter()
                .filter(|item| {
                    item.campaign_name().is_some_and(|name| {
                        initiative.members.iter().any(|member| member.name == name)
                    }) || initiative
                        .task_dispatches
                        .iter()
                        .any(|receipt| receipt.work_item == item.id)
                        || item.source.as_ref().is_some_and(|source| {
                            source.kind == WorkSourceKind::Initiative
                                && source.reference == initiative.name
                                && matches!(
                                    item.kind,
                                    WorkItemKind::Task | WorkItemKind::CampaignCycle
                                )
                        })
                })
                .collect();
            let active = owned.iter().any(|item| item.occupies_slot())
                || initiative
                    .members
                    .iter()
                    .filter_map(|member| self.find(&member.name))
                    .any(|campaign| campaign.status == CampaignStatus::Active);
            let last_activity = initiative
                .created_at
                .into_iter()
                .chain(
                    owned
                        .iter()
                        .flat_map(|item| [item.started_at, item.settled_at].into_iter().flatten()),
                )
                .chain(
                    initiative
                        .members
                        .iter()
                        .filter_map(|member| self.find(&member.name))
                        .filter_map(|campaign| {
                            campaign
                                .open_escalation
                                .as_ref()
                                .map(|escalation| escalation.escalated_at)
                        }),
                )
                .max();
            if !active
                && last_activity
                    .is_some_and(|at| now.signed_duration_since(at) > Duration::days(30))
            {
                events.push(attention_event(format!("initiative-inactive-{}", initiative.name),
                    "initiative_inactive", now, format!("Initiative '{}' has had no active member for over 30 days. Review its outcome and continue or cancel it: foundry initiative show {}", initiative.name, initiative.name)));
            }
        }
        Ok(events)
    }
}

fn attention_event(
    id: String,
    event_type: &str,
    now: DateTime<Utc>,
    summary: String,
) -> OpsEventDigest {
    OpsEventDigest {
        id,
        event_type: event_type.into(),
        occurred_at: now.to_rfc3339(),
        domain: "infrastructure".into(),
        urgency: Some("P1".into()),
        summary: Some(summary),
        client: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::initiative::tests::store;

    #[test]
    fn strict_day_threshold_and_answers_control_current_anomalies() {
        let mut store = store();
        let now = Utc::now();
        store.record_question("sample", "Which API version?", &[]).unwrap();
        store.find_initiative_mut("sample").unwrap().open_questions[0].opened_at =
            now - Duration::days(1);
        assert!(
            store
                .initiative_attention_events(&WorkItemStore::default(), now)
                .unwrap()
                .is_empty()
        );
        let events = store
            .initiative_attention_events(&WorkItemStore::default(), now + Duration::seconds(1))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "initiative_question_overdue");
        assert!(events[0].summary.as_ref().unwrap().contains("--member one --member two"));
        store.decide_initiative("sample", "Use v1", &[], now).unwrap();
        assert!(
            store
                .initiative_attention_events(&WorkItemStore::default(), now + Duration::days(2))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn inactivity_needs_a_known_age_and_no_active_member() {
        let mut store = store();
        let now = Utc::now();
        store.find_initiative_mut("sample").unwrap().created_at = None;
        assert!(
            store
                .initiative_attention_events(&WorkItemStore::default(), now)
                .unwrap()
                .is_empty()
        );
        store.find_initiative_mut("sample").unwrap().created_at = Some(now - Duration::days(30));
        assert!(
            store
                .initiative_attention_events(&WorkItemStore::default(), now)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .initiative_attention_events(&WorkItemStore::default(), now + Duration::seconds(1))
                .unwrap()[0]
                .event_type,
            "initiative_inactive"
        );
        store.find_mut("one").unwrap().status = CampaignStatus::Active;
        assert!(
            store
                .initiative_attention_events(&WorkItemStore::default(), now + Duration::days(2))
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn owner_decisions_do_not_hide_inactivity_but_member_settlements_do() {
        use crate::work_item::{WorkItem, WorkItemKind, WorkItemSpec, WorkLane};
        use crate::work_source::WorkSource;
        let mut store = store();
        let now = Utc::now();
        store.find_initiative_mut("sample").unwrap().created_at = Some(now - Duration::days(40));
        store
            .decide_initiative("sample", "Still considering the scope", &[], now)
            .unwrap();
        assert!(
            store
                .initiative_attention_events(&WorkItemStore::default(), now)
                .unwrap()
                .iter()
                .any(|event| event.event_type == "initiative_inactive")
        );
        let mut item = WorkItem::submitted(
            WorkItemSpec {
                project: "sample-rs".into(),
                objective: "Ship member behavior".into(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Campaign,
                origin: "initiative".into(),
                trace_id: None,
            },
            now - Duration::days(35),
        );
        item.source = Some(WorkSource::initiative("sample", 1));
        item.start(now - Duration::days(35));
        item.settle_landed("Member landed", now - Duration::days(2));
        let ledger = WorkItemStore {
            items: vec![item],
            ..Default::default()
        };
        assert!(store.initiative_attention_events(&ledger, now).unwrap().is_empty());
    }
    #[test]
    fn running_and_recently_settled_assessments_do_not_count_as_member_activity() {
        use crate::event::{Event, EventType};
        use crate::throttle::Throttle;
        use crate::work_item::{WorkItem, WorkItemSpec, WorkLane};
        use crate::work_source::WorkSource;
        let mut store = store();
        let now = Utc::now();
        store.find_initiative_mut("sample").unwrap().created_at = Some(now - Duration::days(40));
        let root = Event::new(
            EventType::InitiativeAdvanceRequested,
            "system".into(),
            Throttle::Full,
            serde_json::json!({"initiative":"sample"}),
        );
        let mut assessment = WorkItem::queued(
            WorkItemSpec {
                project: "system".into(),
                objective: "Assess current initiative".into(),
                kind: WorkItemKind::Workflow,
                lane: WorkLane::Campaign,
                origin: "owner advance".into(),
                trace_id: None,
            },
            root,
            now,
        );
        assessment.source = Some(WorkSource::initiative("sample", 0));
        assessment.start(now);
        for settled in [false, true] {
            if settled {
                assessment.settle_landed("waiting on owner", now);
            }
            let ledger = WorkItemStore {
                items: vec![assessment.clone()],
                ..Default::default()
            };
            assert!(
                store
                    .initiative_attention_events(&ledger, now)
                    .unwrap()
                    .iter()
                    .any(|event| event.event_type == "initiative_inactive")
            );
        }
    }
}
