//! Readiness diagnostics are observations, not permission to dispatch.
use std::fmt::Write;

use crate::proto;

pub fn detail(readiness: Option<&proto::InitiativeReadiness>) -> String {
    let Some(readiness) = readiness else {
        return "\nReadiness: not reported by this daemon.\n".into();
    };
    let scope = if readiness.live {
        "live store and work ledger"
    } else {
        "store only; live work and registry not checked"
    };
    let mut out = format!("\nReadiness ({scope}):\n");
    for member in &readiness.members {
        let _ = writeln!(
            out,
            "  {}: {}",
            member.member,
            if member.ready {
                "ready for next assessment"
            } else if member.blockers.iter().any(|blocker| matches!(
                blocker.kind.as_str(),
                "campaign_terminal" | "initiative_terminal"
            )) {
                "not eligible"
            } else {
                "waiting"
            }
        );
        for blocker in &member.blockers {
            let _ = writeln!(out, "    {}", blocker.detail);
        }
        for dependency in &member.dependencies {
            let _ = writeln!(
                out,
                "    {}: {}",
                dependency.prerequisite,
                if dependency.satisfied {
                    "satisfied"
                } else {
                    "waiting for evidence"
                }
            );
            if !dependency.evidence.is_empty() {
                let _ = writeln!(out, "      Evidence: {}", dependency.evidence.join("; "));
            }
        }
    }
    let _ = writeln!(
        out,
        "  New task slot: {}",
        if readiness.dispatch_open {
            "available"
        } else {
            "waiting"
        }
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_shows_the_exact_dependency_and_evidence_without_an_answer_command() {
        let value = proto::InitiativeReadiness {
            live: true,
            members: vec![proto::MemberReadiness {
                member: "consumer".into(),
                ready: false,
                blockers: vec![proto::ReadinessBlocker {
                    kind: "dependency_unmet".into(),
                    detail: "waits on provider completion".into(),
                }],
                dependencies: vec![proto::DependencyReadiness {
                    prerequisite: "provider".into(),
                    member: "provider".into(),
                    milestone: "completed".into(),
                    satisfied: false,
                    evidence: vec!["cycle 1 landed, mission still active".into()],
                }],
            }],
            ..Default::default()
        };
        let out = detail(Some(&value));
        assert!(out.contains("consumer: waiting"));
        assert!(out.contains("waits on provider completion"));
        assert!(out.contains("cycle 1 landed, mission still active"));
        assert!(!out.contains("decide"));
        assert!(detail(None).contains("not reported"));
        let offline = proto::InitiativeReadiness {
            live: false,
            ..value
        };
        assert!(detail(Some(&offline)).contains("live work and registry not checked"));
    }
}
