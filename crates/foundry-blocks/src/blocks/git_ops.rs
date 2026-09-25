use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    GitSyncFailure, ProjectChangesCommittedPayload, ProjectChangesPushedPayload,
    ProjectCompletedPayload, RemediationCompletedPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, RetryPolicy, TaskBlock, TaskBlockResult};

use foundry_sdk::loop_context::has_loop_context;

use crate::gateway::ShellGateway;

use super::SimulatedSuccess;
use super::checkout_sync::{PrePushSync, commits_ahead, integrate_remote_before_push};

task_block_new! {
    /// Commits what the run left uncommitted, then pushes every commit the
    /// branch has ahead of `origin/<branch>`.
    /// Mutator — simulated success at `dry_run`.
    ///
    /// Real behaviour:
    /// - Commits with `git add -A` + `git commit` when `git status --porcelain`
    ///   is non-empty.
    /// - The push decision is "is the local branch ahead of its remote?", not
    ///   "did this block commit anything?". Agents routinely commit their own
    ///   work, which leaves nothing for this block to commit; those commits
    ///   must still be pushed. For the same reason a trigger reporting
    ///   `"changes": false` is still accepted, so commits stranded by an earlier
    ///   run are pushed on the next one.
    /// - Pushes only when `registry.actions.push` is `true` and the trigger did
    ///   not report `"success": false` (a failed run's commits stay local:
    ///   `push_failure: "run_failed"`).
    /// - Before pushing it fetches `origin/<branch>` and fast-forwards onto it.
    ///   If the remote moved during the run, the local commits are rebased
    ///   with `git rebase origin/<branch>` and the project's required gates are
    ///   re-run on the rebased commits; the push happens only when they pass
    ///   (`push_failure: "gates_failed_after_rebase"` otherwise, including when
    ///   the project has no gates). A conflicting rebase is aborted
    ///   (`push_rejected_diverged`); a failed fetch records
    ///   `remote_unavailable`; a rejected `git push` records `push_failed`.
    ///   Never forces.
    /// - Emits [`EventType::ProjectChangesCommitted`] after a commit (carrying
    ///   any `push_failure`), and [`EventType::ProjectChangesPushed`] after a
    ///   successful push.
    ///
    /// Commit message varies by trigger event type:
    /// - [`EventType::ProjectIterationCompleted`] → `chore(<project>): automated iterate`
    /// - [`EventType::ProjectMaintenanceCompleted`] → `chore(<project>): automated maintenance`
    /// - All other triggers → `chore(<project>): automated remediation`
    ///
    /// When the project is not found in the registry, the block returns a failure result.
    pub struct CommitAndPush {
        shell: ShellGateway = crate::gateway::ProcessShellGateway
    }
}

/// Outcome of a commit-and-push dry-run simulation.
pub(crate) struct CommitDryRunOutcome {
    cve: String,
    push_enabled: bool,
    /// False when the trigger reported `"changes": false`: the block still runs
    /// (to push stranded commits) but a dry run cannot know whether any exist,
    /// so it simulates no events.
    commit_expected: bool,
    run_failed: bool,
}

impl CommitAndPush {
    /// Look up whether push is enabled for a project in the registry.
    ///
    /// Returns `true` when the project is not found (unknown → optimistic default) or
    /// when `entry.actions.push` is `true`.  Returns `true` on lock-poison as well
    /// (defensive default keeps the dry-run chain intact).
    ///
    /// This is an **Absorb** per AGENTS.md's Failure Policy: a poisoned
    /// registry lock is discarded in favor of an optimistic default, but only
    /// with the `// Best-effort:` marker below and the `tracing::error!` that
    /// carries the fault — never a bare discard.
    fn push_enabled_for(&self, project: &str) -> bool {
        match super::read_registry(&self.registry) {
            Ok(guard) => guard.find_project(project).is_none_or(|e| e.actions.push),
            // Best-effort: a poisoned registry lock must not block a dry-run
            // simulation from completing; default to push-enabled and log
            // the fault so it is investigable.
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "registry lock poisoned in push_enabled_for; defaulting push_enabled=true"
                );
                true
            }
        }
    }

    async fn commit_and_push(
        registry: Arc<RwLock<Registry>>,
        shell: Arc<dyn ShellGateway>,
        project: String,
        throttle: foundry_sdk::throttle::Throttle,
        event_type: EventType,
        proceed: Proceed,
    ) -> anyhow::Result<TaskBlockResult> {
        let Proceed {
            cve, run_failed, ..
        } = proceed;
        // Resolve the project path and push flag from the registry.
        // Extract synchronously before any .await point so the lock is not held across yields.
        let entry_data = super::read_registry(&registry)?
            .find_project(&project)
            .map(|e| (e.path.clone(), e.branch.clone(), e.actions.push));

        let Some((path_str, branch, push_enabled)) = entry_data else {
            tracing::warn!(project = %project, "project not found in registry");
            return Ok(TaskBlockResult::project_not_found(&project));
        };

        let path = std::path::Path::new(&path_str);

        tracing::info!(%project, "checking for changes to commit");
        let status = shell.run(path, "git", &["status", "--porcelain"], None, None).await?;
        let commit_msg = if status.stdout.trim().is_empty() {
            tracing::info!(%project, "working tree clean, nothing to commit");
            None
        } else {
            commit_changes(&*shell, path, &project, &event_type).await?
        };

        let push = if !push_enabled {
            tracing::info!(%project, "push disabled in registry, skipping");
            PushOutcome::Disabled
        } else if run_failed {
            keep_failed_run_local(&*shell, path, &branch).await?
        } else {
            push_if_ahead(&*shell, path, &project, &branch, &cve).await?
        };

        if let PushOutcome::Refused { failure, detail } = &push {
            tracing::warn!(%project, %failure, %detail, "push refused; commits left on local branch");
        }

        let push_failure = match &push {
            PushOutcome::Refused { failure, .. } => Some(*failure),
            _ => None,
        };
        let push_payload = match &push {
            PushOutcome::Pushed { commits } => Some(ProjectChangesPushedPayload {
                project: project.clone(),
                cve: cve.clone(),
                message: Some(format!("pushed {commits} commit(s) to origin/{branch}")),
                dry_run: None,
            }),
            _ => None,
        };

        let mut events = Vec::new();
        if let Some(message) = &commit_msg {
            events.push(super::event_from_payload(
                EventType::ProjectChangesCommitted,
                &project,
                throttle,
                &ProjectChangesCommittedPayload {
                    project: project.clone(),
                    cve: cve.clone(),
                    message: message.clone(),
                    dry_run: None,
                    push_failure,
                },
            )?);
        }
        if let Some(payload) = &push_payload {
            events.push(super::event_from_payload(
                EventType::ProjectChangesPushed,
                &project,
                throttle,
                payload,
            )?);
        }

        // Record, not fail: a refusal travels in the summary (and as a typed
        // `push_failure` when this block committed). Returning a failed result
        // would be retried by the engine, and a retry after a rebase would push
        // without re-running the gates.
        Ok(TaskBlockResult::success(
            push_summary(commit_msg.is_some(), &push, &branch),
            events,
        ))
    }
}

/// What the push step did.
#[derive(Debug, PartialEq)]
enum PushOutcome {
    /// Push disabled for this project in the registry.
    Disabled,
    /// The branch has no commits the remote lacks.
    NothingToPush,
    /// This many commits were pushed.
    Pushed { commits: u32 },
    /// Commits exist that were not pushed, and why.
    Refused {
        failure: GitSyncFailure,
        detail: String,
    },
}

/// One-line, human-readable result of the block.
fn push_summary(committed: bool, push: &PushOutcome, branch: &str) -> String {
    let commit = if committed {
        "Committed changes"
    } else {
        "No changes to commit"
    };
    match push {
        PushOutcome::Disabled => format!("{commit}; push disabled"),
        PushOutcome::NothingToPush => format!("{commit}; nothing ahead of origin/{branch}"),
        PushOutcome::Pushed { commits } => {
            format!("{commit}; pushed {commits} commit(s) to origin/{branch}")
        }
        PushOutcome::Refused { failure, detail } => {
            format!("{commit}; push refused ({failure}): {detail} — commits left on local branch")
        }
    }
}

/// The run failed: never push. Report how many commits are being kept local
/// (against the last-fetched remote ref, without touching the network).
async fn keep_failed_run_local(
    shell: &dyn ShellGateway,
    path: &std::path::Path,
    branch: &str,
) -> anyhow::Result<PushOutcome> {
    Ok(match commits_ahead(shell, path, branch).await? {
        Some(0) => PushOutcome::NothingToPush,
        Some(n) => PushOutcome::Refused {
            failure: GitSyncFailure::RunFailed,
            detail: format!(
                "run reported failure; {n} commit(s) ahead of origin/{branch} kept local"
            ),
        },
        None => PushOutcome::Refused {
            failure: GitSyncFailure::RunFailed,
            detail: format!("run reported failure; could not resolve origin/{branch}"),
        },
    })
}

/// Integrate remote movement, then push every commit the branch has ahead of
/// `origin/<branch>`. Rebased commits are re-verified by the required gates
/// before they are pushed. Never forces.
async fn push_if_ahead(
    shell: &dyn ShellGateway,
    path: &std::path::Path,
    project: &str,
    branch: &str,
    cve: &str,
) -> anyhow::Result<PushOutcome> {
    let rebased = match integrate_remote_before_push(shell, path, project, branch).await? {
        PrePushSync::Refused { failure, detail } => {
            return Ok(PushOutcome::Refused { failure, detail });
        }
        PrePushSync::Ready { rebased } => rebased,
    };

    let commits = match commits_ahead(shell, path, branch).await? {
        Some(0) => return Ok(PushOutcome::NothingToPush),
        Some(n) => n,
        None => {
            return Ok(PushOutcome::Refused {
                failure: GitSyncFailure::RemoteUnavailable,
                detail: format!("could not resolve origin/{branch}"),
            });
        }
    };

    if rebased && let Err(detail) = verify_rebased_commits(shell, path).await {
        return Ok(PushOutcome::Refused {
            failure: GitSyncFailure::GatesFailedAfterRebase,
            detail,
        });
    }

    tracing::info!(%project, commits, %cve, "pushing commits ahead of origin");
    let push = shell.run(path, "git", &["push", "origin", branch], None, None).await?;
    if push.success {
        Ok(PushOutcome::Pushed { commits })
    } else {
        Ok(PushOutcome::Refused {
            failure: GitSyncFailure::PushFailed,
            detail: format!("git push origin {branch} failed: {}", push.stderr.trim()),
        })
    }
}

/// Re-run the project's required gates on commits that were just rebased.
/// A project with no gates cannot verify them, so that is a refusal too.
async fn verify_rebased_commits(
    shell: &dyn ShellGateway,
    path: &std::path::Path,
) -> Result<(), String> {
    let gates = crate::gate_file::read_gates(path)
        .map_err(|e| format!("could not read gates to verify rebased commits: {e}"))?;
    if gates.is_empty() {
        return Err("no gates to verify the rebased commits".to_string());
    }
    let run = crate::gate_runner::run_gates(&gates, path, shell)
        .await
        .map_err(|e| format!("gate run errored on rebased commits: {e}"))?;
    if run.required_passed {
        return Ok(());
    }
    let failed: Vec<&str> = run
        .results
        .iter()
        .filter(|r| r.required && !r.passed)
        .map(|r| r.name.as_str())
        .collect();
    Err(format!("required gates failed on rebased commits: {}", failed.join(", ")))
}

/// Decision outcome for a commit-and-push trigger.
///
/// Centralises the guard logic shared between `dry_run_events` and `execute`,
/// eliminating drift risk between the two paths.
#[derive(Debug, PartialEq)]
enum CommitDecision {
    /// Intermediate completion inside a nested loop — skip.
    SkipNestedLoop,
    /// Commit anything left uncommitted, then push whatever is ahead.
    Proceed(Proceed),
}

/// What the trigger tells the commit-and-push step.
#[derive(Debug, PartialEq)]
struct Proceed {
    /// CVE identifier from the payload, or `"unknown"`.
    cve: String,
    /// False when the payload says `"changes": false`.
    commit_expected: bool,
    /// True when the payload says `"success": false`.
    run_failed: bool,
}

/// Evaluate the trigger and decide what `CommitAndPush` should do.
///
/// Both `dry_run_events` and `execute` delegate to this function so the
/// loop-context guard, the no-changes self-filter, and CVE extraction are
/// always derived from a single source of truth.
fn decide_commit(trigger: &Event) -> CommitDecision {
    // 1. loop_context guard: completion events inside a nested loop are skipped.
    let is_completion_event = matches!(
        trigger.event_type,
        EventType::ProjectIterationCompleted | EventType::ProjectMaintenanceCompleted
    );
    if is_completion_event && has_loop_context(&trigger.payload) {
        return CommitDecision::SkipNestedLoop;
    }

    // 2. self-filter: payload explicitly says no changes.
    let changes_flag = match trigger.parse_payload::<ProjectCompletedPayload>() {
        Ok(p) => p.changes,
        Err(e) => {
            // Best-effort: this block sinks on multiple event types, so a trigger
            // that doesn't carry a ProjectCompletedPayload shape is routine (not every
            // sinked event type has a `changes` field); treat as "flag absent" same as
            // the previous .ok() behaviour, but log so unexpected drift is visible.
            tracing::debug!(error = %e, "trigger payload did not match ProjectCompletedPayload");
            None
        }
    };
    // A run that reports no changes is still accepted: commits stranded by an
    // earlier run must be pushed. It only tells the dry run not to expect a commit.
    let commit_expected = changes_flag != Some(false);
    let run_failed =
        trigger.payload.get("success").and_then(serde_json::Value::as_bool) == Some(false);

    // 3. extract CVE (defaults to "unknown" when absent).
    let cve = match trigger.parse_payload::<RemediationCompletedPayload>() {
        Ok(p) => p.cve,
        Err(e) => {
            // Best-effort: most triggers (ProjectIterationCompleted, ProjectMaintenanceCompleted)
            // don't carry a RemediationCompletedPayload shape at all, so a parse failure here
            // is routine; fall back to "unknown" same as the previous .ok() behaviour, logged
            // for investigability.
            tracing::debug!(error = %e, "trigger payload did not match RemediationCompletedPayload");
            None
        }
    }
    .unwrap_or_else(|| "unknown".to_string());

    CommitDecision::Proceed(Proceed {
        cve,
        commit_expected,
        run_failed,
    })
}

impl SimulatedSuccess for CommitAndPush {
    type Outcome = Option<CommitDryRunOutcome>;

    fn simulate(&self, trigger: &Event) -> Option<CommitDryRunOutcome> {
        match decide_commit(trigger) {
            CommitDecision::SkipNestedLoop => None,
            CommitDecision::Proceed(Proceed {
                cve,
                commit_expected,
                run_failed,
            }) => {
                let push_enabled = self.push_enabled_for(&trigger.project);
                Some(CommitDryRunOutcome {
                    cve,
                    push_enabled,
                    commit_expected,
                    run_failed,
                })
            }
        }
    }

    fn success_events(&self, trigger: &Event, outcome: &Option<CommitDryRunOutcome>) -> Vec<Event> {
        let Some(ref data) = *outcome else {
            return vec![];
        };
        if !data.commit_expected {
            return vec![];
        }
        let push_payload =
            (data.push_enabled && !data.run_failed).then(|| ProjectChangesPushedPayload {
                project: trigger.project.clone(),
                cve: data.cve.clone(),
                message: None,
                dry_run: Some(true),
            });
        #[allow(
            clippy::expect_used,
            reason = "commit and push event payloads are infallibly serializable (Payload Conventions, AGENTS.md)"
        )]
        build_commit_push_events(
            &trigger.project,
            trigger.throttle,
            &ProjectChangesCommittedPayload {
                project: trigger.project.clone(),
                cve: data.cve.clone(),
                message: commit_message(&trigger.event_type, &trigger.project),
                dry_run: Some(true),
                push_failure: None,
            },
            push_payload.as_ref(),
        )
        .expect("commit and push event payloads are infallibly serializable")
    }
}

impl TaskBlock for CommitAndPush {
    task_block_meta! {
        name: "Commit and Push",
        kind: Mutator,
        sinks_on: [RemediationCompleted, ProjectIterationCompleted, ProjectMaintenanceCompleted, InnerIterationCompleted],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        matches!(decide_commit(trigger), CommitDecision::Proceed(_))
    }

    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy {
            max_retries: 2,
            backoff: Duration::from_secs(5),
        }
    }

    dry_run_via_simulation!();

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let CommitDecision::Proceed(proceed) = decide_commit(trigger) else {
            // Defensive: accepts() filters SkipNestedLoop before dispatch.
            return skip!("Skipped: no commit needed");
        };
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let event_type = trigger.event_type.clone();
        let registry = Arc::clone(&self.registry);
        let shell = Arc::clone(&self.shell);
        Box::pin(Self::commit_and_push(registry, shell, project, throttle, event_type, proceed))
    }
}

#[derive(Debug, PartialEq)]
enum CommitOutcome {
    Committed,
    NothingToCommit,
    Failed,
}

/// Classify the outcome of a `git commit` invocation from the exit status and
/// combined stdout+stderr output.
fn classify_commit_outcome(success: bool, stdout: &str, stderr: &str) -> CommitOutcome {
    if success {
        CommitOutcome::Committed
    } else {
        let combined = format!("{stdout} {stderr}").to_lowercase();
        if combined.contains("nothing to commit") || combined.contains("no changes added") {
            CommitOutcome::NothingToCommit
        } else {
            CommitOutcome::Failed
        }
    }
}

/// Stage all changes and commit; returns the commit message on success, `None` when
/// git reports nothing to commit (a successful no-op), and `Err` on real failures.
async fn commit_changes(
    shell: &dyn ShellGateway,
    path: &std::path::Path,
    project: &str,
    event_type: &EventType,
) -> anyhow::Result<Option<String>> {
    shell.run(path, "git", &["add", "-A"], None, None).await?;

    let commit_msg = commit_message(event_type, project);
    let commit = shell.run(path, "git", &["commit", "-m", &commit_msg], None, None).await?;

    match classify_commit_outcome(commit.success, &commit.stdout, &commit.stderr) {
        CommitOutcome::Committed => {
            tracing::info!(%project, "committed changes");
            Ok(Some(commit_msg))
        }
        CommitOutcome::NothingToCommit => {
            // Can happen when both iterate and maintain trigger CommitAndPush and the
            // first one already committed everything, or when git add -A stages content
            // identical to HEAD.
            tracing::info!(%project, "git commit found nothing to commit");
            Ok(None)
        }
        CommitOutcome::Failed => {
            Err(anyhow::anyhow!("git commit failed: {}", commit.stderr.trim()))
        }
    }
}

/// Build a `Vec<Event>` for a commit (and optional push) from typed payloads.
///
/// Eliminates the repeated serialize-and-construct pattern in the real execution
/// path, `dry_run_events`, and the stub fallback.
fn build_commit_push_events(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    commit_payload: &ProjectChangesCommittedPayload,
    push_payload: Option<&ProjectChangesPushedPayload>,
) -> anyhow::Result<Vec<Event>> {
    let mut events = vec![super::event_from_payload(
        EventType::ProjectChangesCommitted,
        project,
        throttle,
        commit_payload,
    )?];
    if let Some(push) = push_payload {
        events.push(super::event_from_payload(
            EventType::ProjectChangesPushed,
            project,
            throttle,
            push,
        )?);
    }
    Ok(events)
}

/// Map a trigger event type to the appropriate `chore(project): ...` commit message.
fn commit_message(event_type: &EventType, project: &str) -> String {
    match event_type {
        EventType::ProjectIterationCompleted => format!("chore({project}): automated iterate"),
        EventType::ProjectMaintenanceCompleted => {
            format!("chore({project}): automated maintenance")
        }
        EventType::InnerIterationCompleted => format!("chore({project}): strategic iterate cycle"),
        _ => format!("chore({project}): automated remediation"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry};
    use tempfile::TempDir;

    use foundry_sdk::task_block::TaskBlock;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::super::test_helpers;
    use super::{
        CommitAndPush, CommitDecision, CommitOutcome, Proceed, classify_commit_outcome,
        commit_message, decide_commit,
    };

    fn make_trigger(project: &str, cve: &str) -> Event {
        test_helpers::make_trigger(
            EventType::RemediationCompleted,
            project,
            serde_json::json!({ "cve": cve }),
        )
    }

    fn make_trigger_for(event_type: EventType, project: &str) -> Event {
        test_helpers::make_trigger(event_type, project, serde_json::json!({}))
    }

    fn make_trigger_no_changes(event_type: EventType, project: &str) -> Event {
        test_helpers::make_trigger(event_type, project, serde_json::json!({ "changes": false }))
    }

    fn registry_for(name: &str, path: &str, push: bool) -> Arc<RwLock<Registry>> {
        test_helpers::registry_with_entry(ProjectEntry {
            agent: String::new(),
            actions: ActionFlags {
                push,
                ..Default::default()
            },
            ..test_helpers::project_entry(name, path)
        })
    }

    /// Fake sequence that simulates: status=dirty, add=ok, commit=ok, push=ok.
    fn dirty_sequence() -> Arc<FakeShellGateway> {
        FakeShellGateway::sequence(vec![
            // git status --porcelain: non-empty output = dirty
            CommandResult {
                stdout: " M file.txt\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // git add -A
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // git commit
            CommandResult {
                stdout: "[main abc1234] committed\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // git fetch origin main
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // git merge --ff-only origin/main (remote unmoved)
            CommandResult {
                stdout: "Already up to date.\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // git rev-list --left-right --count HEAD...origin/main (1 ahead)
            CommandResult {
                stdout: "1\t0\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // git push origin main
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ])
    }

    /// Fake sequence: clean tree, remote unmoved, nothing ahead.
    fn clean_sequence() -> Arc<FakeShellGateway> {
        FakeShellGateway::sequence(vec![
            ok(""),                    // status: clean
            ok(""),                    // fetch origin main
            ok("Already up to date."), // merge --ff-only
            ok("0\t0\n"),              // rev-list: level with origin
        ])
    }

    // -- classify_commit_outcome pure function tests --

    #[test]
    fn classify_commit_outcome_success_is_committed() {
        assert_eq!(classify_commit_outcome(true, "[main abc] msg\n", ""), CommitOutcome::Committed);
    }

    #[test]
    fn classify_commit_outcome_nothing_to_commit() {
        assert_eq!(
            classify_commit_outcome(
                false,
                "On branch main\nnothing to commit, working tree clean\n",
                ""
            ),
            CommitOutcome::NothingToCommit
        );
        assert_eq!(
            classify_commit_outcome(false, "", "no changes added to commit"),
            CommitOutcome::NothingToCommit
        );
    }

    #[test]
    fn classify_commit_outcome_failure() {
        assert_eq!(
            classify_commit_outcome(false, "", "error: failed to push some refs"),
            CommitOutcome::Failed
        );
    }

    // -- commit_message pure function tests --

    #[test]
    fn commit_message_iterate() {
        assert_eq!(
            commit_message(&EventType::ProjectIterationCompleted, "my-project"),
            "chore(my-project): automated iterate"
        );
    }

    #[test]
    fn commit_message_maintenance() {
        assert_eq!(
            commit_message(&EventType::ProjectMaintenanceCompleted, "my-project"),
            "chore(my-project): automated maintenance"
        );
    }

    #[test]
    fn commit_message_strategic() {
        assert_eq!(
            commit_message(&EventType::InnerIterationCompleted, "my-project"),
            "chore(my-project): strategic iterate cycle"
        );
    }

    #[test]
    fn commit_message_remediation_default() {
        assert_eq!(
            commit_message(&EventType::RemediationCompleted, "my-project"),
            "chore(my-project): automated remediation"
        );
    }

    #[tokio::test]
    async fn unknown_project_returns_failure() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = make_trigger("no-such-project", "CVE-2026-0001");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn clean_tree_emits_no_events() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = clean_sequence();
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = make_trigger("my-project", "CVE-2026-0002");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert_eq!(result.summary, "No changes to commit; nothing ahead of origin/main");
    }

    #[tokio::test]
    async fn dirty_tree_commits_and_pushes_when_enabled() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = dirty_sequence();
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = make_trigger("my-project", "CVE-2026-0003");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["project_changes_committed", "project_changes_pushed"]);
    }

    fn ok(stdout: &str) -> CommandResult {
        CommandResult {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    fn fail(stderr: &str) -> CommandResult {
        CommandResult {
            stdout: String::new(),
            stderr: stderr.to_string(),
            exit_code: 1,
            success: false,
        }
    }

    fn git_calls(shell: &FakeShellGateway) -> Vec<String> {
        shell.invocations().into_iter().map(|i| i.args.join(" ")).collect()
    }

    /// A project dir with one required gate, so rebased commits can be verified.
    fn dir_with_gate() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"test","command":"cargo test","required":true}]}"#,
        )
        .unwrap();
        dir
    }

    #[tokio::test]
    async fn remote_moved_and_clean_rebase_pushes() {
        let dir = dir_with_gate();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = FakeShellGateway::sequence(vec![
            ok(" M file.txt\n"),                                     // status
            ok(""),                                                  // add -A
            ok("[main abc1234] committed\n"),                        // commit
            ok(""),                                                  // fetch origin main
            fail("fatal: Not possible to fast-forward, aborting."),  // merge --ff-only
            ok("Successfully rebased and updated refs/heads/main."), // rebase
            ok("1\t0\n"),                                            // rev-list
            ok("test result: ok"),                                   // gate: cargo test
            ok(""),                                                  // push
        ]);
        let block = CommitAndPush::with_gateways(registry, Arc::clone(&shell) as _);
        let trigger = make_trigger_for(EventType::ProjectMaintenanceCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["project_changes_committed", "project_changes_pushed"]);
        assert!(result.events[0].payload.get("push_failure").is_none());
        assert_eq!(
            git_calls(&shell)[3..],
            [
                "fetch origin main",
                "merge --ff-only origin/main",
                "rebase origin/main",
                "rev-list --left-right --count HEAD...origin/main",
                "-c cargo test",
                "push origin main"
            ],
            "the gates re-run on the rebased commits before the push"
        );
    }

    #[tokio::test]
    async fn remote_moved_and_rebase_conflicts_records_push_rejected_diverged() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = FakeShellGateway::sequence(vec![
            ok(" M file.txt\n"),
            ok(""),
            ok("[main abc1234] committed\n"),
            ok(""),
            fail("fatal: Not possible to fast-forward, aborting."),
            fail("CONFLICT (content): Merge conflict in file.txt"),
            ok(""), // rebase --abort
        ]);
        let block = CommitAndPush::with_gateways(registry, Arc::clone(&shell) as _);
        let trigger = make_trigger_for(EventType::ProjectMaintenanceCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["project_changes_committed"], "no push event on refusal");
        assert_eq!(result.events[0].payload["push_failure"], "push_rejected_diverged");
        assert!(result.summary.contains("push_rejected_diverged"), "{}", result.summary);
        let calls = git_calls(&shell);
        assert_eq!(calls.last().map(String::as_str), Some("rebase --abort"));
        assert!(!calls.iter().any(|c| c.starts_with("push")), "must not push: {calls:?}");
        assert!(!calls.iter().any(|c| c.contains("--force")), "must never force: {calls:?}");
    }

    #[tokio::test]
    async fn failed_pre_push_fetch_records_remote_unavailable() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = FakeShellGateway::sequence(vec![
            ok(" M file.txt\n"),
            ok(""),
            ok("[main abc1234] committed\n"),
            fail("fatal: could not read from remote repository"),
        ]);
        let block = CommitAndPush::with_gateways(registry, Arc::clone(&shell) as _);
        let trigger = make_trigger("my-project", "CVE-2026-0005");

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].payload["push_failure"], "remote_unavailable");
        assert!(!git_calls(&shell).iter().any(|c| c.starts_with("push")));
    }

    #[tokio::test]
    async fn dirty_tree_commits_but_skips_push_when_disabled() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), false);
        // Only three calls needed: status, add, commit (no push).
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: " M file.txt\n".to_string(),
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
            CommandResult {
                stdout: "[main abc1234] committed\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = make_trigger("my-project", "CVE-2026-0004");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["project_changes_committed"]);
    }

    // -- push is based on "ahead of origin", not on "this block committed" --

    #[tokio::test]
    async fn agent_committed_work_is_pushed_even_though_nothing_is_left_to_commit() {
        // The bug: the maintain agent commits its own changes, the tree is
        // clean, and the commits were never pushed.
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = FakeShellGateway::sequence(vec![
            ok(""),                    // status: clean — the agent already committed
            ok(""),                    // fetch origin main
            ok("Already up to date."), // merge --ff-only
            ok("2\t0\n"),              // rev-list: 2 ahead
            ok(""),                    // push origin main
        ]);
        let block = CommitAndPush::with_gateways(registry, Arc::clone(&shell) as _);
        let trigger = make_trigger_for(EventType::ProjectMaintenanceCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["project_changes_pushed"], "no commit of ours, but a push");
        assert_eq!(result.summary, "No changes to commit; pushed 2 commit(s) to origin/main");
        assert_eq!(git_calls(&shell).last().map(String::as_str), Some("push origin main"));
    }

    #[tokio::test]
    async fn stranded_commits_are_pushed_on_a_run_that_reports_no_changes() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = FakeShellGateway::sequence(vec![
            ok(""),
            ok(""),
            ok("Already up to date."),
            ok("4\t0\n"),
            ok(""),
        ]);
        let block = CommitAndPush::with_gateways(registry, Arc::clone(&shell) as _);
        let trigger = make_trigger_no_changes(EventType::ProjectMaintenanceCompleted, "my-project");
        assert!(block.accepts(&trigger));

        let result = block.execute(&trigger).await.unwrap();

        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["project_changes_pushed"]);
        assert!(result.summary.contains("pushed 4 commit(s)"), "{}", result.summary);
    }

    #[tokio::test]
    async fn rebased_commits_that_fail_the_gates_are_not_pushed() {
        let dir = dir_with_gate();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = FakeShellGateway::sequence(vec![
            ok(""),                                                 // status: clean
            ok(""),                                                 // fetch
            fail("fatal: Not possible to fast-forward, aborting."), // merge --ff-only
            ok("Successfully rebased"),                             // rebase
            ok("1\t0\n"),                                           // rev-list
            fail("test failed"),                                    // gate: cargo test
        ]);
        let block = CommitAndPush::with_gateways(registry, Arc::clone(&shell) as _);
        let trigger = make_trigger_for(EventType::ProjectMaintenanceCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.events.is_empty(), "no push event");
        assert!(result.summary.contains("gates_failed_after_rebase"), "{}", result.summary);
        assert!(result.summary.contains("test"), "names the failed gate: {}", result.summary);
        let calls = git_calls(&shell);
        assert!(!calls.iter().any(|c| c.starts_with("push")), "must not push: {calls:?}");
        assert!(!calls.iter().any(|c| c.contains("--force")), "must never force: {calls:?}");
    }

    #[tokio::test]
    async fn rebased_commits_without_gates_are_not_pushed() {
        let dir = TempDir::new().unwrap(); // no .hone-gates.json
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = FakeShellGateway::sequence(vec![
            ok(" M file.txt\n"),
            ok(""),
            ok("[main abc1234] committed\n"),
            ok(""),
            fail("fatal: Not possible to fast-forward, aborting."),
            ok("Successfully rebased"),
            ok("1\t0\n"),
        ]);
        let block = CommitAndPush::with_gateways(registry, Arc::clone(&shell) as _);
        let trigger = make_trigger_for(EventType::ProjectMaintenanceCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].payload["push_failure"], "gates_failed_after_rebase");
        assert!(result.summary.contains("no gates"), "{}", result.summary);
        assert!(!git_calls(&shell).iter().any(|c| c.starts_with("push")));
    }

    #[tokio::test]
    async fn rejected_push_is_recorded_and_never_forced() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = FakeShellGateway::sequence(vec![
            ok(" M file.txt\n"),
            ok(""),
            ok("[main abc1234] committed\n"),
            ok(""),
            ok("Already up to date."),
            ok("1\t0\n"),
            fail(" ! [rejected]        main -> main (non-fast-forward)"),
        ]);
        let block = CommitAndPush::with_gateways(registry, Arc::clone(&shell) as _);
        let trigger = make_trigger_for(EventType::ProjectMaintenanceCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success, "recorded, not failed (a retry must not skip the gates)");
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["project_changes_committed"], "no push event");
        assert_eq!(result.events[0].payload["push_failure"], "push_failed");
        assert!(result.summary.contains("non-fast-forward"), "{}", result.summary);
        let calls = git_calls(&shell);
        assert_eq!(calls.iter().filter(|c| c.starts_with("push")).count(), 1, "{calls:?}");
        assert!(!calls.iter().any(|c| c.contains("--force")), "must never force: {calls:?}");
    }

    #[tokio::test]
    async fn failed_run_keeps_its_commits_local() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = FakeShellGateway::sequence(vec![
            ok(""),       // status: clean (the agent committed)
            ok("3\t0\n"), // rev-list against the existing origin ref
        ]);
        let block = CommitAndPush::with_gateways(registry, Arc::clone(&shell) as _);
        let trigger = test_event!(EventType::ProjectMaintenanceCompleted, "my-project", {
            "project": "my-project",
            "success": false,
            "summary": "gates failed after 3 retries"
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.events.is_empty());
        assert!(result.summary.contains("run_failed"), "{}", result.summary);
        assert!(result.summary.contains("3 commit(s)"), "{}", result.summary);
        let calls = git_calls(&shell);
        assert!(
            !calls.iter().any(|c| c.starts_with("push") || c.starts_with("fetch")),
            "a failed run touches neither the remote nor the push: {calls:?}"
        );
    }

    #[tokio::test]
    async fn real_git_pushes_commits_an_agent_already_made() {
        // End to end against real git: a bare remote, a clone, an agent-style
        // commit, and a clean tree when the block runs.
        let tmp = TempDir::new().unwrap();
        let remote = tmp.path().join("remote.git");
        let work = tmp.path().join("work");
        let run = |dir: &std::path::Path, args: &[&str]| {
            let out =
                std::process::Command::new("git").current_dir(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(tmp.path(), &["init", "--bare", "-b", "main", remote.to_str().unwrap()]);
        run(tmp.path(), &["clone", remote.to_str().unwrap(), work.to_str().unwrap()]);
        run(&work, &["config", "user.email", "test@example.com"]);
        run(&work, &["config", "user.name", "Test"]);
        run(&work, &["checkout", "-b", "main"]);
        std::fs::write(work.join("README.md"), "init").unwrap();
        run(&work, &["add", "-A"]);
        run(&work, &["commit", "-m", "init"]);
        run(&work, &["push", "-u", "origin", "main"]);
        // The agent's own commit:
        std::fs::write(work.join("Cargo.lock"), "bumped").unwrap();
        run(&work, &["add", "-A"]);
        run(&work, &["commit", "-m", "Update thiserror"]);

        let registry = registry_for("my-project", work.to_str().unwrap(), true);
        let block = CommitAndPush::new(registry);
        let trigger = make_trigger_for(EventType::ProjectMaintenanceCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.summary.contains("pushed 1 commit(s)"), "{}", result.summary);
        assert_eq!(
            run(&work, &["rev-parse", "HEAD"]),
            run(&remote, &["rev-parse", "main"]),
            "the agent's commit reached the remote"
        );
    }

    #[test]
    fn sinks_on_includes_all_event_types() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let sinks = block.sinks_on();
        assert!(sinks.contains(&EventType::RemediationCompleted));
        assert!(sinks.contains(&EventType::ProjectIterationCompleted));
        assert!(sinks.contains(&EventType::ProjectMaintenanceCompleted));
        assert!(sinks.contains(&EventType::InnerIterationCompleted));
    }

    #[test]
    fn accepts_returns_false_when_nested_loop() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = test_event!(EventType::ProjectIterationCompleted, "proj", {
            "project": "proj",
            "success": true,
            "loop_context": { "strategic": { "iteration": 1 } }
        });
        assert!(!block.accepts(&trigger));
    }

    #[test]
    fn accepts_returns_true_when_no_changes() {
        // The block must still run: commits stranded by an earlier run get pushed.
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = make_trigger_no_changes(EventType::ProjectIterationCompleted, "proj");
        assert!(block.accepts(&trigger));
    }

    #[test]
    fn accepts_returns_true_when_remediation_in_loop_context() {
        // RemediationCompleted is not a completion event — loop_context does not trigger the
        // nested-loop guard, so accepts() must return true.
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = test_event!(EventType::RemediationCompleted, "proj", {
            "cve": "CVE-2026-0001",
            "loop_context": { "strategic": { "iteration": 1 } }
        });
        assert!(block.accepts(&trigger));
    }

    #[tokio::test]
    async fn inner_iteration_completed_commits_with_strategic_message() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), false);
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: " M f\n".to_string(),
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
            CommandResult {
                stdout: "[main x] msg\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = test_event!(EventType::InnerIterationCompleted, "my-project", {
            "project": "my-project",
            "success": true,
            "loop_context": { "strategic": { "iteration": 1 } }
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let msg = result.events[0].payload["message"].as_str().unwrap();
        assert!(msg.contains("strategic"), "expected 'strategic' in '{msg}'");
    }

    #[tokio::test]
    async fn does_not_skip_remediation_with_loop_context() {
        // RemediationCompleted is not a completion event, so loop_context should not trigger skip.
        // However, with no registry entry the block returns an honest failure.
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = test_event!(EventType::RemediationCompleted, "my-project", {
            "cve": "CVE-2026-0001",
            "loop_context": { "strategic": { "iteration": 1 } }
        });

        let result = block.execute(&trigger).await.unwrap();

        // Should NOT skip (no nested-loop guard) but returns failure: project not in registry.
        assert!(!result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn remediation_trigger_uses_remediation_commit_message() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), false);
        // status=dirty, add, commit (no push)
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: " M f\n".to_string(),
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
            CommandResult {
                stdout: "[main x] msg\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = make_trigger("my-project", "CVE-2026-1000");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(!result.events.is_empty());
        let msg = result.events[0].payload["message"].as_str().unwrap();
        assert!(msg.contains("remediation"), "expected 'remediation' in '{msg}'");
    }

    #[tokio::test]
    async fn iterate_trigger_uses_iterate_commit_message() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), false);
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: " M f\n".to_string(),
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
            CommandResult {
                stdout: "[main x] msg\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = make_trigger_for(EventType::ProjectIterationCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let msg = result.events[0].payload["message"].as_str().unwrap();
        assert!(msg.contains("iterate"), "expected 'iterate' in '{msg}'");
    }

    #[tokio::test]
    async fn maintain_trigger_uses_maintenance_commit_message() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), false);
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: " M f\n".to_string(),
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
            CommandResult {
                stdout: "[main x] msg\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = make_trigger_for(EventType::ProjectMaintenanceCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let msg = result.events[0].payload["message"].as_str().unwrap();
        assert!(msg.contains("maintenance"), "expected 'maintenance' in '{msg}'");
    }

    #[tokio::test]
    async fn commit_nothing_to_commit_is_success_not_error() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), false);
        // status=dirty (something shows up), add=ok, commit fails with "nothing to commit"
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: " m .claude/worktrees/agent-abc123\n".to_string(),
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
            CommandResult {
                stdout: "On branch main\nnothing to commit, working tree clean\n".to_string(),
                stderr: String::new(),
                exit_code: 1,
                success: false,
            },
        ]);
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = make_trigger_for(EventType::ProjectIterationCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success, "should be success, not error");
        assert!(result.events.is_empty());
        assert_eq!(result.summary, "No changes to commit; push disabled");
    }

    #[test]
    fn retry_policy_allows_retries() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let policy = block.retry_policy();
        assert_eq!(policy.max_retries, 2);
        assert_eq!(policy.backoff, Duration::from_secs(5));
    }

    // -- Real git repo tests for commit message variants --

    async fn init_git_repo_real(dir: &std::path::Path) {
        crate::shell::run(dir, "git", &["init"], None, None).await.unwrap();
        crate::shell::run(dir, "git", &["config", "user.email", "test@example.com"], None, None)
            .await
            .unwrap();
        crate::shell::run(dir, "git", &["config", "user.name", "Test"], None, None)
            .await
            .unwrap();
        std::fs::write(dir.join("README.md"), "init").unwrap();
        crate::shell::run(dir, "git", &["add", "-A"], None, None).await.unwrap();
        crate::shell::run(dir, "git", &["commit", "-m", "init"], None, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn real_git_dirty_tree_commits_with_correct_message() {
        let tmp = TempDir::new().unwrap();
        init_git_repo_real(tmp.path()).await;
        std::fs::write(tmp.path().join("change.txt"), "change").unwrap();

        let registry = registry_for("my-project", tmp.path().to_str().unwrap(), false);
        let block = CommitAndPush::new(registry);
        let trigger = make_trigger("my-project", "CVE-2026-9999");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        let msg = result.events[0].payload["message"].as_str().unwrap();
        assert!(msg.contains("remediation"), "expected 'remediation' in '{msg}'");
    }

    // --- decide_commit pure function tests ---

    #[test]
    fn decide_commit_skips_nested_loop() {
        let trigger = test_event!(EventType::ProjectIterationCompleted, "proj", {
            "project": "proj",
            "success": true,
            "loop_context": { "strategic": { "iteration": 1 } }
        });
        assert_eq!(decide_commit(&trigger), CommitDecision::SkipNestedLoop);
    }

    #[test]
    fn decide_commit_skips_nested_loop_for_maintenance_completion() {
        let trigger = test_event!(EventType::ProjectMaintenanceCompleted, "proj", {
            "project": "proj",
            "success": true,
            "loop_context": { "strategic": { "iteration": 2 } }
        });
        assert_eq!(decide_commit(&trigger), CommitDecision::SkipNestedLoop);
    }

    #[test]
    fn decide_commit_proceeds_without_expecting_a_commit_when_no_changes() {
        // Stranded commits from an earlier run must still be pushed.
        let trigger = make_trigger_no_changes(EventType::ProjectIterationCompleted, "proj");
        assert!(matches!(
            decide_commit(&trigger),
            CommitDecision::Proceed(Proceed {
                commit_expected: false,
                run_failed: false,
                ..
            })
        ));
    }

    #[test]
    fn decide_commit_marks_failed_run() {
        let trigger = test_event!(EventType::ProjectMaintenanceCompleted, "proj", {
            "project": "proj",
            "success": false,
            "summary": "gates failed after 3 retries"
        });
        assert!(matches!(
            decide_commit(&trigger),
            CommitDecision::Proceed(Proceed {
                run_failed: true,
                ..
            })
        ));
    }

    #[test]
    fn decide_commit_proceeds_with_cve() {
        // make_trigger uses EventType::RemediationCompleted with a full payload including cve.
        // The RemediationCompletedPayload requires `success: bool` — use a complete payload.
        let trigger = test_helpers::make_trigger(
            EventType::RemediationCompleted,
            "proj",
            serde_json::json!({ "cve": "CVE-2026-5555", "success": true }),
        );
        assert!(
            matches!(decide_commit(&trigger), CommitDecision::Proceed(Proceed { cve, .. }) if cve == "CVE-2026-5555")
        );
    }

    #[test]
    fn decide_commit_defaults_cve_to_unknown_when_payload_missing_required_fields() {
        // When the payload cannot be parsed as RemediationCompletedPayload (e.g. missing
        // required `success` field), CVE defaults to "unknown".
        let trigger = make_trigger_for(EventType::ProjectIterationCompleted, "proj");
        assert!(
            matches!(decide_commit(&trigger), CommitDecision::Proceed(Proceed { cve, .. }) if cve == "unknown")
        );
    }

    #[test]
    fn dry_run_returns_empty_for_loop_context_trigger() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = test_event!(EventType::ProjectIterationCompleted, "proj", {
            "project": "proj",
            "success": true,
            "loop_context": { "strategic": { "iteration": 1 } }
        });
        assert!(block.dry_run_events(&trigger).is_empty());
    }

    #[test]
    fn dry_run_returns_empty_for_no_changes_trigger() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = make_trigger_no_changes(EventType::ProjectIterationCompleted, "proj");
        assert!(block.dry_run_events(&trigger).is_empty());
    }

    #[test]
    fn dry_run_and_accepts_agree_on_skip_for_nested_loop() {
        // Both dry_run_events and accepts() must reject a nested loop trigger.
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = test_event!(EventType::ProjectIterationCompleted, "proj", {
            "project": "proj",
            "success": true,
            "loop_context": { "strategic": { "iteration": 1 } }
        });
        assert!(!block.accepts(&trigger), "accepts() must reject nested loop");
        assert!(block.dry_run_events(&trigger).is_empty(), "dry_run must skip nested loop");
    }

    #[test]
    fn dry_run_simulates_no_events_for_no_changes_trigger_it_accepts() {
        // accepts() runs the block (it may have stranded commits to push), but a
        // dry run cannot know whether any exist, so it simulates nothing.
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = make_trigger_no_changes(EventType::ProjectIterationCompleted, "proj");
        assert!(block.accepts(&trigger));
        assert!(block.dry_run_events(&trigger).is_empty());
    }

    #[test]
    fn dry_run_omits_push_for_failed_run() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let block = CommitAndPush::new(registry);
        let trigger = test_event!(EventType::ProjectMaintenanceCompleted, "my-project", {
            "project": "my-project",
            "success": false
        });
        let types: Vec<String> =
            block.dry_run_events(&trigger).iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["project_changes_committed"]);
    }

    #[test]
    fn dry_run_omits_push_when_push_disabled() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), false); // push=false
        let block = CommitAndPush::new(registry);
        let trigger = make_trigger("my-project", "CVE-2026-0001");
        let events = block.dry_run_events(&trigger);
        let types: Vec<String> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(
            types.iter().any(|t| t == "project_changes_committed"),
            "commit event must always be present"
        );
        assert!(
            !types.iter().any(|t| t == "project_changes_pushed"),
            "push event must be omitted when push is disabled"
        );
    }
}
