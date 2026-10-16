//! Which initiative members hold one of its `max_active_members` slots.
//!
//! Every admission check counts occupancy here, so a member can never be
//! counted one way by steering and another way by formation or dispatch.

use std::collections::BTreeSet;

use super::{CampaignStore, Result};
use crate::work_item::WorkItemStore;

impl CampaignStore {
    /// Initiative members with live work in the ledger.
    ///
    /// Live work is an item that still holds execution capacity, or one still
    /// waiting to start (submitted, queued or held). An item belongs to the
    /// member campaign recorded on its root, or to the task slot whose dispatch
    /// receipt names it. A waiting item of a completed or cancelled member
    /// campaign is not live: it will never start. A running cycle of such a
    /// member is live until it settles, because it occupies a real worker.
    /// The result can name members of any initiative;
    /// [`Self::occupied_member_slots`] keeps only the members of the one it
    /// counts.
    #[must_use]
    pub fn ledger_active_members(&self, ledger: &WorkItemStore) -> Vec<String> {
        ledger
            .items
            .iter()
            .filter_map(|item| {
                let member = item.campaign_name().map(str::to_owned).or_else(|| {
                    self.initiatives
                        .iter()
                        .flat_map(|initiative| &initiative.task_dispatches)
                        .find(|receipt| receipt.work_item == item.id)
                        .map(|receipt| receipt.member.clone())
                })?;
                let live = item.occupies_slot()
                    || (item.state.is_waiting() && !self.member_campaign_terminal(&member));
                live.then_some(member)
            })
            .collect()
    }

    /// Whether `member` names a completed or cancelled campaign.
    fn member_campaign_terminal(&self, member: &str) -> bool {
        self.find(member).is_some_and(|campaign| campaign.status.is_terminal())
    }

    /// The members of initiative `name` that occupy an active-member slot.
    ///
    /// A member occupies a slot while it has live work named in
    /// `active_members` (from the ledger, see [`Self::ledger_active_members`],
    /// or the workflow tracker), a reserved admission, or a member campaign
    /// cycle in flight. A completed or cancelled member campaign occupies a
    /// slot only through live work: a cycle still running after a graceful
    /// cancellation uses a real worker until it settles. Its reservation and
    /// its unsettled last objective are history, not capacity in use.
    pub fn occupied_member_slots(
        &self,
        name: &str,
        active_members: &[String],
    ) -> Result<BTreeSet<String>> {
        let initiative = self.require_initiative(name)?;
        let is_member =
            |member: &str| initiative.members.iter().any(|definition| definition.name == member);
        let recorded = initiative
            .reservations
            .iter()
            .map(|reservation| reservation.member.clone())
            .filter(|member| !self.member_campaign_terminal(member));
        // has_cycle_in_flight is already false for a terminal campaign.
        let in_flight = self
            .campaigns
            .iter()
            .filter(|campaign| {
                campaign.initiative.as_deref() == Some(name) && campaign.has_cycle_in_flight()
            })
            .map(|campaign| campaign.name.clone());
        Ok(active_members
            .iter()
            .filter(|member| is_member(member))
            .cloned()
            .chain(recorded)
            .chain(in_flight)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::campaign::{CampaignCycle, CampaignStatus};
    use crate::initiative::{MemberReservation, ReservationKind, TaskDispatch};
    use crate::work_item::{WorkItem, WorkItemKind, WorkItemSpec, WorkLane};

    /// Three members over one project with the default limit of two slots.
    fn store() -> CampaignStore {
        let mut store = CampaignStore::default();
        for name in ["frontend", "backend", "docs"] {
            let campaign = serde_json::from_value(serde_json::json!({
                "name": name, "project": "sample", "mission": "Ship the behavior",
                "authorized_by": "Stacey",
                "done_evidence": [{"kind": "review", "statement": "It works"}]
            }))
            .unwrap();
            store.add(campaign).unwrap();
        }
        let initiative = serde_json::from_value(serde_json::json!({
            "name": "shared", "outcome": "All work", "projects": ["sample"],
            "authorized_by": "Stacey",
            "members": [
                {"kind": "campaign", "name": "frontend"},
                {"kind": "campaign", "name": "backend"},
                {"kind": "campaign", "name": "docs"}
            ],
            "done_evidence": [{"kind": "review", "statement": "All work"}]
        }))
        .unwrap();
        store.add_initiative(initiative, &["sample".into()]).unwrap();
        store
    }

    fn dispatch_unsettled_cycle(store: &mut CampaignStore, member: &str) {
        let campaign = store.find_mut(member).unwrap();
        campaign.status = CampaignStatus::Active;
        campaign.cycles_completed += 1;
        campaign.record_dispatched_objective("Deliver the next slice".into());
    }

    fn running_item(campaign: &str) -> WorkItem {
        let mut item = WorkItem::submitted(
            WorkItemSpec {
                project: "sample".into(),
                objective: "Deliver the next slice".into(),
                kind: WorkItemKind::CampaignCycle,
                lane: WorkLane::Campaign,
                origin: "test".into(),
                trace_id: None,
            },
            Utc::now(),
        );
        item.source = Some(crate::work_source::WorkSource::campaign(campaign, 1));
        item.start(Utc::now());
        item
    }

    fn queued_item(campaign: &str) -> WorkItem {
        let mut item = running_item(campaign);
        item.state = crate::work_item::WorkItemState::Queued;
        item.reason = "queued".into();
        item.started_at = None;
        item
    }

    fn occupied(store: &CampaignStore, active: &[String]) -> Vec<String> {
        store.occupied_member_slots("shared", active).unwrap().into_iter().collect()
    }

    #[test]
    fn a_cancelled_member_with_an_unsettled_last_objective_frees_its_slot() {
        let mut store = store();
        dispatch_unsettled_cycle(&mut store, "frontend");
        dispatch_unsettled_cycle(&mut store, "backend");
        store.find_mut("frontend").unwrap().status = CampaignStatus::Cancelled;

        assert_eq!(occupied(&store, &[]), ["backend"]);
        store.enforce_member_eligibility("shared", "docs", &[]).unwrap();
    }

    #[test]
    fn a_completed_member_with_an_unsettled_last_objective_frees_its_slot() {
        let mut store = store();
        dispatch_unsettled_cycle(&mut store, "frontend");
        dispatch_unsettled_cycle(&mut store, "backend");
        store.find_mut("frontend").unwrap().status = CampaignStatus::Completed;

        assert_eq!(occupied(&store, &[]), ["backend"]);
        store.enforce_member_eligibility("shared", "docs", &[]).unwrap();
    }

    #[test]
    fn a_terminal_member_frees_its_slot_despite_a_stale_reservation_or_waiting_item() {
        let mut store = store();
        dispatch_unsettled_cycle(&mut store, "backend");
        store
            .find_initiative_mut("shared")
            .unwrap()
            .reservations
            .push(MemberReservation {
                member: "frontend".into(),
                operation_id: "wi_withdrawn".into(),
                kind: ReservationKind::Formation,
            });
        store.find_mut("frontend").unwrap().status = CampaignStatus::Cancelled;
        let mut held = queued_item("frontend");
        held.hold();
        let ledger = WorkItemStore {
            items: vec![queued_item("frontend"), held],
            ..WorkItemStore::default()
        };
        let active = store.ledger_active_members(&ledger);

        assert!(active.is_empty(), "a terminal member's waiting work never starts: {active:?}");
        assert_eq!(occupied(&store, &active), ["backend"]);
        store.enforce_member_eligibility("shared", "docs", &active).unwrap();
    }

    #[test]
    fn a_running_cycle_of_a_cancelled_member_keeps_its_slot_until_it_settles() {
        let mut store = store();
        dispatch_unsettled_cycle(&mut store, "backend");
        dispatch_unsettled_cycle(&mut store, "frontend");
        store.find_mut("frontend").unwrap().status = CampaignStatus::Cancelled;
        let mut ledger = WorkItemStore {
            items: vec![running_item("frontend")],
            ..WorkItemStore::default()
        };
        let active = store.ledger_active_members(&ledger);

        assert_eq!(active, ["frontend"]);
        assert_eq!(occupied(&store, &active), ["backend", "frontend"]);
        assert!(store.enforce_member_eligibility("shared", "docs", &active).is_err());

        ledger.items[0].execution_completed = true;
        ledger.items[0].settle_landed("workflow completed", Utc::now());
        let active = store.ledger_active_members(&ledger);
        assert_eq!(occupied(&store, &active), ["backend"]);
        store.enforce_member_eligibility("shared", "docs", &active).unwrap();
    }

    #[test]
    fn a_live_members_waiting_item_still_counts() {
        let store = store();
        let ledger = WorkItemStore {
            items: vec![queued_item("backend")],
            ..WorkItemStore::default()
        };
        assert_eq!(store.ledger_active_members(&ledger), ["backend"]);
    }

    #[test]
    fn live_members_still_occupy_their_slots() {
        let mut store = store();
        dispatch_unsettled_cycle(&mut store, "frontend");
        store
            .find_initiative_mut("shared")
            .unwrap()
            .reservations
            .push(MemberReservation {
                member: "backend".into(),
                operation_id: "wi_formation".into(),
                kind: ReservationKind::Formation,
            });

        assert_eq!(occupied(&store, &[]), ["backend", "frontend"]);
        let refused = store.enforce_member_eligibility("shared", "docs", &[]).unwrap_err();
        assert_eq!(refused.to_string(), "initiative 'shared' has reached its active member limit");
        // A member that already holds a slot is not refused by the limit.
        store.enforce_member_eligibility("shared", "frontend", &[]).unwrap();
    }

    #[test]
    fn a_settled_last_objective_frees_the_slot_of_a_live_member() {
        let mut store = store();
        dispatch_unsettled_cycle(&mut store, "frontend");
        store.find_mut("frontend").unwrap().objective_history = vec![CampaignCycle {
            cycle: 1,
            objective: "Deliver the next slice".into(),
            outcome: Some(crate::campaign::CycleOutcome {
                verdict: crate::payload::TaskVerdict::Complete,
                landed: true,
                land_blocked: None,
            }),
        }];

        assert!(occupied(&store, &[]).is_empty());
    }

    #[test]
    fn ledger_work_counts_for_its_member_campaign_or_task_slot_until_it_settles() {
        let mut store = store();
        let campaign_item = running_item("backend");
        let mut task_item = running_item("unrelated");
        task_item.source = None;
        task_item.kind = WorkItemKind::Task;
        store.find_initiative_mut("shared").unwrap().task_dispatches.push(TaskDispatch {
            member: "task_probe".into(),
            work_item: task_item.id.clone(),
            project: "sample".into(),
        });
        let mut settled = running_item("docs");
        settled.execution_completed = true;
        settled.settle_landed("workflow completed", Utc::now());
        let ledger = WorkItemStore {
            items: vec![campaign_item, task_item, settled],
            ..WorkItemStore::default()
        };

        assert_eq!(store.ledger_active_members(&ledger), ["backend", "task_probe"]);
    }
}
