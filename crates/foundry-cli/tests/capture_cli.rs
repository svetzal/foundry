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
