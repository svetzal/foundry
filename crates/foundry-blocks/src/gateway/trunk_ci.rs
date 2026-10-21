//! The GitHub boundary of the trunk CI watch: which CI runs a commit has, and
//! what a red one says.
//!
//! [`TrunkCiGateway`] is the seam. [`GhTrunkCiGateway`] asks the `gh` CLI
//! through a [`ShellGateway`] and holds no logic beyond building its arguments
//! and reading its output; what the runs mean is decided by
//! [`foundry_sdk::trunk_ci::conclude`]. The daemon's watcher calls it outside
//! any lock, at its own pace; nothing here sleeps or polls.

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use foundry_sdk::trunk_ci::CiRun;

use super::ShellGateway;

/// A future returned by a [`TrunkCiGateway`] call.
pub type CiFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = Result<T>> + Send + 'a>>;

/// Reads a GitHub repository's CI runs.
pub trait TrunkCiGateway: Send + Sync {
    /// Every CI run GitHub records for `commit` on `repo` (`owner/name`).
    fn runs_for_commit<'a>(&'a self, repo: &'a str, commit: &'a str) -> CiFuture<'a, Vec<CiRun>>;

    /// The names of run `run_id`'s jobs that concluded `failure`, `timed_out`
    /// or `startup_failure`.
    fn failed_jobs<'a>(&'a self, repo: &'a str, run_id: u64) -> CiFuture<'a, Vec<String>>;

    /// The end of run `run_id`'s failed-job log: its last
    /// [`LOG_TAIL_LINES`] lines, at most [`LOG_TAIL_BYTES`].
    fn failed_log_tail<'a>(&'a self, repo: &'a str, run_id: u64) -> CiFuture<'a, String>;
}

/// How many lines of a failed-job log a red watch keeps.
pub const LOG_TAIL_LINES: usize = 80;
/// The most bytes of a failed-job log a red watch keeps.
pub const LOG_TAIL_BYTES: usize = 8 * 1024;

/// The longest one `gh` call may take before the watcher gives up on it and
/// tries again on a later tick.
const GH_TIMEOUT: Duration = Duration::from_secs(120);

/// Job conclusions that make a job one of a run's failed jobs.
const FAILED_JOB_CONCLUSIONS: [&str; 3] = ["failure", "timed_out", "startup_failure"];

/// [`TrunkCiGateway`] over the `gh` CLI.
pub struct GhTrunkCiGateway {
    shell: Arc<dyn ShellGateway>,
}

impl GhTrunkCiGateway {
    /// Ask `gh` through `shell`.
    #[must_use]
    pub fn new(shell: Arc<dyn ShellGateway>) -> Self {
        Self { shell }
    }

    async fn gh(&self, args: &[&str]) -> Result<String> {
        let result = self.shell.run(Path::new("."), "gh", args, None, Some(GH_TIMEOUT)).await?;
        if !result.success {
            let reason = result
                .stderr
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("no error output");
            anyhow::bail!("gh {} failed (exit {}): {reason}", args.join(" "), result.exit_code);
        }
        Ok(result.stdout)
    }
}

/// `gh run list` arguments for every run of `commit` on `repo`.
#[must_use]
pub fn runs_args<'a>(repo: &'a str, commit: &'a str) -> Vec<&'a str> {
    vec![
        "run",
        "list",
        "--repo",
        repo,
        "--commit",
        commit,
        "--json",
        "databaseId,status,conclusion,workflowName,url,event,headBranch",
    ]
}

/// Read `gh run list --json` output.
///
/// # Errors
///
/// Returns an error when the output is not that JSON.
pub fn parse_runs(stdout: &str) -> Result<Vec<CiRun>> {
    serde_json::from_str(stdout).context("gh run list printed unreadable JSON")
}

/// Read the failed jobs out of `gh run view --json jobs` output.
///
/// # Errors
///
/// Returns an error when the output is not that JSON.
pub fn parse_failed_jobs(stdout: &str) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Jobs {
        #[serde(default)]
        jobs: Vec<Job>,
    }
    #[derive(serde::Deserialize)]
    struct Job {
        name: String,
        #[serde(default)]
        conclusion: String,
    }
    let jobs: Jobs = serde_json::from_str(stdout).context("gh run view printed unreadable JSON")?;
    Ok(jobs
        .jobs
        .into_iter()
        .filter(|job| FAILED_JOB_CONCLUSIONS.contains(&job.conclusion.as_str()))
        .map(|job| job.name)
        .collect())
}

/// The end of a failed-job log: its last [`LOG_TAIL_LINES`] lines, cut from
/// the front to at most [`LOG_TAIL_BYTES`] on a character boundary.
#[must_use]
pub fn log_tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    let tail = lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n");
    if tail.len() <= LOG_TAIL_BYTES {
        return tail;
    }
    let mut start = tail.len() - LOG_TAIL_BYTES;
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    tail[start..].to_string()
}

impl TrunkCiGateway for GhTrunkCiGateway {
    fn runs_for_commit<'a>(&'a self, repo: &'a str, commit: &'a str) -> CiFuture<'a, Vec<CiRun>> {
        Box::pin(async move { parse_runs(&self.gh(&runs_args(repo, commit)).await?) })
    }

    fn failed_jobs<'a>(&'a self, repo: &'a str, run_id: u64) -> CiFuture<'a, Vec<String>> {
        Box::pin(async move {
            let id = run_id.to_string();
            parse_failed_jobs(
                &self.gh(&["run", "view", &id, "--repo", repo, "--json", "jobs"]).await?,
            )
        })
    }

    fn failed_log_tail<'a>(&'a self, repo: &'a str, run_id: u64) -> CiFuture<'a, String> {
        Box::pin(async move {
            let id = run_id.to_string();
            Ok(log_tail(&self.gh(&["run", "view", &id, "--repo", repo, "--log-failed"]).await?))
        })
    }
}

/// An in-process [`TrunkCiGateway`] for tests: answers from what the test
/// scripted, records every call, and never touches GitHub.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct FakeTrunkCiGateway {
    runs: std::sync::Mutex<
        std::collections::HashMap<String, std::result::Result<Vec<CiRun>, String>>,
    >,
    jobs: std::sync::Mutex<std::collections::HashMap<u64, Vec<String>>>,
    logs: std::sync::Mutex<std::collections::HashMap<u64, String>>,
    calls: std::sync::Mutex<Vec<String>>,
}

#[cfg(any(test, feature = "test-support"))]
#[allow(
    clippy::unwrap_used,
    reason = "a test double whose locks are never poisoned"
)]
impl FakeTrunkCiGateway {
    /// A fake that reports no runs for any commit.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Report `runs` for `commit` from now on.
    pub fn set_runs(&self, commit: &str, runs: Vec<CiRun>) {
        self.runs.lock().unwrap().insert(commit.to_string(), Ok(runs));
    }

    /// Fail every request for `commit`'s runs with `error` from now on.
    pub fn fail_runs(&self, commit: &str, error: &str) {
        self.runs.lock().unwrap().insert(commit.to_string(), Err(error.to_string()));
    }

    /// Report `jobs` as run `run_id`'s failed jobs.
    pub fn set_failed_jobs(&self, run_id: u64, jobs: &[&str]) {
        self.jobs
            .lock()
            .unwrap()
            .insert(run_id, jobs.iter().map(ToString::to_string).collect());
    }

    /// Report `log` as run `run_id`'s failed-job log tail.
    pub fn set_log_tail(&self, run_id: u64, log: &str) {
        self.logs.lock().unwrap().insert(run_id, log.to_string());
    }

    /// Every call so far, as `<method> <repo> <commit or run id>`.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// A run of `commit`'s CI on `branch` with `status` and `conclusion`.
    #[must_use]
    pub fn run(id: u64, branch: &str, status: &str, conclusion: &str) -> CiRun {
        CiRun {
            database_id: id,
            status: status.to_string(),
            conclusion: conclusion.to_string(),
            workflow_name: "CI".to_string(),
            url: format!("https://github.com/o/r/actions/runs/{id}"),
            event: "push".to_string(),
            head_branch: branch.to_string(),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[allow(
    clippy::unwrap_used,
    reason = "a test double whose locks are never poisoned"
)]
impl TrunkCiGateway for FakeTrunkCiGateway {
    fn runs_for_commit<'a>(&'a self, repo: &'a str, commit: &'a str) -> CiFuture<'a, Vec<CiRun>> {
        self.calls.lock().unwrap().push(format!("runs {repo} {commit}"));
        let answer = self.runs.lock().unwrap().get(commit).cloned().unwrap_or(Ok(Vec::new()));
        Box::pin(async move { answer.map_err(|error| anyhow::anyhow!(error)) })
    }

    fn failed_jobs<'a>(&'a self, repo: &'a str, run_id: u64) -> CiFuture<'a, Vec<String>> {
        self.calls.lock().unwrap().push(format!("jobs {repo} {run_id}"));
        let jobs = self.jobs.lock().unwrap().get(&run_id).cloned().unwrap_or_default();
        Box::pin(async move { Ok(jobs) })
    }

    fn failed_log_tail<'a>(&'a self, repo: &'a str, run_id: u64) -> CiFuture<'a, String> {
        self.calls.lock().unwrap().push(format!("log {repo} {run_id}"));
        let log = self.logs.lock().unwrap().get(&run_id).cloned().unwrap_or_default();
        Box::pin(async move { Ok(log) })
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::gateway::CommandResult;
    use foundry_sdk::gateway::fakes::FakeShellGateway;

    use super::*;

    fn ok(stdout: &str) -> CommandResult {
        CommandResult {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    #[tokio::test]
    async fn runs_for_a_commit_ask_gh_run_list_for_that_commit_and_parse_its_json() {
        let shell = FakeShellGateway::always(ok(
            r#"[{"databaseId":9,"status":"completed","conclusion":"failure","workflowName":"CI",
                "url":"https://ci/9","event":"push","headBranch":"main"}]"#,
        ));
        let gateway = GhTrunkCiGateway::new(shell.clone());

        let runs = gateway.runs_for_commit("o/r", "abc123").await.unwrap();

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].database_id, 9);
        assert_eq!(runs[0].conclusion, "failure");
        let call = &shell.invocations()[0];
        assert_eq!(call.command, "gh");
        assert_eq!(
            call.args,
            vec![
                "run",
                "list",
                "--repo",
                "o/r",
                "--commit",
                "abc123",
                "--json",
                "databaseId,status,conclusion,workflowName,url,event,headBranch"
            ]
        );
        assert_eq!(call.timeout, Some(GH_TIMEOUT));
    }

    #[tokio::test]
    async fn failed_jobs_name_only_failed_timed_out_and_unstarted_jobs() {
        let shell = FakeShellGateway::always(ok(r#"{"jobs":[
            {"name":"build","conclusion":"success"},
            {"name":"test (macos)","conclusion":"failure"},
            {"name":"slow","conclusion":"timed_out"},
            {"name":"setup","conclusion":"startup_failure"},
            {"name":"docs","conclusion":"skipped"},
            {"name":"running","conclusion":""}]}"#));
        let gateway = GhTrunkCiGateway::new(shell.clone());

        let jobs = gateway.failed_jobs("o/r", 42).await.unwrap();

        assert_eq!(jobs, vec!["test (macos)", "slow", "setup"]);
        assert_eq!(
            shell.invocations()[0].args,
            vec!["run", "view", "42", "--repo", "o/r", "--json", "jobs"]
        );
    }

    #[tokio::test]
    async fn the_failed_log_tail_asks_for_the_failed_log_and_keeps_its_end() {
        let log = (1..=200).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let shell = FakeShellGateway::always(ok(&log));
        let gateway = GhTrunkCiGateway::new(shell.clone());

        let tail = gateway.failed_log_tail("o/r", 42).await.unwrap();

        assert!(tail.starts_with("line 121\n"), "{tail}");
        assert!(tail.ends_with("line 200"), "{tail}");
        assert_eq!(tail.lines().count(), LOG_TAIL_LINES);
        assert_eq!(
            shell.invocations()[0].args,
            vec!["run", "view", "42", "--repo", "o/r", "--log-failed"]
        );
    }

    #[test]
    fn a_long_log_tail_is_capped_on_a_character_boundary() {
        let line = "é".repeat(200);
        let log = vec![line.as_str(); 40].join("\n");
        let tail = log_tail(&log);
        assert!(tail.len() <= LOG_TAIL_BYTES);
        assert!(log.ends_with(&tail));
    }

    #[tokio::test]
    async fn a_gh_failure_is_an_error_naming_its_first_stderr_line() {
        let shell = FakeShellGateway::always(CommandResult {
            stdout: String::new(),
            stderr: "\nHTTP 502: bad gateway\nmore".to_string(),
            exit_code: 1,
            success: false,
        });
        let gateway = GhTrunkCiGateway::new(shell);

        let error = gateway.runs_for_commit("o/r", "abc").await.unwrap_err().to_string();

        assert!(error.contains("HTTP 502: bad gateway"), "{error}");
        assert!(error.contains("exit 1"), "{error}");
    }

    #[test]
    fn unreadable_gh_output_is_an_error() {
        assert!(parse_runs("not json").is_err());
        assert!(parse_failed_jobs("not json").is_err());
        assert_eq!(parse_failed_jobs("{}").unwrap(), Vec::<String>::new());
    }
}
