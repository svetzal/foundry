use std::collections::HashMap;

use foundry_sdk::triage::{Decision, FailureClass, InfraIncident};

use super::RawFailure;
use super::classify::classify;

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
        let sig = super::classify::signature(&failure);
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use foundry_sdk::triage::Decision;

    use super::*;

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
}
