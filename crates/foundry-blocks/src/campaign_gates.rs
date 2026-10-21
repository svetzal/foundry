//! Campaign done-evidence gates, run against a project checkout.
//!
//! One runner serves every caller that evaluates a campaign's `gate` done
//! evidence: formation (`blocks::advance_campaign`) runs it against the
//! registered trunk checkout on every advance, and the daemon runs the same
//! function when an owner adds a campaign or amends its done evidence, so the
//! author sees what formation will see before any cycle runs. A second copy of
//! this loop would let the probe and formation disagree about the same gate.

use std::path::Path;

use foundry_sdk::campaign::DoneEvidence;
use foundry_sdk::gates::{GateDefinition, GateResult};

use crate::gateway::ShellGateway;

/// How much of one gate's output a probe returns to the author.
///
/// Enough for the matches of a lexical gate (the false positives an author
/// needs to see) and the tail of a failing command, small enough that a
/// definition with many gates stays a readable CLI response. A failed gate's
/// complete output is in the files its `logs` names.
pub const PROBE_OUTPUT_EXCERPT_BYTES: usize = 2048;

/// Run every `gate` item of `evidence` in `repo`, in definition order.
///
/// Each gate is named `campaign_done_<n>` after its position in `evidence`
/// (review items count), the names formation reports. A gate whose declared
/// artifacts are missing or not repository-relative fails without running its
/// command. Review items produce no result.
///
/// # Errors
///
/// Propagates a fault from the gate runner itself. A command that cannot be
/// started is a failed gate, not an error.
pub async fn run_done_gates(
    shell: &dyn ShellGateway,
    repo: &Path,
    evidence: &[DoneEvidence],
) -> anyhow::Result<Vec<GateResult>> {
    let mut results = Vec::new();
    for (index, item) in evidence.iter().enumerate() {
        let DoneEvidence::Gate {
            command,
            required,
            artifacts,
        } = item
        else {
            continue;
        };
        let name = format!("campaign_done_{}", index + 1);
        if let Some(error) = artifact_error(repo, artifacts) {
            results.push(GateResult {
                name,
                command: command.clone(),
                passed: false,
                required: *required,
                output: error,
                exit_code: 1,
                duration_ms: Some(0),
                fix_applied: false,
                logs: Vec::new(),
            });
            continue;
        }
        let gate = GateDefinition {
            name,
            command: command.clone(),
            required: *required,
            timeout: None,
            fix_command: None,
        };
        results.extend(crate::gate_runner::run_gates(&[gate], repo, shell).await?.results);
    }
    Ok(results)
}

/// A gate's output bounded to [`PROBE_OUTPUT_EXCERPT_BYTES`], and whether it
/// had to be shortened. The tail of every output section is kept, so a flood
/// of stderr cannot hide the stdout lines that matched.
#[must_use]
pub fn probe_excerpt(output: &str) -> (String, bool) {
    let excerpt = crate::gate_runner::output::compact_output(output, PROBE_OUTPUT_EXCERPT_BYTES);
    let truncated = excerpt != output;
    (excerpt, truncated)
}

fn artifact_error(repo: &Path, artifacts: &[String]) -> Option<String> {
    artifacts.iter().find_map(|artifact| {
        let relative = Path::new(artifact);
        let safe = !relative.is_absolute()
            && relative.components().all(|component| {
                matches!(component, std::path::Component::Normal(_) | std::path::Component::CurDir)
            });
        if !safe {
            return Some(format!("campaign gate artifact must be repository-relative: {artifact}"));
        }
        if !repo.join(relative).exists() {
            return Some(format!("campaign gate artifact missing: {artifact}"));
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use foundry_sdk::campaign::DoneEvidence;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::{PROBE_OUTPUT_EXCERPT_BYTES, probe_excerpt, run_done_gates};

    #[tokio::test]
    async fn missing_declared_artifact_fails_gate_without_running_command() {
        let dir = tempfile::tempdir().unwrap();
        let shell = FakeShellGateway::success();
        let evidence = vec![DoneEvidence::Gate {
            command: "mix test test/required_test.exs".to_string(),
            required: true,
            artifacts: vec!["test/required_test.exs".to_string()],
        }];

        let results = run_done_gates(&*shell, dir.path(), &evidence).await.unwrap();

        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
        assert!(results[0].output.contains("campaign gate artifact missing"));
        assert!(shell.invocations().is_empty());
    }

    #[tokio::test]
    async fn present_declared_artifact_allows_gate_command_to_run() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("test")).unwrap();
        std::fs::write(dir.path().join("test/required_test.exs"), "test").unwrap();
        let shell = FakeShellGateway::success();
        let evidence = vec![DoneEvidence::Gate {
            command: "mix test test/required_test.exs".to_string(),
            required: true,
            artifacts: vec!["test/required_test.exs".to_string()],
        }];

        let results = run_done_gates(&*shell, dir.path(), &evidence).await.unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].passed);
        assert_eq!(shell.invocations().len(), 1);
    }

    /// Gates keep their definition position in the name, review items run
    /// nothing, and each command runs in the checkout it was given.
    #[tokio::test]
    async fn gates_are_named_by_position_and_run_in_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "src/net.rs:3: java.net.SocketTimeoutException".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let evidence = vec![
            DoneEvidence::Review {
                statement: "reviewed".to_string(),
            },
            DoneEvidence::Gate {
                command: "! rg -n Timeout src".to_string(),
                required: false,
                artifacts: vec![],
            },
        ];

        let results = run_done_gates(&*shell, dir.path(), &evidence).await.unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "campaign_done_2");
        assert!(!results[0].required);
        assert!(results[0].output.contains("SocketTimeoutException"));
        let invocations = shell.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].working_dir, dir.path().display().to_string());
        assert_eq!(invocations[0].args, vec!["-c".to_string(), "! rg -n Timeout src".to_string()]);
    }

    #[test]
    fn probe_excerpt_keeps_short_output_and_bounds_long_output() {
        assert_eq!(probe_excerpt("one match"), ("one match".to_string(), false));
        let long = "src/a.rs:1: match\n".repeat(1000);
        let (excerpt, truncated) = probe_excerpt(&long);
        assert!(truncated);
        assert!(excerpt.len() < long.len());
        assert!(excerpt.len() <= PROBE_OUTPUT_EXCERPT_BYTES + 200, "{}", excerpt.len());
        assert!(excerpt.ends_with("src/a.rs:1: match"), "the tail is kept");
    }
}
