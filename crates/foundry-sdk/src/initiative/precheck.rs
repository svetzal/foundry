//! What a steering pass could do, decided in code before an agent is asked.
//!
//! An automatic steer runs after every settled fact. Most of the time a member
//! slot is free or new evidence has arrived, and the agent's judgement is
//! needed. When every member is blocked, every slot is in use and nothing has
//! arrived since the last recorded decision, no steering action could take
//! effect, and the agent session would only spend tokens choosing to wait.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{CampaignStore, MemberKind, Result};
use crate::campaign::CampaignStatus;

/// A member that cannot advance now, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockedMember {
    pub member: String,
    pub reason: String,
}

/// The steering pass's options, computed from the store and live work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SteeringPrecheck {
    /// Campaign and candidate members that could advance now.
    pub eligible: Vec<String>,
    /// Every other campaign and candidate member, with the reason it cannot.
    pub blocked: Vec<BlockedMember>,
    /// Members holding one of the initiative's active-member slots.
    pub occupied: Vec<String>,
    pub max_active_members: usize,
    /// Whether a new initiative task could start: a free slot, shared cycles
    /// left and a registered project with no execution owner.
    pub dispatch_open: bool,
    /// Facts recorded since the last steering decision that only steering acts
    /// on: a finding that could reach more members, or an owner decision.
    pub unjudged: Vec<String>,
    /// Set while a completion proposal waits for the owner.
    pub awaiting_completion: bool,
}

impl SteeringPrecheck {
    /// Why no steering action could take effect, or `None` when the agent
    /// should assess.
    ///
    /// The agent is needed when a member could advance, a task could start,
    /// no member holds a slot (the initiative is idle, so outcome, staging or
    /// an owner question may be due), or unjudged evidence arrived. Running
    /// work always settles into a fact that requests the next steer.
    #[must_use]
    pub fn skip_reason(&self) -> Option<String> {
        if self.awaiting_completion {
            return Some("a completion proposal waits for owner confirmation".into());
        }
        if !self.eligible.is_empty()
            || self.dispatch_open
            || self.occupied.is_empty()
            || !self.unjudged.is_empty()
        {
            return None;
        }
        let blocked = self
            .blocked
            .iter()
            .map(|blocked| format!("{} ({})", blocked.member, blocked.reason))
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "no member can advance: {blocked}. {} of {} active member slots are in use by {}, \
             and no task can start. No finding to relay or owner decision arrived since the last \
             steering decision",
            self.occupied.len(),
            self.max_active_members,
            self.occupied.join(", ")
        ))
    }
}

impl CampaignStore {
    /// Compute what a steering pass for initiative `name` could do.
    ///
    /// `active_members` is live ledger work ([`Self::ledger_active_members`]).
    /// `busy_projects` names initiative projects whose repository has an
    /// execution or cleanup owner, excluding the steer itself.
    pub fn steering_precheck(
        &self,
        name: &str,
        active_members: &[String],
        busy_projects: &[String],
    ) -> Result<SteeringPrecheck> {
        let initiative = self.initiative_view(name)?;
        let occupied = self.occupied_member_slots(name, active_members)?;
        let used = self.shared_cycles_used(name)?;
        let budget_left = used < initiative.budget.max_cycles;
        let free_project = |project: &str| !busy_projects.iter().any(|busy| busy == project);
        let mut eligible = Vec::new();
        let mut blocked = Vec::new();
        for member in &initiative.members {
            if member.kind == MemberKind::Task {
                continue;
            }
            let campaign = self.find(&member.name);
            let reason = if let Some(status) = campaign.map(|campaign| campaign.status)
                && matches!(
                    status,
                    CampaignStatus::Completed | CampaignStatus::Cancelled | CampaignStatus::Paused
                ) {
                Some(format!("campaign is {status}"))
            } else if occupied.contains(&member.name) {
                Some("has work in flight".into())
            } else if let Err(refused) =
                self.enforce_member_eligibility(name, &member.name, active_members)
            {
                Some(refused.to_string())
            } else if !budget_left {
                Some(format!("initiative '{name}' exhausted its cycle budget"))
            } else if campaign.map_or_else(
                || !initiative.projects.iter().any(|project| free_project(project)),
                |campaign| !free_project(&campaign.project),
            ) {
                Some("its repository has an execution or cleanup owner".into())
            } else {
                None
            };
            match reason {
                Some(reason) => blocked.push(BlockedMember {
                    member: member.name.clone(),
                    reason,
                }),
                None => eligible.push(member.name.clone()),
            }
        }
        let dispatch_open = occupied.len() < initiative.budget.max_active_members
            && budget_left
            && initiative.projects.iter().any(|project| free_project(project));
        Ok(SteeringPrecheck {
            eligible,
            blocked,
            occupied: occupied.into_iter().collect(),
            max_active_members: initiative.budget.max_active_members,
            dispatch_open,
            unjudged: self.unjudged_steering_evidence(name)?,
            awaiting_completion: initiative.completion_proposal.is_some(),
        })
    }

    /// Facts recorded after the last steering decision that steering alone
    /// would act on. Before any decision, everything is unjudged.
    fn unjudged_steering_evidence(&self, name: &str) -> Result<Vec<String>> {
        let initiative = self.require_initiative(name)?;
        let Some(since) = initiative.steering_history.last().map(|record| record.recorded_at)
        else {
            return Ok(vec!["no steering decision has been recorded yet".into()]);
        };
        let recipients: BTreeSet<&str> = initiative
            .members
            .iter()
            .filter(|member| {
                self.find(&member.name).is_none_or(|campaign| !campaign.status.is_terminal())
            })
            .map(|member| member.name.as_str())
            .collect();
        let newer = |at: DateTime<Utc>| at > since;
        let mut unjudged: Vec<String> = self
            .findings
            .iter()
            .filter(|finding| {
                finding.initiative == name
                    && newer(finding.recorded_at)
                    && !finding.to.is_empty()
                    && recipients.iter().any(|member| {
                        !finding.to.iter().any(|to| to == member)
                            && !finding.consumed_by.iter().any(|seen| seen.member == *member)
                    })
            })
            .map(|finding| format!("finding '{}' could reach more members", finding.id))
            .collect();
        unjudged.extend(
            initiative
                .owner_decisions
                .iter()
                .filter(|decision| newer(decision.decided_at))
                .map(|decision| format!("owner decision: {}", decision.decision)),
        );
        Ok(unjudged)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::campaign::CampaignCycle;
    use crate::gateway::AgentProvider;
    use crate::initiative::{
        Finding, FindingKind, FindingSource, SteeringDecision, SteeringRecord,
    };

    /// Two campaign members over one project, steered once an hour ago.
    fn store() -> CampaignStore {
        let mut store = CampaignStore::default();
        for name in ["frontend", "backend"] {
            let campaign = serde_json::from_value(serde_json::json!({
                "name": name, "project": "sample", "mission": "Ship the behavior",
                "authorized_by": "Stacey",
                "done_evidence": [{"kind": "review", "statement": "It works"}]
            }))
            .unwrap();
            store.add(campaign).unwrap();
        }
        let initiative = serde_json::from_value(serde_json::json!({
            "name": "shared", "outcome": "Both work", "projects": ["sample"],
            "authorized_by": "Stacey",
            "members": [
                {"kind": "campaign", "name": "frontend"},
                {"kind": "campaign", "name": "backend"}
            ],
            "done_evidence": [{"kind": "review", "statement": "Both work"}]
        }))
        .unwrap();
        store.add_initiative(initiative, &["sample".into()]).unwrap();
        store
            .find_initiative_mut("shared")
            .unwrap()
            .steering_history
            .push(SteeringRecord {
                id: "earlier".into(),
                prompt: "Earlier assessment".into(),
                decision: SteeringDecision::Wait {
                    reason: "workers run".into(),
                },
                provider: Some(AgentProvider::Codex),
                recorded_at: Utc::now() - Duration::hours(1),
                follow_up_of: None,
                integration_skipped: None,
            });
        store
    }

    fn running(store: &mut CampaignStore, member: &str) {
        let campaign = store.find_mut(member).unwrap();
        campaign.status = CampaignStatus::Active;
        campaign.cycles_completed += 1;
        campaign.record_dispatched_objective("Deliver the next slice".into());
    }

    fn precheck(store: &CampaignStore) -> SteeringPrecheck {
        store.steering_precheck("shared", &[], &[]).unwrap()
    }

    #[test]
    fn a_free_member_is_eligible_and_the_agent_assesses() {
        let mut store = store();
        running(&mut store, "frontend");

        let checked = precheck(&store);

        assert_eq!(checked.eligible, ["backend"]);
        assert_eq!(checked.occupied, ["frontend"]);
        assert!(checked.dispatch_open);
        assert_eq!(checked.skip_reason(), None);
    }

    #[test]
    fn full_slots_and_no_new_evidence_leave_nothing_to_do() {
        let mut store = store();
        running(&mut store, "frontend");
        running(&mut store, "backend");

        let checked = precheck(&store);

        assert!(checked.eligible.is_empty(), "{checked:?}");
        assert!(!checked.dispatch_open);
        let reason = checked.skip_reason().unwrap();
        assert!(reason.contains("frontend (has work in flight)"), "{reason}");
        assert!(reason.contains("2 of 2 active member slots"), "{reason}");
    }

    #[test]
    fn an_owner_held_member_and_a_running_member_leave_only_dispatch() {
        let mut store = store();
        running(&mut store, "frontend");
        store.hold_member("shared", "backend", "Review the contract first").unwrap();

        let checked = precheck(&store);

        assert!(checked.eligible.is_empty(), "{checked:?}");
        assert_eq!(
            checked.blocked[1],
            BlockedMember {
                member: "backend".into(),
                reason: "member 'backend' is held: Review the contract first".into()
            }
        );
        assert!(checked.dispatch_open, "one slot is free for a task");
        assert_eq!(checked.skip_reason(), None);

        store.find_initiative_mut("shared").unwrap().budget.max_active_members = 1;
        assert!(precheck(&store).skip_reason().is_some());
    }

    #[test]
    fn a_cancelled_member_is_blocked_without_holding_a_slot() {
        let mut store = store();
        running(&mut store, "frontend");
        store.find_mut("frontend").unwrap().status = CampaignStatus::Cancelled;

        let checked = precheck(&store);

        assert_eq!(checked.eligible, ["backend"]);
        assert_eq!(checked.occupied, Vec::<String>::new());
        assert_eq!(checked.blocked[0].reason, "campaign is cancelled");
    }

    #[test]
    fn a_busy_repository_or_a_spent_budget_blocks_advance() {
        let mut store = store();
        let busy = precheck_with_busy(&store);
        assert_eq!(busy.eligible, Vec::<String>::new());
        assert!(!busy.dispatch_open);
        assert_eq!(busy.blocked[0].reason, "its repository has an execution or cleanup owner");
        assert_eq!(busy.skip_reason(), None, "an idle initiative is always assessed");

        store.find_initiative_mut("shared").unwrap().budget.max_cycles = 1;
        store.find_mut("frontend").unwrap().cycles_completed = 1;
        store.find_mut("frontend").unwrap().objective_history = vec![CampaignCycle {
            cycle: 1,
            objective: "Done".into(),
            outcome: None,
        }];
        let spent = precheck(&store);
        assert_eq!(spent.blocked[1].reason, "initiative 'shared' exhausted its cycle budget");
        assert!(!spent.dispatch_open);
    }

    fn precheck_with_busy(store: &CampaignStore) -> SteeringPrecheck {
        store.steering_precheck("shared", &[], &["sample".into()]).unwrap()
    }

    #[test]
    fn a_new_relayable_finding_or_owner_decision_needs_judgement() {
        let mut store = store();
        running(&mut store, "frontend");
        running(&mut store, "backend");
        let source = FindingSource {
            kind: "owner".into(),
            id: "note".into(),
        };
        let mut old = Finding::new(
            "shared",
            source.clone(),
            vec!["frontend".into()],
            FindingKind::Evidence,
            "Seen already",
            vec![],
        );
        old.recorded_at = Utc::now() - Duration::hours(2);
        store.add_finding(old).unwrap();
        store
            .add_finding(Finding::new(
                "shared",
                source.clone(),
                vec![],
                FindingKind::Regression,
                "Reaches everyone already",
                vec![],
            ))
            .unwrap();
        assert!(precheck(&store).skip_reason().is_some(), "{:?}", precheck(&store));

        let targeted = store
            .add_finding(Finding::new(
                "shared",
                source,
                vec!["frontend".into()],
                FindingKind::ContractChanged,
                "Port moved",
                vec![],
            ))
            .unwrap();
        assert_eq!(
            precheck(&store).unjudged,
            [format!(
                "finding '{}' could reach more members",
                targeted.id
            )]
        );
        assert_eq!(precheck(&store).skip_reason(), None);
    }

    #[test]
    fn a_new_owner_decision_or_an_unsteered_initiative_needs_judgement() {
        let mut store = store();
        running(&mut store, "frontend");
        running(&mut store, "backend");
        store.decide_initiative("shared", "Ship v2 first", &[], Utc::now()).unwrap();
        assert_eq!(precheck(&store).unjudged, ["owner decision: Ship v2 first"]);

        store.find_initiative_mut("shared").unwrap().steering_history.clear();
        assert!(precheck(&store).unjudged[0].contains("no steering decision"));
    }

    #[test]
    fn a_waiting_completion_proposal_is_never_reassessed() {
        let mut checked = precheck(&store());
        assert_eq!(checked.eligible, ["frontend", "backend"]);
        checked.awaiting_completion = true;
        assert_eq!(
            checked.skip_reason().as_deref(),
            Some("a completion proposal waits for owner confirmation")
        );
    }
}
