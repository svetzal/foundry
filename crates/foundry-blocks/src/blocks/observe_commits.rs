//! `ObserveCommits` — first block of the daily commit-digest formation.
//!
//! Sinks on `CommitDigestStarted`. Snapshots the active project registry,
//! shells out to `git log --since="24 hours ago"` per project, and emits
//! `CommitsObserved` with the parsed evidence. Failure in a single project's
//! `git log` is captured inline as `ProjectCommits.error` so the chain
//! continues — the digest is best-effort.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{CommitInfo, CommitsObservedPayload, ProjectCommits};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

use super::{TriggerContext, emit_result};

/// Width of the wall-clock window passed to `git log --since`. Constant so the
/// emitted payload's `window_hours` field always matches the actual query.
const WINDOW_HOURS: u32 = 24;
const WINDOW_SPEC: &str = "24 hours ago";

/// Per-project git-log timeout. Local logs return in milliseconds for sane
/// repos; we cap at 30s to bound a hung `git` process from blocking the chain.
const GIT_LOG_TIMEOUT: Duration = Duration::from_secs(30);

/// ASCII unit-separator. Used as the field delimiter in our `git log
/// --pretty=format` string so commit subjects can safely contain whitespace,
/// pipes, commas, or anything else that would confuse a printable separator.
const FIELD_SEP: char = '\u{001f}';

task_block_new! {
    /// Observes the day's git activity across every active registry project
    /// and emits `CommitsObserved` carrying the raw evidence for the
    /// downstream summariser. Observer — never gated by throttle.
    pub struct ObserveCommits {
        shell: ShellGateway = crate::gateway::ProcessShellGateway
    }
}

impl TaskBlock for ObserveCommits {
    task_block_meta! {
        name: "Observe Commits",
        kind: Observer,
        sinks_on: [CommitDigestStarted],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project, throttle, ..
        } = TriggerContext::from_trigger(trigger);

        // Snapshot the active registry under a short read lock; drop before await.
        let snapshot: Vec<(String, String, PathBuf)> = {
            let guard = match super::read_registry(&self.registry) {
                Ok(g) => g,
                Err(e) => return Box::pin(async move { Err(e) }),
            };
            guard
                .active_projects()
                .iter()
                .map(|p| (p.name.clone(), p.branch.clone(), PathBuf::from(&p.path)))
                .collect()
        };

        let shell = Arc::clone(&self.shell);

        Box::pin(observe(project, throttle, snapshot, shell))
    }
}

async fn observe(
    project: String,
    throttle: foundry_sdk::throttle::Throttle,
    snapshot: Vec<(String, String, PathBuf)>,
    shell: Arc<dyn ShellGateway>,
) -> anyhow::Result<TaskBlockResult> {
    let mut projects = Vec::with_capacity(snapshot.len());
    for (name, branch, path) in snapshot {
        let entry = run_git_log(&name, &branch, &path, shell.as_ref()).await;
        projects.push(entry);
    }

    let payload = CommitsObservedPayload {
        window_hours: WINDOW_HOURS,
        projects,
    };

    let summary = format!(
        "Observed {} commits across {} active projects",
        payload.total_commits(),
        payload.project_count()
    );

    emit_result(summary, EventType::CommitsObserved, &project, throttle, &payload)
}

async fn run_git_log(
    name: &str,
    branch: &str,
    working_dir: &Path,
    shell: &dyn ShellGateway,
) -> ProjectCommits {
    // %H<sep>%an<sep>%aI<sep>%s — full SHA, author name, author RFC3339, subject.
    let pretty = format!("--pretty=format:%H{FIELD_SEP}%an{FIELD_SEP}%aI{FIELD_SEP}%s");
    let args = ["log", "--since", WINDOW_SPEC, "--no-merges", &pretty];

    match shell.run(working_dir, "git", &args, None, Some(GIT_LOG_TIMEOUT)).await {
        Ok(result) if result.success => {
            let commits = parse_git_log_output(&result.stdout);
            tracing::info!(
                project = %name,
                commit_count = commits.len(),
                "git log captured"
            );
            ProjectCommits {
                name: name.to_string(),
                branch: branch.to_string(),
                commits,
                error: None,
            }
        }
        Ok(result) => {
            let stderr = result.stderr.lines().next().unwrap_or("git log failed").to_string();
            tracing::warn!(project = %name, stderr = %stderr, "git log non-zero exit");
            ProjectCommits {
                name: name.to_string(),
                branch: branch.to_string(),
                commits: vec![],
                error: Some(format!("git log exited {}: {}", result.exit_code, stderr)),
            }
        }
        Err(e) => {
            tracing::warn!(project = %name, error = %e, "git log invocation failed");
            ProjectCommits {
                name: name.to_string(),
                branch: branch.to_string(),
                commits: vec![],
                error: Some(format!("git log invocation failed: {e}")),
            }
        }
    }
}

/// Parse `git log --pretty=format:%H<US>%an<US>%aI<US>%s` output into a
/// list of [`CommitInfo`]. Skips malformed rows silently — `git log`
/// produces one row per commit, but corrupt repos could produce stray
/// blank lines.
fn parse_git_log_output(stdout: &str) -> Vec<CommitInfo> {
    stdout.lines().filter_map(parse_pretty_line).collect()
}

/// Parse a single `git log --pretty=format` line. Returns `None` for lines
/// that do not match the 4-field shape.
fn parse_pretty_line(line: &str) -> Option<CommitInfo> {
    let fields: Vec<&str> = line.split(FIELD_SEP).collect();
    if fields.len() != 4 {
        return None;
    }
    let sha = fields[0].trim();
    let author = fields[1].trim();
    let timestamp = fields[2].trim();
    let subject = fields[3].trim();
    if sha.is_empty() || timestamp.is_empty() {
        return None;
    }
    Some(CommitInfo {
        sha: sha.to_string(),
        author: author.to_string(),
        timestamp: timestamp.to_string(),
        subject: subject.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;

    use foundry_sdk::registry::{ProjectEntry, Registry};
    use foundry_sdk::throttle::Throttle;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::super::test_helpers;
    use super::*;

    fn registry_with(projects: Vec<(&str, &str, &str)>) -> Arc<RwLock<Registry>> {
        let projects = projects
            .into_iter()
            .map(|(name, branch, path)| ProjectEntry {
                branch: branch.to_string(),
                repo: format!("svetzal/{name}"),
                ..test_helpers::project_entry(name, path)
            })
            .collect();
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects,
        }))
    }

    fn pretty_line(sha: &str, author: &str, ts: &str, subject: &str) -> String {
        format!("{sha}{FIELD_SEP}{author}{FIELD_SEP}{ts}{FIELD_SEP}{subject}")
    }

    fn ok(stdout: impl Into<String>) -> CommandResult {
        CommandResult {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    fn err(stderr: impl Into<String>) -> CommandResult {
        CommandResult {
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code: 128,
            success: false,
        }
    }

    fn trigger() -> Event {
        Event::new(
            EventType::CommitDigestStarted,
            "system".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
    }

    assert_block_meta!(
        ObserveCommits::with_gateways(test_helpers::empty_registry(), FakeShellGateway::success()),
        kind: Observer,
        sinks_on: [CommitDigestStarted],
    );

    // -----------------------------------------------------------------
    // parse_git_log_output
    // -----------------------------------------------------------------

    #[test]
    fn parser_extracts_full_sha_author_timestamp_subject() {
        let line = pretty_line(
            "abcdef0123456789abcdef0123456789abcdef01",
            "Stacey Vetzal",
            "2026-05-28T16:30:00-04:00",
            "feat(slice2): add the commit-digest formation",
        );
        let commits = parse_git_log_output(&line);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].sha, "abcdef0123456789abcdef0123456789abcdef01");
        assert_eq!(commits[0].author, "Stacey Vetzal");
        assert_eq!(commits[0].timestamp, "2026-05-28T16:30:00-04:00");
        assert_eq!(commits[0].subject, "feat(slice2): add the commit-digest formation");
    }

    #[test]
    fn parser_handles_multiple_commits_one_per_line() {
        let stdout = format!(
            "{}\n{}\n",
            pretty_line("aaa", "A", "2026-05-28T16:00:00Z", "first"),
            pretty_line("bbb", "B", "2026-05-28T17:00:00Z", "second"),
        );
        let commits = parse_git_log_output(&stdout);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, "aaa");
        assert_eq!(commits[1].sha, "bbb");
    }

    #[test]
    fn parser_skips_blank_lines() {
        let stdout = format!(
            "\n{}\n\n{}\n",
            pretty_line("aaa", "A", "2026-05-28T16:00:00Z", "first"),
            pretty_line("bbb", "B", "2026-05-28T17:00:00Z", "second"),
        );
        let commits = parse_git_log_output(&stdout);
        assert_eq!(commits.len(), 2);
    }

    #[test]
    fn parser_preserves_subjects_containing_pipes_and_separators() {
        // Subjects can contain anything — confirm the unit separator is the
        // only field boundary we trust.
        let line = pretty_line(
            "abcdef",
            "Author",
            "2026-05-28T16:00:00Z",
            "fix(table): rendering of | column | values",
        );
        let commits = parse_git_log_output(&line);
        assert_eq!(commits[0].subject, "fix(table): rendering of | column | values");
    }

    // -----------------------------------------------------------------
    // execute() — end-to-end with FakeShellGateway
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn empty_registry_emits_observed_with_no_projects() {
        let block = ObserveCommits::with_gateways(
            test_helpers::empty_registry(),
            FakeShellGateway::success(),
        );
        let result = block.execute(&trigger()).await.unwrap();
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::CommitsObserved);
        let payload: CommitsObservedPayload = result.events[0].parse_payload().unwrap();
        assert_eq!(payload.window_hours, 24);
        assert!(payload.projects.is_empty());
        assert_eq!(payload.total_commits(), 0);
    }

    #[tokio::test]
    async fn two_projects_each_with_commits_aggregate_into_one_event() {
        let projects = vec![
            ("alpha", "main", "/tmp/alpha"),
            ("beta", "main", "/tmp/beta"),
        ];
        let shell = FakeShellGateway::sequence(vec![
            ok(pretty_line("a1", "A", "2026-05-28T16:00:00Z", "alpha 1")
                + "\n"
                + &pretty_line("a2", "A", "2026-05-28T17:00:00Z", "alpha 2")),
            ok(pretty_line("b1", "B", "2026-05-28T16:30:00Z", "beta 1")),
        ]);
        let block = ObserveCommits::with_gateways(registry_with(projects), shell);
        let result = block.execute(&trigger()).await.unwrap();
        let payload: CommitsObservedPayload = result.events[0].parse_payload().unwrap();
        assert_eq!(payload.projects.len(), 2);
        assert_eq!(payload.projects[0].name, "alpha");
        assert_eq!(payload.projects[0].commits.len(), 2);
        assert_eq!(payload.projects[1].name, "beta");
        assert_eq!(payload.projects[1].commits.len(), 1);
        assert_eq!(payload.total_commits(), 3);
        assert!(result.summary.contains("3 commits"));
        assert!(result.summary.contains("2 active projects"));
    }

    #[tokio::test]
    async fn project_with_failing_git_log_records_error_and_chain_continues() {
        let projects = vec![("broken", "main", "/tmp/broken"), ("ok", "main", "/tmp/ok")];
        let shell = FakeShellGateway::sequence(vec![
            err("fatal: not a git repository"),
            ok(pretty_line("c1", "C", "2026-05-28T16:00:00Z", "ok 1")),
        ]);
        let block = ObserveCommits::with_gateways(registry_with(projects), shell);
        let result = block.execute(&trigger()).await.unwrap();
        let payload: CommitsObservedPayload = result.events[0].parse_payload().unwrap();
        assert_eq!(payload.projects.len(), 2);
        assert!(
            payload.projects[0].error.is_some(),
            "first project should carry the captured error"
        );
        assert!(payload.projects[0].error.as_deref().unwrap().contains("128"));
        assert!(payload.projects[0].commits.is_empty());
        assert!(payload.projects[1].error.is_none());
        assert_eq!(payload.projects[1].commits.len(), 1);
        assert_eq!(payload.total_commits(), 1);
        assert_eq!(payload.project_count(), 2, "errored project still counted");
    }
}
