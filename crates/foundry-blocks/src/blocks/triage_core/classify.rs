use foundry_sdk::triage::{Decision, FailureClass};

use super::RawFailure;

/// Compute a normalised infra signature for correlation.
///
/// Strips project-specific tokens (paths, timestamps, line numbers) to
/// produce a canonical string suitable for cross-project grouping.
pub fn signature(failure: &RawFailure) -> String {
    let output = &failure.gate_output;

    // Match well-known infra patterns in priority order.
    if output.contains("os error 2") || output.contains("never-executed") {
        return "os_error_2_never_executed".to_string();
    }
    if output.contains("No such file") {
        return "no_such_file".to_string();
    }
    if output.contains("rmeta") {
        return "rmeta_artifact".to_string();
    }
    if output.contains("NameResolution") {
        return "name_resolution_failure".to_string();
    }
    if output.contains("ConnectionRefused") {
        return "connection_refused".to_string();
    }
    if output.contains("timed out after 300s") {
        return "timeout_300s".to_string();
    }
    if output.contains("OOM") {
        return "out_of_memory".to_string();
    }
    if output.contains("timed out after") && output.contains("claude") {
        return "claude_timeout".to_string();
    }
    if output.contains("agent failed") {
        return "agent_failed".to_string();
    }
    if output.contains("silent no-op") {
        return "agent_silent_noop".to_string();
    }

    // Fallback: use gate name + first 40 chars of output
    let truncated: String = output.chars().take(40).collect();
    format!("{}:{}", failure.gate_name, truncated)
}

/// Return `true` if the summary string represents a benign triage-rejection
/// — i.e., the agent concluded no real failure exists.
pub fn is_benign_decline(summary: &str) -> bool {
    let lower = summary.to_lowercase();
    lower.contains("no correction warranted")
        || lower.contains("triage rejected")
        || lower.contains("unknown violation")
        || lower.contains("no changes needed")
}

/// Classify a failure into one of the 12 `FailureClass` variants.
pub fn classify(failure: &RawFailure) -> FailureClass {
    let gate = failure.gate_name.to_lowercase();
    let output = &failure.gate_output;
    let output_lower = output.to_lowercase();

    // TriageRejectedNoise — check early so benign noise doesn't get misclassified
    if is_benign_decline(output) {
        return FailureClass::TriageRejectedNoise;
    }

    // AgentRunnerFault
    if gate.contains("agent_execution")
        || output_lower.contains("silent no-op")
        || output_lower.contains("agent failed")
        || (output_lower.contains("timed out after") && output_lower.contains("claude"))
    {
        return FailureClass::AgentRunnerFault;
    }

    // CiInfraFlake
    if output_lower.contains("os error 2")
        || output_lower.contains("never-executed")
        || (output_lower.contains("tmp") && output_lower.contains("no such file"))
        || output_lower.contains("rmeta")
        || output_lower.contains("nameresolution")
        || output_lower.contains("connectionrefused")
        || output_lower.contains("timed out after 300s")
        || output_lower.contains("oom")
        || gate.contains("network")
    {
        return FailureClass::CiInfraFlake;
    }

    // FormatAndLintDrift
    if gate.contains("fmt")
        || gate.contains("format")
        || gate.contains("clippy")
        || gate.contains("lint")
        || output_lower.contains("machine-applicable")
    {
        return FailureClass::FormatAndLintDrift;
    }

    // Vulnerability classes — check for fix version
    let is_security_gate =
        gate.contains("security") || gate.contains("audit") || gate.contains("vuln");
    if is_security_gate {
        let has_fix_version = output.contains('\u{2192}') // →
            || output_lower.contains("fixed")
            || {
                // "upgrade to X.Y.Z" pattern — look for a digit after "upgrade to"
                if let Some(pos) = output_lower.find("upgrade to") {
                    let after = &output_lower[pos + 10..];
                    after.chars().any(|c| c.is_ascii_digit())
                } else {
                    false
                }
            };
        return if has_fix_version {
            FailureClass::VulnWithFix
        } else {
            FailureClass::VulnNoFix
        };
    }

    // RoutineDependencyBump
    if gate.contains("dep")
        || (output_lower.contains("dependency")
            && (output_lower.contains("update")
                || output_lower.contains("bump")
                || output_lower.contains("upgrade")))
    {
        return FailureClass::RoutineDependencyBump;
    }

    // DependencyMajorBumpOrConstraintRelax
    if (output_lower.contains("major")
        || output_lower.contains("constraint")
        || output_lower.contains("pin"))
        && output.chars().any(|c| c.is_ascii_digit())
    {
        return FailureClass::DependencyMajorBumpOrConstraintRelax;
    }

    // CompileAndStaticAnalysisCodeError — check before TestBreakage (error[ wins)
    if gate.contains("compile")
        || gate.contains("dialyzer")
        || gate.contains("cppcheck")
        || gate.contains("typecheck")
        || output.contains("error[")
    {
        return FailureClass::CompileAndStaticAnalysisCodeError;
    }

    // TestBreakage
    if gate.contains("test") {
        return FailureClass::TestBreakage;
    }

    // GateInfraMisconfig
    if gate.contains("gate")
        && (output_lower.contains("not found")
            || output_lower.contains("toolchain")
            || output_lower.contains("misconfig"))
    {
        return FailureClass::GateInfraMisconfig;
    }

    // Fallback
    FailureClass::CompileAndStaticAnalysisCodeError
}

/// Map a `FailureClass` to its base `Decision`.
pub fn decision_for(class: &FailureClass) -> Decision {
    match class {
        FailureClass::AgentRunnerFault | FailureClass::CiInfraFlake => Decision::SuppressInfra,
        FailureClass::FormatAndLintDrift
        | FailureClass::VulnWithFix
        | FailureClass::RoutineDependencyBump => Decision::AutoFixable,
        FailureClass::VulnNoFix
        | FailureClass::DependencyMajorBumpOrConstraintRelax
        | FailureClass::GateInfraMisconfig => Decision::PolicyCall,
        FailureClass::CompileAndStaticAnalysisCodeError | FailureClass::TestBreakage => {
            Decision::NeedsInvestigation
        }
        FailureClass::ChronicDeadlock => Decision::Escalate,
        FailureClass::TriageRejectedNoise => Decision::ReclassifyBenign,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use foundry_sdk::triage::{Decision, FailureClass};

    use super::*;

    fn failure_for(gate: &str, output: &str) -> RawFailure {
        RawFailure {
            project: "test-project".to_string(),
            gate_name: gate.to_string(),
            gate_output: output.to_string(),
            exit_code: Some(1),
            fix_command: None,
            occurred_at: Utc::now(),
        }
    }

    #[test]
    fn is_benign_decline_recognizes_known_phrases() {
        assert!(is_benign_decline("No correction warranted by the agent."));
        assert!(is_benign_decline("triage rejected: not a violation"));
        assert!(is_benign_decline("Unknown violation — skipping"));
        assert!(is_benign_decline("No changes needed at this time."));
    }

    #[test]
    fn is_benign_decline_case_insensitive() {
        assert!(is_benign_decline("NO CORRECTION WARRANTED"));
        assert!(is_benign_decline("TRIAGE REJECTED"));
    }

    #[test]
    fn is_benign_decline_false_for_real_failures() {
        assert!(!is_benign_decline("cargo fmt failed with exit code 1"));
        assert!(!is_benign_decline("error[E0308]: mismatched types"));
        assert!(!is_benign_decline("test failed: assertion failed at line 42"));
    }

    #[test]
    fn classify_agent_runner_fault_by_gate_name() {
        let f = failure_for("agent_execution", "exited 1");
        assert_eq!(classify(&f), FailureClass::AgentRunnerFault);
    }

    #[test]
    fn classify_agent_runner_fault_by_output_silent_noop() {
        let f = failure_for("maintain", "silent no-op detected");
        assert_eq!(classify(&f), FailureClass::AgentRunnerFault);
    }

    #[test]
    fn classify_agent_runner_fault_by_output_agent_failed() {
        let f = failure_for("maintain", "agent failed with exit code 2");
        assert_eq!(classify(&f), FailureClass::AgentRunnerFault);
    }

    #[test]
    fn classify_ci_infra_flake_os_error_2() {
        let f = failure_for("test", "io error: os error 2: No such file");
        assert_eq!(classify(&f), FailureClass::CiInfraFlake);
    }

    #[test]
    fn classify_ci_infra_flake_never_executed() {
        let f = failure_for("build", "command never-executed: artifact missing");
        assert_eq!(classify(&f), FailureClass::CiInfraFlake);
    }

    #[test]
    fn classify_ci_infra_flake_network_gate() {
        let f = failure_for("network_check", "connection refused");
        assert_eq!(classify(&f), FailureClass::CiInfraFlake);
    }

    #[test]
    fn classify_format_and_lint_drift_fmt_gate() {
        let f = failure_for("fmt", "rustfmt: diff detected");
        assert_eq!(classify(&f), FailureClass::FormatAndLintDrift);
    }

    #[test]
    fn classify_format_and_lint_drift_clippy_gate() {
        let f = failure_for("clippy", "warning: unused variable (machine-applicable)");
        assert_eq!(classify(&f), FailureClass::FormatAndLintDrift);
    }

    #[test]
    fn classify_vuln_with_fix_arrow_notation() {
        let f = failure_for("security", "openssl 1.0 → 1.1 available");
        assert_eq!(classify(&f), FailureClass::VulnWithFix);
    }

    #[test]
    fn classify_vuln_with_fix_upgrade_to() {
        let f = failure_for("audit", "CVE-2024-1234: upgrade to 2.0.1");
        assert_eq!(classify(&f), FailureClass::VulnWithFix);
    }

    #[test]
    fn classify_vuln_no_fix_when_no_version() {
        let f = failure_for("audit", "CVE-2024-9999: no fix available yet");
        assert_eq!(classify(&f), FailureClass::VulnNoFix);
    }

    #[test]
    fn classify_routine_dependency_bump() {
        let f = failure_for("dep_check", "dependency update available: serde 1.0.190 → 1.0.195");
        assert_eq!(classify(&f), FailureClass::RoutineDependencyBump);
    }

    #[test]
    fn classify_test_breakage() {
        let f = failure_for("test", "test suite failed: 3 failures");
        assert_eq!(classify(&f), FailureClass::TestBreakage);
    }

    #[test]
    fn classify_compile_error() {
        let f = failure_for("build", "error[E0308]: mismatched types at src/main.rs:42");
        assert_eq!(classify(&f), FailureClass::CompileAndStaticAnalysisCodeError);
    }

    #[test]
    fn classify_gate_infra_misconfig() {
        let f = failure_for("gate_check", "toolchain not found: stable-aarch64");
        assert_eq!(classify(&f), FailureClass::GateInfraMisconfig);
    }

    #[test]
    fn classify_triage_rejected_noise() {
        let f = failure_for("maintain", "no correction warranted at this time");
        assert_eq!(classify(&f), FailureClass::TriageRejectedNoise);
    }

    #[test]
    fn decision_for_all_classes() {
        use FailureClass as FC;
        let cases = [
            (FC::AgentRunnerFault, Decision::SuppressInfra),
            (FC::CiInfraFlake, Decision::SuppressInfra),
            (FC::FormatAndLintDrift, Decision::AutoFixable),
            (FC::VulnWithFix, Decision::AutoFixable),
            (FC::RoutineDependencyBump, Decision::AutoFixable),
            (FC::VulnNoFix, Decision::PolicyCall),
            (FC::DependencyMajorBumpOrConstraintRelax, Decision::PolicyCall),
            (FC::GateInfraMisconfig, Decision::PolicyCall),
            (FC::CompileAndStaticAnalysisCodeError, Decision::NeedsInvestigation),
            (FC::TestBreakage, Decision::NeedsInvestigation),
            (FC::ChronicDeadlock, Decision::Escalate),
            (FC::TriageRejectedNoise, Decision::ReclassifyBenign),
        ];
        for (class, expected) in &cases {
            assert_eq!(
                decision_for(class),
                *expected,
                "decision_for({class:?}) should be {expected:?}"
            );
        }
    }
}
