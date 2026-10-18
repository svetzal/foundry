//! Operational recovery evidence, rendered separately from owner decisions.
use crate::proto;
use std::fmt::Write;

pub fn to_proto(state: &foundry_sdk::recovery::RecoveryState) -> proto::RecoveryState {
    proto::RecoveryState {
        id: state.id.clone(),
        kind: state.kind.to_string(),
        detail: state.detail.clone(),
        recorded_at: state.recorded_at.to_rfc3339(),
        resolution: state.resolution.as_ref().map(|resolution| proto::RecoveryResolution {
            evidence: resolution.evidence.clone(),
            resolved_at: resolution.resolved_at.to_rfc3339(),
        }),
    }
}

pub fn detail(scope: &str, name: &str, state: &proto::RecoveryState) -> String {
    let mut out = format!(
        "Recovery: {} ({}, {})\n  {}\n",
        state.id,
        state.kind,
        if state.resolution.is_some() {
            "recheck requested"
        } else {
            "waiting"
        },
        state.detail
    );
    if let Some(resolution) = &state.resolution {
        let _ = writeln!(
            out,
            "  Evidence: {} ({})",
            resolution.evidence.join("; "),
            resolution.resolved_at
        );
    } else {
        let _ = writeln!(
            out,
            "  Recheck: foundry {scope} recover {name} --recovery {} --evidence '<what changed>'",
            state.id
        );
    }
    out
}

pub fn rechecked(response: &proto::RecoverResponse) -> String {
    if response.event_id.is_empty() {
        format!(
            "Recorded recovery evidence. Reassessment waits: {}\nNo owner decision was recorded.\n",
            response.waiting_reason
        )
    } else {
        format!(
            "Recorded recovery evidence and requested reassessment ({}).\nNo owner decision was recorded; normal gates and owner controls still apply.\n",
            response.event_id
        )
    }
}
