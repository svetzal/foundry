use std::path::Path;

use anyhow::Result;
use foundry_sdk::gates::{GateDefinition, GateResult, GatesRunResult};

use crate::gateway::ShellGateway;

mod logs;
pub mod output;

use logs::{CommandCapture, GateLogWriter, RunRole};
pub use logs::{
    GATE_LOG_MAX_AGE, GATE_LOG_MAX_TOTAL_BYTES, GATE_LOG_STREAM_CAP_BYTES, RetentionPolicy,
    default_gate_log_root,
};
use output::{AUTOFIX_FAILED_MARKER, AUTOFIX_STILL_FAILING_MARKER};

/// Lines of output kept in a gate's `output` view per command run, shared
/// between stdout and stderr (see [`output`]).
const MAX_OUTPUT_LINES: usize = 200;

/// Environment override applied to every gate command (and its `fix_command`).
/// A gate run builds once and exits, so Cargo's incremental cache is only disk
/// cost. Scoped to gates: agents that edit and rebuild keep incremental builds.
const DISABLE_CARGO_INCREMENTAL: (&str, &str) = ("CARGO_INCREMENTAL", "0");

/// How long a gate (or its `fix_command`) may run when its definition sets no
/// `timeout`. Ten minutes: a full test suite on a large workspace routinely
/// passes five, and the shell's general five-minute default is meant for git
/// and other housekeeping, not builds. A gate that needs longer sets its own.
pub const DEFAULT_GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Environment variable that stretches every gate timeout on this host.
///
/// Gate files are written and calibrated on the fastest development machine;
/// a slower host sets a factor (for example `2.0`) on its daemon instead of
/// every repository carrying limits for the slowest host. Unset, empty,
/// non-numeric or non-positive values mean `1.0`.
pub const GATE_TIMEOUT_SCALE_ENV: &str = "FOUNDRY_GATE_TIMEOUT_SCALE";

/// Parse a gate-timeout scale factor, falling back to `1.0` for anything that
/// is not a finite positive number.
#[must_use]
pub fn parse_gate_timeout_scale(raw: Option<&str>) -> f64 {
    raw.and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f > 0.0)
        .unwrap_or(1.0)
}

/// The timeout a gate runs under: its own, or [`DEFAULT_GATE_TIMEOUT`], scaled
/// by `factor`.
#[must_use]
pub fn effective_gate_timeout(
    own: Option<std::time::Duration>,
    factor: f64,
) -> std::time::Duration {
    own.unwrap_or(DEFAULT_GATE_TIMEOUT).mul_f64(factor)
}

fn host_gate_timeout_scale() -> f64 {
    static SCALE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| {
        let raw = std::env::var(GATE_TIMEOUT_SCALE_ENV).ok();
        let set = raw.as_deref().is_some_and(|r| !r.trim().is_empty());
        let valid = raw.as_deref().and_then(|r| r.trim().parse::<f64>().ok()).is_some_and(|f| f.is_finite() && f > 0.0);
        if set && !valid {
            tracing::warn!(value = ?raw, "{GATE_TIMEOUT_SCALE_ENV} is not a positive number; using 1.0");
        }
        parse_gate_timeout_scale(raw.as_deref())
    })
}

/// Run all gates sequentially and return an aggregated result.
///
/// Self-healing: when a gate fails and declares a `fix_command`, the runner runs
/// the fix and then re-runs the gate `command` once. A passing re-check resolves
/// the gate (with `fix_applied = true`); the in-place changes the fix left in the
/// working tree are committed by the downstream commit step. This is what breaks
/// the preflight deadlock for mechanically-fixable gates (formatters, lint
/// autofix) — without it, a required formatting gate blocks the very step that
/// would reformat the code.
///
/// Each result's `output` is a bounded view (see [`output`]). A gate that
/// fails also has the complete output of every command it ran written under
/// [`default_gate_log_root`], with the paths in `logs`.
pub async fn run_gates(
    gates: &[GateDefinition],
    working_dir: &Path,
    shell: &dyn ShellGateway,
) -> Result<GatesRunResult> {
    run_gates_with_log_root(gates, working_dir, shell, &default_gate_log_root()).await
}

/// [`run_gates`], writing failed-gate logs under `log_root` instead of the
/// default root. Retention ([`RetentionPolicy::default`]) is applied to
/// `log_root` after each write.
pub async fn run_gates_with_log_root(
    gates: &[GateDefinition],
    working_dir: &Path,
    shell: &dyn ShellGateway,
    log_root: &Path,
) -> Result<GatesRunResult> {
    let mut log_writer = GateLogWriter::new(log_root, RetentionPolicy::default());
    let mut results = Vec::with_capacity(gates.len());

    for gate in gates {
        tracing::info!(gate = %gate.name, command = %gate.command, "running gate");

        let start = std::time::Instant::now();
        let initial = run_command(working_dir, &gate.command, gate.timeout, shell).await;
        let mut passed = initial.passed;
        let mut exit_code = initial.exit_code;
        let mut output = initial.view.clone();
        let mut fix_applied = false;
        let mut runs = vec![(RunRole::Initial, initial)];

        // Attempt a single self-heal when a gate fails and declares a fix command.
        if !passed && let Some(fix) = gate.fix_command.as_deref() {
            tracing::info!(gate = %gate.name, fix_command = %fix, "gate failed, attempting autofix");
            let fix_run = run_command(working_dir, fix, gate.timeout, shell).await;

            if fix_run.passed {
                let recheck = run_command(working_dir, &gate.command, gate.timeout, shell).await;
                if recheck.passed {
                    tracing::info!(gate = %gate.name, "autofix resolved gate failure");
                    passed = true;
                    fix_applied = true;
                    output.clone_from(&recheck.view);
                    exit_code = recheck.exit_code;
                } else {
                    tracing::info!(gate = %gate.name, "autofix ran but gate still failing");
                    output = format!("{output}\n{AUTOFIX_STILL_FAILING_MARKER}\n{}", recheck.view);
                }
                runs.push((RunRole::Fix, fix_run));
                runs.push((RunRole::Recheck, recheck));
            } else {
                tracing::info!(gate = %gate.name, "autofix command itself failed");
                output = format!("{output}\n{AUTOFIX_FAILED_MARKER}\n{}", fix_run.view);
                runs.push((RunRole::Fix, fix_run));
            }
        }

        let logs = if passed {
            Vec::new()
        } else {
            let captures: Vec<(RunRole, &CommandCapture)> = runs
                .iter()
                .filter_map(|(role, run)| run.capture.as_ref().map(|capture| (*role, capture)))
                .collect();
            log_writer.write_failed_gate(&gate.name, &captures)
        };

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
            logs,
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

/// One line naming the files that hold a failed gate's complete output, for
/// readers that show gate failures to an agent or the owner. `None` when no
/// logs were written.
#[must_use]
pub fn full_logs_note(logs: &[String]) -> Option<String> {
    (!logs.is_empty()).then(|| format!("full logs: {}", logs.join(", ")))
}

/// A failed gate's name, followed by its [`full_logs_note`] when it has logs.
#[must_use]
pub fn describe_failed_gate(gate: &GateResult) -> String {
    match full_logs_note(&gate.logs) {
        Some(note) => format!("{} ({note})", gate.name),
        None => gate.name.clone(),
    }
}

/// What one command run produced.
struct CommandRun {
    passed: bool,
    exit_code: i32,
    /// The bounded view kept in `GateResult::output`.
    view: String,
    /// The complete streams, kept until the gate's outcome is known so a
    /// failed gate can write them out. `None` when the command never ran.
    capture: Option<CommandCapture>,
}

/// Run a single shell command, keeping both its bounded view and its complete
/// output.
async fn run_command(
    working_dir: &Path,
    command: &str,
    timeout: Option<std::time::Duration>,
    shell: &dyn ShellGateway,
) -> CommandRun {
    let (key, value) = DISABLE_CARGO_INCREMENTAL;
    let env = [(key.to_string(), value.to_string())];
    let timeout = Some(effective_gate_timeout(timeout, host_gate_timeout_scale()));
    match shell.run(working_dir, "sh", &["-c", command], Some(&env), timeout).await {
        Ok(r) => CommandRun {
            passed: r.success,
            exit_code: r.exit_code,
            view: output::command_view(&r.stdout, &r.stderr, MAX_OUTPUT_LINES),
            capture: Some(CommandCapture {
                stdout: r.stdout,
                stderr: r.stderr,
            }),
        },
        Err(e) => CommandRun {
            passed: false,
            exit_code: -1,
            view: format!("gate execution error: {e}"),
            capture: None,
        },
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

    #[test]
    fn the_host_scale_stretches_both_default_and_own_timeouts() {
        use std::time::Duration;
        assert_eq!(effective_gate_timeout(None, 1.0), Duration::from_secs(600));
        assert_eq!(effective_gate_timeout(None, 2.0), Duration::from_secs(1_200));
        assert_eq!(
            effective_gate_timeout(Some(Duration::from_secs(2_400)), 2.0),
            Duration::from_secs(4_800)
        );
        assert_eq!(
            effective_gate_timeout(Some(Duration::from_secs(60)), 1.5),
            Duration::from_secs(90)
        );
    }

    #[test]
    fn a_missing_or_invalid_scale_is_one() {
        assert!((parse_gate_timeout_scale(None) - 1.0).abs() < f64::EPSILON);
        assert!((parse_gate_timeout_scale(Some("")) - 1.0).abs() < f64::EPSILON);
        assert!((parse_gate_timeout_scale(Some("fast")) - 1.0).abs() < f64::EPSILON);
        assert!((parse_gate_timeout_scale(Some("0")) - 1.0).abs() < f64::EPSILON);
        assert!((parse_gate_timeout_scale(Some("-2")) - 1.0).abs() < f64::EPSILON);
        assert!((parse_gate_timeout_scale(Some("inf")) - 1.0).abs() < f64::EPSILON);
        assert!((parse_gate_timeout_scale(Some(" 2.0 ")) - 2.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn a_gate_without_its_own_timeout_gets_ten_minutes() {
        let shell = FakeShellGateway::success();
        let mut timed = gate("slow", "cargo test --workspace", true);
        timed.timeout = Some(std::time::Duration::from_secs(1_800));
        let gates = vec![gate("test", "cargo test", true), timed];
        let dir = std::env::temp_dir();

        run_gates(&gates, &dir, shell.as_ref()).await.unwrap();

        // The host scale (FOUNDRY_GATE_TIMEOUT_SCALE) applies to both, so a
        // host that sets it still runs this test hermetically.
        let scale = host_gate_timeout_scale();
        let calls = shell.invocations();
        assert_eq!(calls[0].timeout, Some(DEFAULT_GATE_TIMEOUT.mul_f64(scale)));
        assert_eq!(DEFAULT_GATE_TIMEOUT, std::time::Duration::from_secs(600));
        assert_eq!(calls[1].timeout, Some(std::time::Duration::from_secs(1_800).mul_f64(scale)));
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

    fn result(stdout: &str, stderr: &str, success: bool) -> CommandResult {
        CommandResult {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            exit_code: i32::from(!success),
            success,
        }
    }

    fn numbered(prefix: &str, count: usize) -> String {
        (0..count).map(|i| format!("{prefix} {i}")).collect::<Vec<_>>().join("\n")
    }

    fn file_names(logs: &[String]) -> Vec<String> {
        logs.iter()
            .map(|p| Path::new(p).file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn stdout_failure_lines_survive_a_flood_of_stderr() {
        let stdout = format!(
            "{}\ntest foo::bar ... FAILED\n\nfailures:\n    foo::bar\n\ntest result: FAILED",
            numbered("test ok", 40)
        );
        let stderr = numbered("   Compiling some-crate v1.0.0", 500);
        let shell = FakeShellGateway::always(result(&stdout, &stderr, false));
        let gates = vec![gate("coverage", "cargo tarpaulin --fail-under 56", true)];
        let root = tempfile::tempdir().unwrap();

        let run = run_gates_with_log_root(&gates, Path::new("."), shell.as_ref(), root.path())
            .await
            .unwrap();

        let output = &run.results[0].output;
        assert!(output.contains("test foo::bar ... FAILED"), "{output}");
        assert!(output.contains("test result: FAILED"));
        assert!(output.contains(output::STDERR_MARKER));
        assert!(output.ends_with("Compiling some-crate v1.0.0 499"));
        assert!(output.lines().count() <= MAX_OUTPUT_LINES + 1);
    }

    #[tokio::test]
    async fn a_failed_gate_writes_its_complete_output_under_the_log_root() {
        let stdout = numbered("stdout line", 1_000);
        let stderr = numbered("stderr line", 1_000);
        let shell = FakeShellGateway::always(result(&stdout, &stderr, false));
        let gates = vec![gate("unit tests", "cargo test", true)];
        let root = tempfile::tempdir().unwrap();

        let run = run_gates_with_log_root(&gates, Path::new("."), shell.as_ref(), root.path())
            .await
            .unwrap();

        let logs = &run.results[0].logs;
        assert_eq!(file_names(logs), vec!["unit_tests.stdout.log", "unit_tests.stderr.log"]);
        for path in logs {
            assert!(Path::new(path).is_absolute(), "{path} must be absolute");
            assert!(Path::new(path).starts_with(std::path::absolute(root.path()).unwrap()));
        }
        assert_eq!(std::fs::read_to_string(&logs[0]).unwrap(), stdout);
        assert_eq!(std::fs::read_to_string(&logs[1]).unwrap(), stderr);
        let run_dir = Path::new(&logs[0]).parent().unwrap();
        let run_dir_name = run_dir.file_name().unwrap().to_string_lossy();
        assert_eq!(run_dir.parent().unwrap(), std::path::absolute(root.path()).unwrap());
        assert_eq!(run_dir_name.find('T'), Some(8), "{run_dir_name} starts with a timestamp");
    }

    #[tokio::test]
    async fn failed_gates_in_one_run_share_one_run_directory() {
        let shell = FakeShellGateway::always(result("out", "err", false));
        let gates = vec![
            gate("lint", "cargo clippy", true),
            gate("test", "cargo test", true),
        ];
        let root = tempfile::tempdir().unwrap();

        let run = run_gates_with_log_root(&gates, Path::new("."), shell.as_ref(), root.path())
            .await
            .unwrap();

        let dir = |r: &GateResult| Path::new(&r.logs[0]).parent().unwrap().to_path_buf();
        assert_eq!(dir(&run.results[0]), dir(&run.results[1]));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn a_passing_gate_writes_no_logs() {
        let shell = FakeShellGateway::always(result("all good", "Compiling", true));
        let gates = vec![gate("test", "cargo test", true)];
        let root = tempfile::tempdir().unwrap();
        let log_root = root.path().join("gate-logs");

        let run = run_gates_with_log_root(&gates, Path::new("."), shell.as_ref(), &log_root)
            .await
            .unwrap();

        assert!(run.results[0].logs.is_empty(), "{:?}", run.results[0].logs);
        assert!(!log_root.exists(), "a passing run leaves nothing on disk");
        let json = serde_json::to_string(&run.results[0]).unwrap();
        assert!(!json.contains("logs"), "passing results serialize as before: {json}");
    }

    #[tokio::test]
    async fn a_failed_self_heal_keeps_the_fix_and_recheck_logs() {
        let shell = FakeShellGateway::sequence(vec![
            result("", "initial type error", false),
            result("fixed 2 files", "", true),
            result("", "recheck type error", false),
        ]);
        let gates = vec![gate_with_fix(
            "compile",
            "build --check",
            "build --fix",
            true,
        )];
        let root = tempfile::tempdir().unwrap();

        let run = run_gates_with_log_root(&gates, Path::new("."), shell.as_ref(), root.path())
            .await
            .unwrap();

        let gate = &run.results[0];
        assert!(!gate.passed);
        assert_eq!(
            file_names(&gate.logs),
            vec![
                "compile.stderr.log",
                "compile.fix.stdout.log",
                "compile.recheck.stderr.log"
            ]
        );
        assert_eq!(std::fs::read_to_string(&gate.logs[2]).unwrap(), "recheck type error");
        assert!(gate.output.contains("initial type error"));
        assert!(gate.output.contains(output::AUTOFIX_STILL_FAILING_MARKER));
        assert!(gate.output.contains("recheck type error"));
    }

    #[tokio::test]
    async fn a_failed_fix_command_keeps_the_fix_logs() {
        let shell = FakeShellGateway::sequence(vec![
            result("", "needs formatting", false),
            result("", "formatter crashed", false),
        ]);
        let gates = vec![gate_with_fix("format", "fmt --check", "fmt", true)];
        let root = tempfile::tempdir().unwrap();

        let run = run_gates_with_log_root(&gates, Path::new("."), shell.as_ref(), root.path())
            .await
            .unwrap();

        let gate = &run.results[0];
        assert_eq!(file_names(&gate.logs), vec!["format.stderr.log", "format.fix.stderr.log"]);
        assert!(gate.output.contains(output::AUTOFIX_FAILED_MARKER));
        assert!(gate.output.contains("formatter crashed"));
    }

    #[tokio::test]
    async fn a_self_healed_gate_writes_no_logs() {
        let shell = FakeShellGateway::sequence(vec![
            result("", "needs formatting", false),
            result("", "", true),
            result("", "", true),
        ]);
        let gates = vec![gate_with_fix("format", "fmt --check", "fmt", true)];
        let root = tempfile::tempdir().unwrap();
        let log_root = root.path().join("gate-logs");

        let run = run_gates_with_log_root(&gates, Path::new("."), shell.as_ref(), &log_root)
            .await
            .unwrap();

        assert!(run.results[0].passed);
        assert!(run.results[0].logs.is_empty(), "{:?}", run.results[0].logs);
        assert!(!log_root.exists());
    }

    #[tokio::test]
    async fn an_unwritable_log_root_never_changes_the_outcome() {
        let shell = FakeShellGateway::always(result("out", "err", false));
        let gates = vec![gate("test", "cargo test", true)];
        let root = tempfile::tempdir().unwrap();
        let blocked = root.path().join("file");
        std::fs::write(&blocked, "not a directory").unwrap();

        let run = run_gates_with_log_root(&gates, Path::new("."), shell.as_ref(), &blocked)
            .await
            .unwrap();

        assert!(!run.results[0].passed);
        assert!(run.results[0].logs.is_empty(), "{:?}", run.results[0].logs);
        assert_eq!(run.results[0].output, format!("out\n{}\nerr", output::STDERR_MARKER));
    }

    #[test]
    fn a_failed_gate_is_described_with_its_logs() {
        let mut failed = GateResult {
            name: "test".into(),
            command: "cargo test".into(),
            passed: false,
            required: true,
            output: String::new(),
            exit_code: 101,
            duration_ms: None,
            fix_applied: false,
            logs: Vec::new(),
        };
        assert_eq!(describe_failed_gate(&failed), "test");
        failed.logs = vec!["/l/test.stdout.log".into()];
        assert_eq!(describe_failed_gate(&failed), "test (full logs: /l/test.stdout.log)");
    }

    #[test]
    fn the_full_logs_note_lists_every_path() {
        assert_eq!(full_logs_note(&[]), None);
        assert_eq!(
            full_logs_note(&["/a.stdout.log".into(), "/a.stderr.log".into()]).as_deref(),
            Some("full logs: /a.stdout.log, /a.stderr.log")
        );
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

    #[tokio::test]
    async fn gate_and_fix_commands_run_without_incremental_builds() {
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: String::new(),
                stderr: "files need formatting".to_string(),
                exit_code: 1,
                success: false,
            },
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let gates = vec![gate_with_fix(
            "format",
            "cargo fmt --check",
            "cargo fmt",
            true,
        )];
        let dir = std::env::temp_dir();

        run_gates(&gates, &dir, shell.as_ref()).await.unwrap();

        let invocations = shell.invocations();
        assert_eq!(invocations.len(), 3, "gate, fix, and re-check must all run");
        let expected = vec![("CARGO_INCREMENTAL".to_string(), "0".to_string())];
        for inv in &invocations {
            assert_eq!(inv.env, expected, "{:?} must run with CARGO_INCREMENTAL=0", inv.args);
        }
    }
}
