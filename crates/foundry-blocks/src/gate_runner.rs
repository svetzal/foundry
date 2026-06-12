use std::path::Path;

use anyhow::Result;
use foundry_sdk::gates::{GateDefinition, GateResult, GatesRunResult};

use crate::gateway::ShellGateway;

/// Maximum number of output lines to keep per gate.
const MAX_OUTPUT_LINES: usize = 200;

/// Run all gates sequentially and return an aggregated result.
///
/// Self-healing: when a gate fails and declares a `fix_command`, the runner runs
/// the fix and then re-runs the gate `command` once. A passing re-check resolves
/// the gate (with `fix_applied = true`); the in-place changes the fix left in the
/// working tree are committed by the downstream commit step. This is what breaks
/// the preflight deadlock for mechanically-fixable gates (formatters, lint
/// autofix) — without it, a required formatting gate blocks the very step that
/// would reformat the code.
pub async fn run_gates(
    gates: &[GateDefinition],
    working_dir: &Path,
    shell: &dyn ShellGateway,
) -> Result<GatesRunResult> {
    let mut results = Vec::with_capacity(gates.len());

    for gate in gates {
        tracing::info!(gate = %gate.name, command = %gate.command, "running gate");

        let start = std::time::Instant::now();
        let (mut passed, mut output, mut exit_code) =
            run_command(working_dir, &gate.command, gate.timeout, shell).await;
        let mut fix_applied = false;

        // Attempt a single self-heal when a gate fails and declares a fix command.
        if !passed {
            if let Some(fix) = gate.fix_command.as_deref() {
                tracing::info!(gate = %gate.name, fix_command = %fix, "gate failed, attempting autofix");
                let (fix_ok, fix_output, _) =
                    run_command(working_dir, fix, gate.timeout, shell).await;

                if fix_ok {
                    let (recheck_passed, recheck_output, recheck_exit) =
                        run_command(working_dir, &gate.command, gate.timeout, shell).await;
                    if recheck_passed {
                        tracing::info!(gate = %gate.name, "autofix resolved gate failure");
                        passed = true;
                        fix_applied = true;
                        output = recheck_output;
                        exit_code = recheck_exit;
                    } else {
                        tracing::info!(gate = %gate.name, "autofix ran but gate still failing");
                        output = format!(
                            "{output}\n--- autofix attempted, gate still failing ---\n{recheck_output}"
                        );
                    }
                } else {
                    tracing::info!(gate = %gate.name, "autofix command itself failed");
                    output = format!("{output}\n--- autofix command failed ---\n{fix_output}");
                }
            }
        }

        let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(gate = %gate.name, passed, exit_code, duration_ms, fix_applied, "gate completed");

        results.push(GateResult {
            name: gate.name.clone(),
            command: gate.command.clone(),
            passed,
            required: gate.required,
            output,
            exit_code,
            duration_ms: Some(duration_ms),
            fix_applied,
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

/// Run a single shell command and return `(passed, trimmed_output, exit_code)`.
async fn run_command(
    working_dir: &Path,
    command: &str,
    timeout: Option<std::time::Duration>,
    shell: &dyn ShellGateway,
) -> (bool, String, i32) {
    match shell.run(working_dir, "sh", &["-c", command], None, timeout).await {
        Ok(r) => {
            let combined = format!("{}\n{}", r.stdout, r.stderr);
            (r.success, tail_lines(&combined, MAX_OUTPUT_LINES), r.exit_code)
        }
        Err(e) => (false, format!("gate execution error: {e}"), -1),
    }
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

    use foundry_sdk::gates::GateDefinition;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::*;

    fn gate(name: &str, command: &str, required: bool) -> GateDefinition {
        GateDefinition {
            name: name.to_string(),
            command: command.to_string(),
            required,
            timeout: None,
            fix_command: None,
        }
    }

    fn gate_with_fix(name: &str, command: &str, fix: &str, required: bool) -> GateDefinition {
        GateDefinition {
            name: name.to_string(),
            command: command.to_string(),
            required,
            timeout: None,
            fix_command: Some(fix.to_string()),
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
        assert!(result.results[0].duration_ms.is_some(), "duration_ms must be recorded");
        assert!(result.results[1].duration_ms.is_some(), "duration_ms must be recorded");
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
    async fn fix_command_self_heals_failing_gate() {
        // Gate fails, fix succeeds, re-check passes → gate resolved, fix_applied set.
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: String::new(),
                stderr: "files need formatting".to_string(),
                exit_code: 1,
                success: false,
            },
            CommandResult {
                stdout: "reformatted 3 files".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let gates = vec![gate_with_fix("format", "fmt --check", "fmt", true)];
        let dir = std::env::temp_dir();

        let result = run_gates(&gates, &dir, shell.as_ref()).await.unwrap();

        assert!(result.all_passed, "gate should pass after autofix");
        assert!(result.required_passed);
        assert!(result.results[0].passed);
        assert!(result.results[0].fix_applied, "fix_applied must be flagged");
        assert_eq!(result.results[0].exit_code, 0);
    }

    #[tokio::test]
    async fn fix_command_that_fails_to_resolve_leaves_gate_failed() {
        // Gate fails, fix runs, but re-check still fails → gate stays failed, no flag.
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: String::new(),
                stderr: "type error".to_string(),
                exit_code: 1,
                success: false,
            },
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: "type error".to_string(),
                exit_code: 1,
                success: false,
            },
        ]);
        let gates = vec![gate_with_fix(
            "compile",
            "build --check",
            "build --fix",
            true,
        )];
        let dir = std::env::temp_dir();

        let result = run_gates(&gates, &dir, shell.as_ref()).await.unwrap();

        assert!(!result.all_passed);
        assert!(!result.required_passed);
        assert!(!result.results[0].passed);
        assert!(!result.results[0].fix_applied);
    }

    #[tokio::test]
    async fn gate_without_fix_command_does_not_self_heal() {
        let shell = FakeShellGateway::failure("lint error");
        let gates = vec![gate("clippy", "cargo clippy", true)];
        let dir = std::env::temp_dir();

        let result = run_gates(&gates, &dir, shell.as_ref()).await.unwrap();

        assert!(!result.results[0].passed);
        assert!(!result.results[0].fix_applied);
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
            fix_command: None,
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
