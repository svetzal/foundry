use std::path::Path;

use anyhow::Result;
use foundry_core::gates::{GateDefinition, GateResult, GatesRunResult};

use crate::gateway::ShellGateway;

/// Maximum number of output lines to keep per gate.
const MAX_OUTPUT_LINES: usize = 200;

/// Run all gates sequentially and return an aggregated result.
pub async fn run_gates(
    gates: &[GateDefinition],
    working_dir: &Path,
    shell: &dyn ShellGateway,
) -> Result<GatesRunResult> {
    let mut results = Vec::with_capacity(gates.len());

    for gate in gates {
        tracing::info!(gate = %gate.name, command = %gate.command, "running gate");

        let cmd_result =
            shell.run(working_dir, "sh", &["-c", &gate.command], None, gate.timeout).await;

        let (passed, output, exit_code) = match cmd_result {
            Ok(r) => {
                let combined = format!("{}\n{}", r.stdout, r.stderr);
                let trimmed = tail_lines(&combined, MAX_OUTPUT_LINES);
                (r.success, trimmed, r.exit_code)
            }
            Err(e) => (false, format!("gate execution error: {e}"), -1),
        };

        tracing::info!(gate = %gate.name, passed, exit_code, "gate completed");

        results.push(GateResult {
            name: gate.name.clone(),
            command: gate.command.clone(),
            passed,
            required: gate.required,
            output,
            exit_code,
        });
    }

    let all_passed = results.iter().all(|r| r.passed);
    let required_passed = results.iter().filter(|r| r.required).all(|r| r.passed);

    Ok(GatesRunResult {
        all_passed,
        required_passed,
        results,
    })
}

/// Keep at most the last `n` lines from `text`, trimming leading/trailing whitespace.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= n {
        text.trim().to_string()
    } else {
        lines[lines.len() - n..].join("\n").trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use foundry_core::gates::GateDefinition;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::*;

    fn gate(name: &str, command: &str, required: bool) -> GateDefinition {
        GateDefinition {
            name: name.to_string(),
            command: command.to_string(),
            required,
            timeout: None,
        }
    }

    #[tokio::test]
    async fn all_gates_pass() {
        let shell = FakeShellGateway::success();
        let gates = vec![
            gate("fmt", "cargo fmt --check", true),
            gate("test", "cargo test", true),
        ];
        let dir = std::env::temp_dir();

        let result = run_gates(&gates, &dir, shell.as_ref()).await.unwrap();

        assert!(result.all_passed);
        assert!(result.required_passed);
        assert_eq!(result.results.len(), 2);
        assert!(result.results[0].passed);
        assert!(result.results[1].passed);
    }

    #[tokio::test]
    async fn required_gate_fails() {
        let shell = FakeShellGateway::failure("lint error");
        let gates = vec![gate("clippy", "cargo clippy", true)];
        let dir = std::env::temp_dir();

        let result = run_gates(&gates, &dir, shell.as_ref()).await.unwrap();

        assert!(!result.all_passed);
        assert!(!result.required_passed);
        assert!(!result.results[0].passed);
        assert!(result.results[0].required);
    }

    #[tokio::test]
    async fn optional_gate_fails_required_passes() {
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: "optional lint warning".to_string(),
                exit_code: 1,
                success: false,
            },
        ]);
        let gates = vec![
            gate("fmt", "cargo fmt --check", true),
            gate("lint-optional", "cargo clippy", false),
        ];
        let dir = std::env::temp_dir();

        let result = run_gates(&gates, &dir, shell.as_ref()).await.unwrap();

        assert!(!result.all_passed);
        assert!(result.required_passed);
        assert!(result.results[0].passed);
        assert!(!result.results[1].passed);
    }

    #[tokio::test]
    async fn empty_gates_returns_all_passed() {
        let shell = FakeShellGateway::success();
        let dir = std::env::temp_dir();

        let result = run_gates(&[], &dir, shell.as_ref()).await.unwrap();

        assert!(result.all_passed);
        assert!(result.required_passed);
        assert!(result.results.is_empty());
    }

    #[tokio::test]
    async fn gate_with_timeout() {
        let shell = FakeShellGateway::success();
        let gates = vec![GateDefinition {
            name: "slow".to_string(),
            command: "make test".to_string(),
            required: true,
            timeout: Some(Duration::from_secs(60)),
        }];
        let dir = std::env::temp_dir();

        let result = run_gates(&gates, &dir, shell.as_ref()).await.unwrap();

        assert!(result.all_passed);
    }

    #[tokio::test]
    async fn output_is_captured() {
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "test output line".to_string(),
            stderr: "stderr line".to_string(),
            exit_code: 0,
            success: true,
        });
        let gates = vec![gate("test", "cargo test", true)];
        let dir = std::env::temp_dir();

        let result = run_gates(&gates, &dir, shell.as_ref()).await.unwrap();

        assert!(result.results[0].output.contains("test output line"));
        assert!(result.results[0].output.contains("stderr line"));
    }

    #[test]
    fn tail_lines_over_limit_truncates() {
        let input: String = (0..201).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let result = tail_lines(&input, 200);
        assert_eq!(result.lines().count(), 200);
        // Should keep the last 200 lines (lines 1..=200)
        assert!(result.starts_with("line 1"));
        assert!(result.ends_with("line 200"));
    }

    #[test]
    fn tail_lines_at_limit_unchanged() {
        let input: String = (0..200).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let result = tail_lines(&input, 200);
        assert_eq!(result.lines().count(), 200);
        assert!(result.starts_with("line 0"));
        assert!(result.ends_with("line 199"));
    }

    #[test]
    fn tail_lines_trims_whitespace() {
        let input = "\n  hello\nworld  \n";
        let result = tail_lines(input, 200);
        assert_eq!(result, "hello\nworld");
    }

    #[tokio::test]
    async fn invocations_use_sh_c() {
        let shell: Arc<FakeShellGateway> = FakeShellGateway::success();
        let gates = vec![gate("fmt", "cargo fmt --check", true)];
        let dir = std::env::temp_dir();

        run_gates(&gates, &dir, shell.as_ref()).await.unwrap();

        let invocations = shell.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].command, "sh");
        assert_eq!(invocations[0].args, vec!["-c", "cargo fmt --check"]);
    }
}
