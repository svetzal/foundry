//! Read-only readiness snapshots. Admission always rechecks current controls.
use foundry_sdk::{
    campaign::CampaignStore, initiative::SteeringPrecheck, registry::Registry,
    work_item::WorkItemStore,
};
use tonic::Status;

use crate::proto;

fn to_proto(check: SteeringPrecheck) -> proto::InitiativeReadiness {
    proto::InitiativeReadiness {
        members: check
            .readiness
            .into_iter()
            .map(|member| proto::MemberReadiness {
                member: member.member,
                ready: member.ready,
                blockers: member
                    .blockers
                    .into_iter()
                    .map(|blocker| proto::ReadinessBlocker {
                        kind: blocker.kind.as_str().into(),
                        detail: blocker.detail,
                    })
                    .collect(),
                dependencies: member
                    .dependencies
                    .into_iter()
                    .map(|dependency| proto::DependencyReadiness {
                        prerequisite: dependency.prerequisite,
                        member: dependency.member,
                        milestone: dependency.milestone,
                        satisfied: dependency.satisfied,
                        evidence: dependency.evidence,
                    })
                    .collect(),
            })
            .collect(),
        dispatch_open: check.dispatch_open,
        occupied: check.occupied,
        max_active_members: u64::try_from(check.max_active_members).unwrap_or(u64::MAX),
        observed_at: chrono::Utc::now().to_rfc3339(),
        live: true,
    }
}

pub(super) fn attach(
    response: &mut proto::Initiative,
    store: &CampaignStore,
    ledger: &WorkItemStore,
    registry: &Registry,
) -> Result<(), Status> {
    let check = foundry_blocks::blocks::steering_readiness(store, &response.name, ledger, registry)
        .map_err(|error| Status::internal(error.to_string()))?;
    response.readiness = Some(to_proto(check));
    Ok(())
}
