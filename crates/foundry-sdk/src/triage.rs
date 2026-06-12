//! Triage domain types for the post-maintenance failure triage formation.
//!
//! These types are part of the SDK contract so they can be shared between
//! the core classification logic in `foundry-blocks` and any downstream
//! consumer that reads `MaintenanceTriageCompletedPayload`.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Classification of a gate failure into one of 12 domain classes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Agent runner crashed, silent no-op, or timed out.
    AgentRunnerFault,
    /// CI/infra flake: filesystem error, network hiccup, OOM, ephemeral tool failure.
    CiInfraFlake,
    /// Formatting or lint drift — mechanically fixable.
    FormatAndLintDrift,
    /// Security advisory with a known fix version available.
    VulnWithFix,
    /// Routine dependency bump (patch/minor, no constraints broken).
    RoutineDependencyBump,
    /// Security advisory with no available fix.
    VulnNoFix,
    /// Major version bump or constraint relaxation required.
    DependencyMajorBumpOrConstraintRelax,
    /// Compile error or static-analysis failure requiring human attention.
    CompileAndStaticAnalysisCodeError,
    /// Test suite failure.
    TestBreakage,
    /// Gate toolchain or configuration problem (tool not found, misconfig).
    GateInfraMisconfig,
    /// N≥3 consecutive failures on the same gate — persistent deadlock.
    ChronicDeadlock,
    /// Triage rejected the outcome as noise (not a real failure).
    TriageRejectedNoise,
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::AgentRunnerFault => "agent_runner_fault",
            Self::CiInfraFlake => "ci_infra_flake",
            Self::FormatAndLintDrift => "format_and_lint_drift",
            Self::VulnWithFix => "vuln_with_fix",
            Self::RoutineDependencyBump => "routine_dependency_bump",
            Self::VulnNoFix => "vuln_no_fix",
            Self::DependencyMajorBumpOrConstraintRelax => {
                "dependency_major_bump_or_constraint_relax"
            }
            Self::CompileAndStaticAnalysisCodeError => "compile_and_static_analysis_code_error",
            Self::TestBreakage => "test_breakage",
            Self::GateInfraMisconfig => "gate_infra_misconfig",
            Self::ChronicDeadlock => "chronic_deadlock",
            Self::TriageRejectedNoise => "triage_rejected_noise",
        };
        f.write_str(s)
    }
}

/// The recommended action for a classified failure or infra incident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Correlated infra noise — suppress, no action needed.
    SuppressInfra,
    /// Mechanically fixable — a `proposed_command` is available.
    AutoFixable,
    /// Requires a human policy call (e.g., accept/reject a security advisory).
    PolicyCall,
    /// Warrants investigation before deciding on a fix.
    NeedsInvestigation,
    /// Escalate immediately — chronic deadlock or persistent blocker.
    Escalate,
    /// Reclassified as benign; not a real failure.
    ReclassifyBenign,
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::SuppressInfra => "suppress_infra",
            Self::AutoFixable => "auto_fixable",
            Self::PolicyCall => "policy_call",
            Self::NeedsInvestigation => "needs_investigation",
            Self::Escalate => "escalate",
            Self::ReclassifyBenign => "reclassify_benign",
        };
        f.write_str(s)
    }
}

/// A classified verdict for a single gate failure in a single project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureVerdict {
    pub project: String,
    pub gate: String,
    pub class: FailureClass,
    pub decision: Decision,
    pub evidence: String,
    /// The shell command proposed to fix this failure, when `decision` is `AutoFixable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_command: Option<String>,
}

/// A correlated infra incident — N≥3 projects shared the same failure signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfraIncident {
    /// Normalised signature string used for correlation.
    pub signature: String,
    /// Always `SuppressInfra` for correlated incidents.
    pub decision: Decision,
    /// The projects whose failures were collapsed into this incident.
    pub projects: Vec<String>,
    /// A representative evidence string from one of the collapsed failures.
    pub sample_evidence: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_class_display_round_trips_via_serde() {
        let cases = [
            (FailureClass::AgentRunnerFault, "agent_runner_fault"),
            (FailureClass::CiInfraFlake, "ci_infra_flake"),
            (FailureClass::FormatAndLintDrift, "format_and_lint_drift"),
            (FailureClass::VulnWithFix, "vuln_with_fix"),
            (FailureClass::RoutineDependencyBump, "routine_dependency_bump"),
            (FailureClass::VulnNoFix, "vuln_no_fix"),
            (
                FailureClass::DependencyMajorBumpOrConstraintRelax,
                "dependency_major_bump_or_constraint_relax",
            ),
            (
                FailureClass::CompileAndStaticAnalysisCodeError,
                "compile_and_static_analysis_code_error",
            ),
            (FailureClass::TestBreakage, "test_breakage"),
            (FailureClass::GateInfraMisconfig, "gate_infra_misconfig"),
            (FailureClass::ChronicDeadlock, "chronic_deadlock"),
            (FailureClass::TriageRejectedNoise, "triage_rejected_noise"),
        ];

        for (class, expected) in &cases {
            assert_eq!(class.to_string(), *expected, "Display for {class:?} should be {expected}");
            let json = serde_json::to_string(class).unwrap();
            assert_eq!(json, format!("\"{expected}\""), "serialization for {class:?}");
            let back: FailureClass = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, class, "round-trip for {class:?}");
        }
    }

    #[test]
    fn decision_display_round_trips_via_serde() {
        let cases = [
            (Decision::SuppressInfra, "suppress_infra"),
            (Decision::AutoFixable, "auto_fixable"),
            (Decision::PolicyCall, "policy_call"),
            (Decision::NeedsInvestigation, "needs_investigation"),
            (Decision::Escalate, "escalate"),
            (Decision::ReclassifyBenign, "reclassify_benign"),
        ];

        for (decision, expected) in &cases {
            assert_eq!(
                decision.to_string(),
                *expected,
                "Display for {decision:?} should be {expected}"
            );
            let json = serde_json::to_string(decision).unwrap();
            assert_eq!(json, format!("\"{expected}\""), "serialization for {decision:?}");
            let back: Decision = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, decision, "round-trip for {decision:?}");
        }
    }

    #[test]
    fn failure_verdict_round_trips() {
        let v = FailureVerdict {
            project: "my-project".to_string(),
            gate: "fmt".to_string(),
            class: FailureClass::FormatAndLintDrift,
            decision: Decision::AutoFixable,
            evidence: "cargo fmt produced diffs".to_string(),
            proposed_command: Some("cargo fmt".to_string()),
        };
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["project"], "my-project");
        assert_eq!(json["gate"], "fmt");
        assert_eq!(json["class"], "format_and_lint_drift");
        assert_eq!(json["decision"], "auto_fixable");
        assert_eq!(json["proposed_command"], "cargo fmt");

        let back: FailureVerdict = serde_json::from_value(json).unwrap();
        assert_eq!(back.project, "my-project");
        assert_eq!(back.class, FailureClass::FormatAndLintDrift);
        assert_eq!(back.decision, Decision::AutoFixable);
        assert_eq!(back.proposed_command.as_deref(), Some("cargo fmt"));
    }

    #[test]
    fn failure_verdict_omits_proposed_command_when_none() {
        let v = FailureVerdict {
            project: "proj".to_string(),
            gate: "test".to_string(),
            class: FailureClass::TestBreakage,
            decision: Decision::NeedsInvestigation,
            evidence: "tests failed".to_string(),
            proposed_command: None,
        };
        let json = serde_json::to_value(&v).unwrap();
        assert!(
            json.get("proposed_command").is_none(),
            "proposed_command must be absent when None"
        );
    }

    #[test]
    fn infra_incident_round_trips() {
        let incident = InfraIncident {
            signature: "os_error_2_never_executed".to_string(),
            decision: Decision::SuppressInfra,
            projects: vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
            sample_evidence: "os error 2: No such file or directory (never-executed)".to_string(),
        };
        let json = serde_json::to_value(&incident).unwrap();
        assert_eq!(json["signature"], "os_error_2_never_executed");
        assert_eq!(json["decision"], "suppress_infra");
        assert_eq!(json["projects"].as_array().unwrap().len(), 3);

        let back: InfraIncident = serde_json::from_value(json).unwrap();
        assert_eq!(back.projects.len(), 3);
        assert_eq!(back.decision, Decision::SuppressInfra);
    }
}
