//! Trunk CI after a landing: the watch Foundry keeps on a landed commit, and
//! the rule that concludes it from the CI runs GitHub reports.
//!
//! Foundry's gates run on the Foundry host, which has tools the CI runners may
//! lack and cannot show failures that only another platform hits. A landing
//! that passed every gate there can still turn the branch's CI red. So when a
//! task lands, its ledger item records a [`TrunkCiWatch`] in
//! [`TrunkCiState::Pending`], and the daemon's watcher asks GitHub for the
//! runs of that commit until [`conclude`] decides.
//!
//! Everything here is pure: no I/O and no clock. The watcher supplies the runs
//! (or the error it got asking for them) and the time.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// How long a watch waits for CI to report any run for its commit before it
/// concludes [`TrunkCiState::NoRun`].
pub const TRUNK_CI_GRACE: Duration = Duration::minutes(15);

/// How long a watch may stay pending (runs still going, or GitHub unreachable)
/// before it concludes [`TrunkCiState::Unavailable`].
pub const TRUNK_CI_LIMIT: Duration = Duration::hours(2);

/// The prefix of the reason a campaign advance stands down with while trunk CI
/// is pending on its project. The watcher finds stood-down advances by it.
pub const AWAITING_TRUNK_CI: &str = "awaiting trunk CI on ";

/// The reason a campaign advance stands down with while `item`'s watch of
/// `commit` is pending.
#[must_use]
pub fn awaiting_reason(commit: &str, item: &str) -> String {
    format!("{AWAITING_TRUNK_CI}{} ({item})", short_commit(commit))
}

/// The first seven characters of a commit id, or all of it when shorter.
#[must_use]
pub fn short_commit(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// Where a trunk CI watch stands.
///
/// Only `Failed` is red. `NoRun` (CI never reported a run for the commit) and
/// `Unavailable` (Foundry could not tell in time) say nothing about the
/// commit, so they open nothing and gate nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrunkCiState {
    /// Waiting for CI to conclude.
    Pending,
    /// Every relevant run passed.
    Passed,
    /// A relevant run failed.
    Failed,
    /// No relevant run appeared within [`TRUNK_CI_GRACE`].
    NoRun,
    /// Still unknown after [`TRUNK_CI_LIMIT`].
    Unavailable,
}

impl TrunkCiState {
    /// The serialized tag this state is written to disk and to the wire as.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::NoRun => "no_run",
            Self::Unavailable => "unavailable",
        }
    }
}

/// The trunk CI watch on one landed work item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrunkCiWatch {
    /// The GitHub repository (`owner/name`) CI runs for.
    pub repo: String,
    /// The branch the commit landed on.
    pub branch: String,
    /// The landed commit.
    pub commit: String,
    /// Where the watch stands.
    pub state: TrunkCiState,
    /// When the item landed and the watch began.
    pub requested_at: DateTime<Utc>,
    /// When the watch concluded. Absent while pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concluded_at: Option<DateTime<Utc>>,
    /// The run that decided the conclusion, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_url: Option<String>,
    /// The failed jobs of a red run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing_jobs: Vec<String>,
    /// The end of a red run's failed-job log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_tail: Option<String>,
    /// Why the watch stands where it does, when that needs saying: the last
    /// error asking GitHub, why it gave up, or why red opened nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The follow-up a red watch opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<String>,
}

impl TrunkCiWatch {
    /// A new pending watch of `commit` on `repo`'s `branch`, begun at `at`.
    #[must_use]
    pub fn pending(repo: &str, branch: &str, commit: &str, at: DateTime<Utc>) -> Self {
        Self {
            repo: repo.to_string(),
            branch: branch.to_string(),
            commit: commit.to_string(),
            state: TrunkCiState::Pending,
            requested_at: at,
            concluded_at: None,
            run_url: None,
            failing_jobs: Vec::new(),
            log_tail: None,
            detail: None,
            follow_up: None,
        }
    }

    /// Whether the watch is still waiting for CI.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.state == TrunkCiState::Pending
    }
}

/// One CI run GitHub reports for a commit, as `gh run list --json
/// databaseId,status,conclusion,workflowName,url,event,headBranch` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CiRun {
    /// The run's id, for `gh run view`.
    pub database_id: u64,
    /// `queued`, `in_progress`, `completed`, ...
    pub status: String,
    /// The conclusion of a completed run; empty while it runs.
    #[serde(default)]
    pub conclusion: String,
    /// The workflow the run belongs to.
    #[serde(default)]
    pub workflow_name: String,
    /// The run's page.
    #[serde(default)]
    pub url: String,
    /// What triggered it (`push`, `pull_request`, ...).
    #[serde(default)]
    pub event: String,
    /// The branch the run was for.
    #[serde(default)]
    pub head_branch: String,
}

impl CiRun {
    fn is_completed(&self) -> bool {
        self.status == "completed"
    }
}

/// Conclusions that say nothing about the commit: the run did not judge it.
const IGNORED_CONCLUSIONS: [&str; 3] = ["cancelled", "skipped", "stale"];
/// Conclusions that make a completed run red.
const FAILED_CONCLUSIONS: [&str; 4] =
    ["failure", "timed_out", "startup_failure", "action_required"];
/// Conclusions that make a completed run green.
const PASSED_CONCLUSIONS: [&str; 2] = ["success", "neutral"];

/// What the runs of a commit say about it, before any failure evidence is
/// fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiVerdict {
    /// Keep waiting. `detail` says why when that is worth recording (a
    /// GitHub error, say).
    Pending { detail: Option<String> },
    /// Every relevant run passed; `run_url` names one of them.
    Passed { run_url: Option<String> },
    /// These relevant completed runs failed.
    Failed { runs: Vec<CiRun> },
    /// No relevant run appeared within [`TRUNK_CI_GRACE`].
    NoRun { detail: String },
    /// Still unknown after [`TRUNK_CI_LIMIT`].
    Unavailable { detail: String },
}

/// Conclude a watch of a commit on `branch` begun at `requested_at`, from the
/// runs GitHub reported for it at `now` (or the error asking for them).
///
/// Runs for another branch, and runs `cancelled`, `skipped` or `stale`, are
/// ignored. Of the rest: any completed run that failed (`failure`,
/// `timed_out`, `startup_failure`, `action_required`) makes it
/// [`CiVerdict::Failed`]; any run not yet completed, or completed with a
/// conclusion this rule does not know, keeps it pending; otherwise every run
/// passed (`success`, `neutral`). With no relevant run it stays pending for
/// [`TRUNK_CI_GRACE`] and then concludes [`CiVerdict::NoRun`]. Pending for
/// [`TRUNK_CI_LIMIT`], a GitHub error included, concludes
/// [`CiVerdict::Unavailable`].
#[must_use]
pub fn conclude(
    runs: Result<&[CiRun], &str>,
    branch: &str,
    requested_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> CiVerdict {
    let elapsed = now - requested_at;
    let past_limit = elapsed >= TRUNK_CI_LIMIT;
    let runs = match runs {
        Ok(runs) => runs,
        Err(error) => {
            let error = error.trim();
            return if past_limit {
                CiVerdict::Unavailable {
                    detail: format!("trunk CI still unknown after 2 hours: {error}"),
                }
            } else {
                CiVerdict::Pending {
                    detail: Some(format!("could not read trunk CI: {error}")),
                }
            };
        }
    };
    let relevant: Vec<&CiRun> = runs
        .iter()
        .filter(|run| run.head_branch == branch)
        .filter(|run| !IGNORED_CONCLUSIONS.contains(&run.conclusion.as_str()))
        .collect();
    let failed: Vec<CiRun> = relevant
        .iter()
        .filter(|run| run.is_completed() && FAILED_CONCLUSIONS.contains(&run.conclusion.as_str()))
        .map(|run| (*run).clone())
        .collect();
    if !failed.is_empty() {
        return CiVerdict::Failed { runs: failed };
    }
    let undecided: Vec<&&CiRun> = relevant
        .iter()
        .filter(|run| !run.is_completed() || !PASSED_CONCLUSIONS.contains(&run.conclusion.as_str()))
        .collect();
    if !undecided.is_empty() {
        let names = undecided
            .iter()
            .map(|run| {
                let state = if run.is_completed() {
                    &run.conclusion
                } else {
                    &run.status
                };
                format!("{} {state}", run.workflow_name)
            })
            .collect::<Vec<_>>()
            .join(", ");
        return if past_limit {
            CiVerdict::Unavailable {
                detail: format!("trunk CI still running after 2 hours: {names}"),
            }
        } else {
            CiVerdict::Pending { detail: None }
        };
    }
    if let Some(run) = relevant.first() {
        return CiVerdict::Passed {
            run_url: Some(run.url.clone()).filter(|url| !url.is_empty()),
        };
    }
    if elapsed >= TRUNK_CI_GRACE {
        CiVerdict::NoRun {
            detail: format!("no CI run on {branch} for this commit within 15 minutes"),
        }
    } else {
        CiVerdict::Pending { detail: None }
    }
}

/// A concluded (or still pending) watch, with the failure evidence a red one
/// carries, ready for the ledger (see
/// [`crate::work_item::WorkItemStore::apply_trunk_ci`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrunkCiConclusion {
    /// Still waiting; `detail` replaces the recorded one.
    Pending { detail: Option<String> },
    /// Green.
    Passed { run_url: Option<String> },
    /// Red, with the failing run, its failed jobs and the end of its log.
    /// `detail` records anything that went wrong fetching that evidence.
    Failed {
        run_url: Option<String>,
        failing_jobs: Vec<String>,
        log_tail: Option<String>,
        detail: Option<String>,
    },
    /// No run appeared.
    NoRun { detail: String },
    /// Unknown after the limit.
    Unavailable { detail: String },
}

impl TrunkCiConclusion {
    /// The state this conclusion leaves the watch in.
    #[must_use]
    pub fn state(&self) -> TrunkCiState {
        match self {
            Self::Pending { .. } => TrunkCiState::Pending,
            Self::Passed { .. } => TrunkCiState::Passed,
            Self::Failed { .. } => TrunkCiState::Failed,
            Self::NoRun { .. } => TrunkCiState::NoRun,
            Self::Unavailable { .. } => TrunkCiState::Unavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_790_000_000, 0).expect("in range") + Duration::minutes(minutes)
    }

    fn run(status: &str, conclusion: &str) -> CiRun {
        CiRun {
            database_id: 7,
            status: status.to_string(),
            conclusion: conclusion.to_string(),
            workflow_name: "CI".to_string(),
            url: "https://github.com/o/r/actions/runs/7".to_string(),
            event: "push".to_string(),
            head_branch: "main".to_string(),
        }
    }

    fn verdict(runs: &[CiRun], minutes: i64) -> CiVerdict {
        conclude(Ok(runs), "main", at(0), at(minutes))
    }

    #[test]
    fn every_failing_conclusion_is_red_even_beside_a_pass_or_a_running_run() {
        for conclusion in FAILED_CONCLUSIONS {
            let red = run("completed", conclusion);
            let runs = [
                run("completed", "success"),
                run("in_progress", ""),
                red.clone(),
            ];
            assert_eq!(verdict(&runs, 1), CiVerdict::Failed { runs: vec![red] }, "{conclusion}");
        }
    }

    #[test]
    fn a_run_still_going_keeps_the_watch_pending_until_the_limit() {
        for status in ["queued", "in_progress", "waiting"] {
            let runs = [run("completed", "success"), run(status, "")];
            assert_eq!(verdict(&runs, 119), CiVerdict::Pending { detail: None }, "{status}");
            assert_eq!(
                verdict(&runs, 120),
                CiVerdict::Unavailable {
                    detail: format!("trunk CI still running after 2 hours: CI {status}")
                }
            );
        }
    }

    #[test]
    fn success_and_neutral_runs_pass_and_name_a_run() {
        let runs = [run("completed", "success"), run("completed", "neutral")];
        assert_eq!(
            verdict(&runs, 1),
            CiVerdict::Passed {
                run_url: Some("https://github.com/o/r/actions/runs/7".to_string())
            }
        );
    }

    #[test]
    fn cancelled_skipped_stale_and_other_branches_are_ignored() {
        let mut elsewhere = run("completed", "failure");
        elsewhere.head_branch = "feature".to_string();
        let ignored = [
            run("completed", "cancelled"),
            run("completed", "skipped"),
            run("completed", "stale"),
            elsewhere,
        ];
        assert_eq!(verdict(&ignored, 14), CiVerdict::Pending { detail: None });
        let mut with_pass = ignored.to_vec();
        with_pass.push(run("completed", "success"));
        assert!(matches!(verdict(&with_pass, 1), CiVerdict::Passed { .. }));
    }

    #[test]
    fn no_relevant_run_waits_out_the_grace_then_concludes_no_run() {
        assert_eq!(verdict(&[], 0), CiVerdict::Pending { detail: None });
        assert_eq!(verdict(&[], 14), CiVerdict::Pending { detail: None });
        assert_eq!(
            verdict(&[], 15),
            CiVerdict::NoRun {
                detail: "no CI run on main for this commit within 15 minutes".to_string()
            }
        );
    }

    #[test]
    fn an_unknown_completed_conclusion_is_undecided_not_green() {
        let runs = [run("completed", "success"), run("completed", "mystery")];
        assert_eq!(verdict(&runs, 1), CiVerdict::Pending { detail: None });
        assert!(matches!(verdict(&runs, 120), CiVerdict::Unavailable { .. }));
    }

    #[test]
    fn a_github_error_stays_pending_with_its_detail_until_the_limit() {
        assert_eq!(
            conclude(Err(" HTTP 502 \n"), "main", at(0), at(30)),
            CiVerdict::Pending {
                detail: Some("could not read trunk CI: HTTP 502".to_string())
            }
        );
        assert_eq!(
            conclude(Err("HTTP 502"), "main", at(0), at(120)),
            CiVerdict::Unavailable {
                detail: "trunk CI still unknown after 2 hours: HTTP 502".to_string()
            }
        );
    }

    #[test]
    fn a_watch_round_trips_with_snake_case_state_and_omits_what_is_unset() {
        let watch = TrunkCiWatch::pending("o/r", "main", "abcdef0123", at(0));
        let json = serde_json::to_value(&watch).unwrap();
        assert_eq!(json["state"], "pending");
        for absent in [
            "concluded_at",
            "run_url",
            "failing_jobs",
            "log_tail",
            "detail",
            "follow_up",
        ] {
            assert!(json.get(absent).is_none(), "{absent} is omitted when unset");
        }
        assert_eq!(serde_json::from_value::<TrunkCiWatch>(json).unwrap(), watch);
        for state in [
            TrunkCiState::Pending,
            TrunkCiState::Passed,
            TrunkCiState::Failed,
            TrunkCiState::NoRun,
            TrunkCiState::Unavailable,
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), state.tag());
        }
    }

    #[test]
    fn the_awaiting_reason_names_the_short_commit_and_the_item() {
        assert_eq!(awaiting_reason("abcdef0123456", "wi_1"), "awaiting trunk CI on abcdef0 (wi_1)");
        assert!(awaiting_reason("abc", "wi_1").starts_with(AWAITING_TRUNK_CI));
        assert_eq!(short_commit("abc"), "abc");
    }

    #[test]
    fn gh_run_json_parses_into_runs() {
        let json = r#"[{"databaseId":42,"status":"completed","conclusion":"failure",
            "workflowName":"CI","url":"u","event":"push","headBranch":"main"},
            {"databaseId":43,"status":"in_progress","conclusion":"","workflowName":"Docs",
            "url":"v","event":"push","headBranch":"main"}]"#;
        let runs: Vec<CiRun> = serde_json::from_str(json).unwrap();
        assert_eq!(runs[0].database_id, 42);
        assert_eq!(runs[1].status, "in_progress");
        assert_eq!(runs[1].conclusion, "");
    }
}
