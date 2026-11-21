#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::process::Command;

#[test]
fn capture_retains_large_logs_returns_real_exit_and_bounds_failure_output() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_foundry"))
        .args(["capture", "--log-dir", dir.path().to_str().unwrap(), "--", "sh", "-c",
            "i=0; while [ $i -lt 4000 ]; do echo verbose-build-line; i=$((i+1)); done; echo actual-failure >&2; exit 7"])
        .output().unwrap();
    assert_eq!(output.status.code(), Some(7));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.len() < 5000);
    assert!(text.contains("actual-failure"));
    let stdout = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with("stdout.log"))
        .unwrap();
    let full = std::fs::read_to_string(stdout).unwrap();
    assert_eq!(full.lines().count(), 4000);
}

/// An agent session runs `foundry capture -- <command>` without a log
/// directory. Its logs go to the session's scratch directory outside the
/// project checkout, not into the repository.
#[test]
fn capture_without_a_log_dir_writes_into_the_session_scratch_directory() {
    let checkout = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_foundry"))
        .current_dir(checkout.path())
        .env("FOUNDRY_SCRATCH_DIR", scratch.path())
        .args(["capture", "--", "sh", "-c", "echo probe-output"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let logs: Vec<_> = std::fs::read_dir(scratch.path().join("logs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(logs.len(), 2, "one stdout and one stderr log: {logs:?}");
    let stdout = logs.iter().find(|p| p.to_string_lossy().ends_with("stdout.log")).unwrap();
    assert_eq!(std::fs::read_to_string(stdout).unwrap(), "probe-output\n");
    assert_eq!(
        std::fs::read_dir(checkout.path()).unwrap().count(),
        0,
        "nothing is written into the working directory"
    );
}
