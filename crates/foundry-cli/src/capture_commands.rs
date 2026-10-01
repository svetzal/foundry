//! Run a command without copying its verbose output into an agent conversation.
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};

fn tail(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(2048)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Preserve both streams and return the child's exit code, including failures.
pub async fn capture(log_dir: Option<PathBuf>, command: Vec<String>) -> Result<i32> {
    let directory = log_dir.unwrap_or_else(|| foundry_sdk::paths::foundry_home().join("tool-logs"));
    std::fs::create_dir_all(&directory)?;
    let id = format!(
        "{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().context("clock out of range")?
    );
    let stdout = directory.join(format!("{id}.stdout.log"));
    let stderr = directory.join(format!("{id}.stderr.log"));
    let program = command.first().context("capture requires a command after --")?;
    let status = tokio::process::Command::new(program)
        .args(&command[1..])
        .stdin(Stdio::inherit())
        .stdout(std::fs::OpenOptions::new().write(true).create_new(true).open(&stdout)?)
        .stderr(std::fs::OpenOptions::new().write(true).create_new(true).open(&stderr)?)
        .kill_on_drop(true)
        .status()
        .await
        .with_context(|| format!("run {program}"))?;
    let code = status.code().unwrap_or(1);
    let failures = if status.success() {
        String::new()
    } else {
        format!("{}\n{}", tail(&stdout)?, tail(&stderr)?)
    };
    print!("{}", crate::render::capture::result(program, code, &stdout, &stderr, &failures));
    Ok(code)
}

/// CLI boundary: preserve the child's exit status for shell callers.
pub async fn run(log_dir: Option<PathBuf>, command: Vec<String>) -> Result<()> {
    let code = capture(log_dir, command).await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
