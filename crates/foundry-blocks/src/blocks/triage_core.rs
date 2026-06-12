//! Pure functional core for the post-maintenance failure triage formation.
//!
//! This module contains all analysis logic — JSONL reading, raw failure
//! extraction, classification, correlation, and streak detection — with no I/O
//! side effects beyond reading JSONL files. All functions are unit-testable
//! without a real engine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike as _, Duration, Utc};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::PreflightCompletedPayload;
use foundry_sdk::triage::{Decision, FailureClass, FailureVerdict, InfraIncident};

/// A raw gate failure extracted from events before classification.
#[derive(Debug, Clone)]
pub struct RawFailure {
    pub project: String,
    pub gate_name: String,
    pub gate_output: String,
    pub exit_code: Option<i32>,
    pub fix_command: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// JSONL reading
// ---------------------------------------------------------------------------

/// Read Foundry events from JSONL files since a given timestamp.
///
/// Reads files for the current and prior month from `events_dir`
/// (files are named `YYYY-MM.jsonl`). Silently skips malformed lines.
pub fn read_events_jsonl(events_dir: &Path, since: DateTime<Utc>) -> Vec<Event> {
    let mut events = Vec::new();

    let now = Utc::now();
    let months: Vec<String> = {
        let mut v = Vec::new();
        // Prior month
        let prior = if now.month0() == 0 {
            chrono::NaiveDate::from_ymd_opt(now.year() - 1, 12, 1)
        } else {
            chrono::NaiveDate::from_ymd_opt(now.year(), now.month() - 1, 1)
        };
        if let Some(d) = prior {
            v.push(d.format("%Y-%m").to_string());
        }
        // Current month
        v.push(now.format("%Y-%m").to_string());
        v
    };

    for month in &months {
        let path = events_dir.join(format!("{month}.jsonl"));
        read_jsonl_file(&path, since, &mut events);
    }

    events
}

fn read_jsonl_file(path: &Path, since: DateTime<Utc>, out: &mut Vec<Event>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                error = %err,
                "triage-core: cannot read JSONL file (skipping)"
            );
            return;
        }
    };

    for (line_no, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(trimmed) {
            Ok(event) if event.occurred_at > since => {
                out.push(event);
            }
            Ok(_) => {}
            Err(err) => {
                tracing::debug!(
                    path = %path.display(),
                    line = line_no + 1,
                    error = %err,
                    "triage-core: skipping malformed JSONL line"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Run failure extraction
// ---------------------------------------------------------------------------

/// Extract raw failures from a set of events within a time window.
///
/// Reads `PreflightCompleted` events, extracts gate results where `passed =
/// false`, and excludes gates that were self-healed (`fix_applied = true`).
/// Only events with `occurred_at` in `[window_start, window_end]` are
/// considered.
pub fn run_failures(
    events: &[Event],
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Vec<RawFailure> {
    let mut failures = Vec::new();

    for event in events {
        if event.event_type != EventType::PreflightCompleted {
            continue;
        }
        if event.occurred_at < window_start || event.occurred_at > window_end {
            continue;
        }

        let Ok(payload) = event.parse_payload::<PreflightCompletedPayload>() else {
            continue;
        };

        for result in &payload.results {
            if result.passed || result.fix_applied {
                continue;
            }
            failures.push(RawFailure {
                project: payload.project.clone(),
                gate_name: result.name.clone(),
                gate_output: result.output.clone(),
                exit_code: Some(result.exit_code),
                fix_command: None, // gate result does not carry fix_command
                occurred_at: event.occurred_at,
            });
        }
    }

    failures
}

// ---------------------------------------------------------------------------
// Signature normalisation
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Correlation
// ---------------------------------------------------------------------------

/// Group infra-class failures by signature.
///
/// When N≥3 distinct projects share the same signature, collapses them into
/// one `InfraIncident` with `decision: SuppressInfra`. Failures with N<3
/// sharing a signature remain as per-project failures (still classified as
/// infra class, just not correlated away).
///
/// Returns `(incidents, remaining_failures)`.
pub fn correlate(failures: Vec<RawFailure>) -> (Vec<InfraIncident>, Vec<RawFailure>) {
    // Separate infra-class from everything else up front.
    let (infra_candidates, mut remaining): (Vec<RawFailure>, Vec<RawFailure>) =
        failures.into_iter().partition(|f| {
            let class = classify(f);
            matches!(class, FailureClass::AgentRunnerFault | FailureClass::CiInfraFlake)
        });

    // Group infra candidates by signature.
    let mut by_sig: HashMap<String, Vec<RawFailure>> = HashMap::new();
    for failure in infra_candidates {
        let sig = signature(&failure);
        by_sig.entry(sig).or_default().push(failure);
    }

    let mut incidents = Vec::new();

    for (sig, group) in by_sig {
        // Collect distinct projects.
        let mut projects: Vec<String> = group
            .iter()
            .map(|f| f.project.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        projects.sort();

        if projects.len() >= 3 {
            let sample_evidence = group
                .first()
                .map(|f| f.gate_output.chars().take(200).collect::<String>())
                .unwrap_or_default();

            incidents.push(InfraIncident {
                signature: sig,
                decision: Decision::SuppressInfra,
                projects,
                sample_evidence,
            });
        } else {
            // Below the threshold — keep as per-project failures.
            remaining.extend(group);
        }
    }

    (incidents, remaining)
}

// ---------------------------------------------------------------------------
// Streak detection
// ---------------------------------------------------------------------------

/// One gate occurrence record used while computing streak lengths.
struct GateOccurrence {
    occurred_at: DateTime<Utc>,
    passed: bool,
}

/// Compute per-(project, gate) consecutive failure streaks from historical events.
///
/// Reads `PreflightCompleted` events from the lookback window, groups gate
/// results by `(project, gate.name)`, sorts by `occurred_at` descending, and
/// counts consecutive failures from the most recent occurrence until a pass or
/// the lookback boundary.
///
/// Returns a map of `(project, gate) => streak_length`.
pub fn streaks(events: &[Event], lookback_days: u32) -> HashMap<(String, String), u32> {
    let cutoff = Utc::now() - Duration::days(i64::from(lookback_days));

    let mut gate_history: HashMap<(String, String), Vec<GateOccurrence>> = HashMap::new();

    for event in events {
        if event.event_type != EventType::PreflightCompleted {
            continue;
        }
        if event.occurred_at < cutoff {
            continue;
        }
        let Ok(payload) = event.parse_payload::<PreflightCompletedPayload>() else {
            continue;
        };
        for result in &payload.results {
            let key = (payload.project.clone(), result.name.clone());
            gate_history.entry(key).or_default().push(GateOccurrence {
                occurred_at: event.occurred_at,
                passed: result.passed || result.fix_applied,
            });
        }
    }

    let mut result = HashMap::new();

    for (key, mut occurrences) in gate_history {
        // Sort descending so we walk from newest to oldest.
        occurrences.sort_by_key(|o| std::cmp::Reverse(o.occurred_at));

        let mut streak: u32 = 0;
        for occ in &occurrences {
            if occ.passed {
                break;
            }
            streak += 1;
        }

        if streak > 0 {
            result.insert(key, streak);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Build verdicts
// ---------------------------------------------------------------------------

/// Build the full set of verdicts applying: correlation, classification,
/// decision boundary, streak override, and benign reclassification.
///
/// 1. Correlate failures → (incidents, remaining)
/// 2. Compute streaks over `all_events`
/// 3. For each remaining failure: classify → base decision → apply overrides
/// 4. Return (verdicts, incidents)
pub fn build_verdicts(
    raw_failures: Vec<RawFailure>,
    all_events: &[Event],
    streak_lookback_days: u32,
) -> (Vec<FailureVerdict>, Vec<InfraIncident>) {
    let (incidents, remaining) = correlate(raw_failures);
    let streak_map = streaks(all_events, streak_lookback_days);

    let mut verdicts = Vec::new();

    for failure in remaining {
        let mut class = classify(&failure);
        let key = (failure.project.clone(), failure.gate_name.clone());
        let streak = streak_map.get(&key).copied().unwrap_or(0);

        // Streak override: N≥3 consecutive failures → ChronicDeadlock + Escalate
        if streak >= 3 {
            class = FailureClass::ChronicDeadlock;
        }

        let mut decision = decision_for(&class);

        // TriageRejectedNoise always maps to ReclassifyBenign (redundant but explicit)
        if class == FailureClass::TriageRejectedNoise {
            decision = Decision::ReclassifyBenign;
        }

        // Attach proposed_command for AutoFixable failures
        let proposed_command = if decision == Decision::AutoFixable {
            failure.fix_command.clone()
        } else {
            None
        };

        let evidence = failure.gate_output.chars().take(500).collect::<String>();

        verdicts.push(FailureVerdict {
            project: failure.project,
            gate: failure.gate_name,
            class,
            decision,
            evidence,
            proposed_command,
        });
    }

    (verdicts, incidents)
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Return the paths for the current and prior month JSONL files.
///
/// Exposed for callers that want to know which files will be read before
/// calling `read_events_jsonl`.
#[allow(dead_code)]
pub fn month_paths(events_dir: &Path) -> Vec<PathBuf> {
    let now = Utc::now();
    let mut paths = Vec::new();

    let prior = if now.month0() == 0 {
        chrono::NaiveDate::from_ymd_opt(now.year() - 1, 12, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(now.year(), now.month() - 1, 1)
    };
    if let Some(d) = prior {
        paths.push(events_dir.join(format!("{}.jsonl", d.format("%Y-%m"))));
    }
    paths.push(events_dir.join(format!("{}.jsonl", now.format("%Y-%m"))));
    paths
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use chrono::TimeZone as _;
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::triage::{Decision, FailureClass};
    use tempfile::TempDir;

    use super::*;

    // -------------------------------------------------------------------------
    // is_benign_decline
    // -------------------------------------------------------------------------

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

    // -------------------------------------------------------------------------
    // classify
    // -------------------------------------------------------------------------

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

    // -------------------------------------------------------------------------
    // decision_for
    // -------------------------------------------------------------------------

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

    // -------------------------------------------------------------------------
    // correlate
    // -------------------------------------------------------------------------

    fn infra_failure(project: &str, output: &str) -> RawFailure {
        RawFailure {
            project: project.to_string(),
            gate_name: "build".to_string(),
            gate_output: output.to_string(),
            exit_code: Some(1),
            fix_command: None,
            occurred_at: Utc::now(),
        }
    }

    #[test]
    fn correlate_collapses_n_ge_3_same_signature_to_one_incident() {
        let failures = vec![
            infra_failure("alpha", "os error 2: No such file"),
            infra_failure("beta", "os error 2: No such file"),
            infra_failure("gamma", "os error 2: No such file"),
        ];

        let (incidents, remaining) = correlate(failures);

        assert_eq!(incidents.len(), 1, "three distinct projects should collapse to one incident");
        assert_eq!(incidents[0].projects.len(), 3);
        assert_eq!(incidents[0].decision, Decision::SuppressInfra);
        assert!(remaining.is_empty(), "all three should be collapsed");
    }

    #[test]
    fn correlate_keeps_n_lt_3_same_signature_as_per_project() {
        let failures = vec![
            infra_failure("alpha", "os error 2: No such file"),
            infra_failure("beta", "os error 2: No such file"),
        ];

        let (incidents, remaining) = correlate(failures);

        assert!(incidents.is_empty(), "N<3 should not produce an incident");
        assert_eq!(remaining.len(), 2, "both failures should remain per-project");
    }

    #[test]
    fn correlate_does_not_collapse_non_infra_failures() {
        let failures = vec![
            RawFailure {
                project: "alpha".to_string(),
                gate_name: "test".to_string(),
                gate_output: "test suite failed".to_string(),
                exit_code: Some(1),
                fix_command: None,
                occurred_at: Utc::now(),
            },
            RawFailure {
                project: "beta".to_string(),
                gate_name: "test".to_string(),
                gate_output: "test suite failed".to_string(),
                exit_code: Some(1),
                fix_command: None,
                occurred_at: Utc::now(),
            },
            RawFailure {
                project: "gamma".to_string(),
                gate_name: "test".to_string(),
                gate_output: "test suite failed".to_string(),
                exit_code: Some(1),
                fix_command: None,
                occurred_at: Utc::now(),
            },
        ];

        let (incidents, remaining) = correlate(failures);

        assert!(incidents.is_empty(), "non-infra failures must not produce incidents");
        assert_eq!(remaining.len(), 3);
    }

    // -------------------------------------------------------------------------
    // run_failures — excludes self-healed gates
    // -------------------------------------------------------------------------

    fn make_preflight_event(project: &str, gate: &str, passed: bool, fix_applied: bool) -> Event {
        Event::new(
            EventType::PreflightCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "workflow": "maintain",
                "all_passed": passed,
                "required_passed": passed,
                "results": [{
                    "name": gate,
                    "command": "cargo fmt --check",
                    "passed": passed,
                    "required": true,
                    "output": "some output",
                    "exit_code": i32::from(!passed),
                    "fix_applied": fix_applied,
                }]
            }),
        )
    }

    #[test]
    fn run_failures_excludes_self_healed_gates() {
        let events = vec![
            make_preflight_event("alpha", "fmt", false, true), // self-healed → excluded
            make_preflight_event("beta", "fmt", false, false), // real failure → included
            make_preflight_event("gamma", "fmt", true, false), // passed → excluded
        ];
        // Set the window *after* creating events so occurred_at falls within it.
        let window_end = Utc::now() + Duration::seconds(1);
        let window_start = window_end - Duration::hours(1);

        let failures = run_failures(&events, window_start, window_end);

        assert_eq!(failures.len(), 1, "only the real failure should be included");
        assert_eq!(failures[0].project, "beta");
    }

    #[test]
    fn run_failures_respects_window_bounds() {
        let window_end = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let window_start = window_end - Duration::hours(2);

        // Event inside window
        let mut inside = make_preflight_event("alpha", "fmt", false, false);
        inside.occurred_at = Utc.with_ymd_and_hms(2026, 6, 1, 11, 0, 0).unwrap();

        // Event before window
        let mut before = make_preflight_event("beta", "fmt", false, false);
        before.occurred_at = Utc.with_ymd_and_hms(2026, 6, 1, 9, 0, 0).unwrap();

        let failures = run_failures(&[inside, before], window_start, window_end);

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].project, "alpha");
    }

    // -------------------------------------------------------------------------
    // streaks
    // -------------------------------------------------------------------------

    #[test]
    fn streaks_detects_consecutive_failures() {
        let events: Vec<Event> = (0..4)
            .map(|i| {
                let mut e = make_preflight_event("alpha", "fmt", false, false);
                e.occurred_at = Utc::now() - Duration::hours(i64::from(i) + 1);
                e
            })
            .collect();

        let result = streaks(&events, 30);
        let streak = result.get(&("alpha".to_string(), "fmt".to_string())).copied().unwrap_or(0);
        assert_eq!(streak, 4);
    }

    #[test]
    fn streaks_resets_on_pass() {
        let mut events: Vec<Event> = Vec::new();

        // Two failures
        for i in 0..2 {
            let mut e = make_preflight_event("alpha", "test", false, false);
            e.occurred_at = Utc::now() - Duration::hours(i64::from(i) + 1);
            events.push(e);
        }
        // A pass in the middle (older)
        let mut pass = make_preflight_event("alpha", "test", true, false);
        pass.occurred_at = Utc::now() - Duration::hours(3);
        events.push(pass);
        // Another failure even older
        let mut old_fail = make_preflight_event("alpha", "test", false, false);
        old_fail.occurred_at = Utc::now() - Duration::hours(4);
        events.push(old_fail);

        let result = streaks(&events, 30);
        let streak = result.get(&("alpha".to_string(), "test".to_string())).copied().unwrap_or(0);
        // The two recent failures form a streak; the pass stops counting backwards
        assert_eq!(streak, 2);
    }

    #[test]
    fn build_verdicts_streak_override_escalates_at_n_ge_3() {
        // Build 4 consecutive failures for the same gate
        let now = Utc::now();
        let window_start = now - Duration::hours(5);
        let window_end = now;

        let mut all_events: Vec<Event> = Vec::new();
        let mut raw_failures: Vec<RawFailure> = Vec::new();

        for i in 0..4_u32 {
            let mut e = make_preflight_event("alpha", "test", false, false);
            e.occurred_at = now - Duration::hours(i64::from(i) + 1);
            all_events.push(e.clone());
            raw_failures.push(RawFailure {
                project: "alpha".to_string(),
                gate_name: "test".to_string(),
                gate_output: "test failed".to_string(),
                exit_code: Some(1),
                fix_command: None,
                occurred_at: e.occurred_at,
            });
        }

        // Only use the most recent raw failure (as the triage window would)
        let recent: Vec<RawFailure> = raw_failures
            .into_iter()
            .filter(|f| f.occurred_at >= window_start && f.occurred_at <= window_end)
            .take(1)
            .collect();

        let (verdicts, _incidents) = build_verdicts(recent, &all_events, 30);

        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].class, FailureClass::ChronicDeadlock);
        assert_eq!(verdicts[0].decision, Decision::Escalate);
    }

    // -------------------------------------------------------------------------
    // JSONL reading
    // -------------------------------------------------------------------------

    #[test]
    fn read_events_jsonl_reads_events_after_since() {
        let dir = TempDir::new().unwrap();

        // Write a JSONL file with two events
        let event1 = Event::new(
            EventType::PreflightCompleted,
            "proj".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "proj", "workflow": "maintain", "all_passed": true, "required_passed": true, "results": []}),
        );
        let event2 = Event::new(
            EventType::PreflightCompleted,
            "proj2".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "proj2", "workflow": "maintain", "all_passed": false, "required_passed": false, "results": []}),
        );

        let now = Utc::now();
        let month = now.format("%Y-%m").to_string();
        let path = dir.path().join(format!("{month}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", serde_json::to_string(&event1).unwrap()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&event2).unwrap()).unwrap();
        drop(f);

        let since = now - Duration::hours(1);
        let loaded = read_events_jsonl(dir.path(), since);

        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn read_events_jsonl_skips_malformed_lines() {
        let dir = TempDir::new().unwrap();
        let now = Utc::now();
        let month = now.format("%Y-%m").to_string();
        let path = dir.path().join(format!("{month}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "this is not json at all").unwrap();

        let good = Event::new(
            EventType::PreflightCompleted,
            "proj".to_string(),
            Throttle::Full,
            serde_json::json!({"project": "proj", "workflow": "maintain", "all_passed": true, "required_passed": true, "results": []}),
        );
        writeln!(f, "{}", serde_json::to_string(&good).unwrap()).unwrap();
        drop(f);

        let since = now - Duration::hours(1);
        let events = read_events_jsonl(dir.path(), since);
        assert_eq!(events.len(), 1, "malformed line must not abort parsing");
    }
}
