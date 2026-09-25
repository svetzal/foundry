//! `ClassifyDependencyUpdates` — the deterministic step before the maintain
//! agent, and the record of what is still outdated after it.
//!
//! Sinks on three events:
//!
//! - `GateResolutionCompleted` for the maintain workflow (phase `before`):
//!   classifies the project and decides the brief. It carries the gates and
//!   chain context forward, and `Execute Maintain` runs on the event it emits.
//! - `ProjectMaintenanceCompleted` (phase `after`): classifies again, so the
//!   summary can report what maintenance actually applied and the majors lane
//!   sees what is left.
//! - `DependencyReviewRequested` (phase `review`): an on-demand look that
//!   applies nothing.
//!
//! Observer: it reads the repository, the registries and the event log, and
//! writes nothing. It never fails the chain for a classification problem —
//! anything it cannot classify is recorded in the payload with a reason.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    ChainContext, ClassificationPhase, DependencyReviewRequestedPayload,
    DependencyUpdatesClassifiedPayload, ProjectCompletedPayload, SupplyChainScannedPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::workflow::WorkflowType;

use crate::dependency_updates::{self, Advisories, RegistryVersionSource, VersionSource, brief};
use crate::gateway::ShellGateway;

/// How old a supply-chain scan may be and still supply advisories.
const ADVISORY_MAX_AGE_DAYS: i64 = 8;

pub struct ClassifyDependencyUpdates {
    registry: Arc<RwLock<Registry>>,
    source: Arc<dyn VersionSource>,
    events_dir: PathBuf,
    /// Reads the checkout's revision and how far it is behind its remote.
    /// `None` skips that check (tests without a git repository).
    git: Option<Arc<dyn ShellGateway>>,
}

impl ClassifyDependencyUpdates {
    pub fn new(shell: Arc<dyn ShellGateway>, registry: Arc<RwLock<Registry>>) -> Self {
        Self {
            registry,
            source: Arc::new(RegistryVersionSource::new(Arc::clone(&shell))),
            events_dir: foundry_sdk::paths::events_dir(),
            git: Some(shell),
        }
    }

    /// Construct with an explicit release source and event-log directory.
    pub fn with_source(
        source: Arc<dyn VersionSource>,
        registry: Arc<RwLock<Registry>>,
        events_dir: PathBuf,
    ) -> Self {
        Self {
            registry,
            source,
            events_dir,
            git: None,
        }
    }

    /// Also check the checkout's revision against its remote with `git`.
    #[must_use]
    pub fn with_git(mut self, git: Arc<dyn ShellGateway>) -> Self {
        self.git = Some(git);
        self
    }
}

/// The checkout's short revision, and a warning when it is behind
/// `origin/<branch>`. A review fetches first (it may run long after the
/// nightly synced the checkout); the nightly phases compare against the ref
/// the sync just fetched.
async fn checkout_state(
    git: &dyn ShellGateway,
    root: &std::path::Path,
    branch: &str,
    fetch: bool,
) -> (Option<String>, Option<String>) {
    let run = |args: Vec<String>| async move {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        git.run(root, "git", &refs, None, None)
            .await
            .ok()
            .filter(|r| r.success)
            .map(|r| r.stdout.trim().to_string())
    };
    let revision = run(vec!["rev-parse".into(), "--short".into(), "HEAD".into()]).await;
    if fetch
        && run(vec![
            "fetch".into(),
            "-q".into(),
            "origin".into(),
            branch.to_string(),
        ])
        .await
        .is_none()
    {
        return (
            revision,
            Some(format!("could not fetch origin/{branch}; the checkout may be behind it")),
        );
    }
    let behind = run(vec![
        "rev-list".into(),
        "--count".into(),
        format!("HEAD..origin/{branch}"),
    ])
    .await
    .and_then(|n| n.parse::<u64>().ok());
    let warning = match behind {
        Some(0) => None,
        Some(n) => Some(format!(
            "the checkout is {n} commit(s) behind origin/{branch}; this describes {}, not the remote",
            revision.as_deref().unwrap_or("the local checkout")
        )),
        None => Some(format!("could not compare the checkout with origin/{branch}")),
    };
    (revision, warning)
}

/// Which phase a trigger asks for, or `None` when it is not for this block.
fn phase_of(trigger: &Event) -> Option<ClassificationPhase> {
    match trigger.event_type {
        EventType::GateResolutionCompleted => (WorkflowType::from_payload(&trigger.payload)
            == WorkflowType::Maintain)
            .then_some(ClassificationPhase::Before),
        EventType::ProjectMaintenanceCompleted => Some(ClassificationPhase::After),
        EventType::DependencyReviewRequested => Some(ClassificationPhase::Review),
        _ => None,
    }
}

/// The latest supply-chain scan's findings for `project`, if a scan in the
/// last [`ADVISORY_MAX_AGE_DAYS`] days covered it.
pub(crate) fn latest_advisories(events_dir: &Path, project: &str) -> Advisories {
    let since = chrono::Utc::now() - chrono::Duration::days(ADVISORY_MAX_AGE_DAYS);
    let now = chrono::Utc::now();
    let months = [
        since.format("%Y-%m").to_string(),
        now.format("%Y-%m").to_string(),
    ];
    let mut latest: Option<(chrono::DateTime<chrono::Utc>, SupplyChainScannedPayload)> = None;
    for month in months.iter().collect::<std::collections::BTreeSet<_>>() {
        let path = events_dir.join(format!("{month}.jsonl"));
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Advisories {
                    findings: Vec::new(),
                    source: Some(format!("event log unreadable ({}): {e}", path.display())),
                };
            }
        };
        for line in text.lines().filter(|l| l.contains("\"supply_chain_scanned\"")) {
            let Ok(event) = serde_json::from_str::<Event>(line) else {
                continue;
            };
            if event.event_type != EventType::SupplyChainScanned || event.occurred_at < since {
                continue;
            }
            let Ok(scan) = event.parse_payload::<SupplyChainScannedPayload>() else {
                continue;
            };
            if latest.as_ref().is_none_or(|(at, _)| event.occurred_at > *at) {
                latest = Some((event.occurred_at, scan));
            }
        }
    }
    match latest {
        None => Advisories {
            findings: Vec::new(),
            source: Some(format!(
                "no supply-chain scan in the last {ADVISORY_MAX_AGE_DAYS} days; no advisory data"
            )),
        },
        Some((at, scan)) => match scan.projects.into_iter().find(|p| p.project == project) {
            Some(p) if p.scan_error.is_some() => Advisories {
                findings: Vec::new(),
                source: Some(format!(
                    "supply-chain scan of {} could not scan this project: {}",
                    at.format("%Y-%m-%d %H:%M UTC"),
                    p.scan_error.unwrap_or_default()
                )),
            },
            Some(p) => Advisories {
                findings: p.findings,
                source: Some(format!("supply-chain scan of {}", at.format("%Y-%m-%d %H:%M UTC"))),
            },
            None => Advisories {
                findings: Vec::new(),
                source: Some(format!(
                    "supply-chain scan of {} did not cover this project",
                    at.format("%Y-%m-%d %H:%M UTC")
                )),
            },
        },
    }
}

impl TaskBlock for ClassifyDependencyUpdates {
    task_block_meta! {
        name: "Classify Dependency Updates",
        kind: Observer,
        sinks_on: [GateResolutionCompleted, ProjectMaintenanceCompleted, DependencyReviewRequested],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        phase_of(trigger).is_some()
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let Some(phase) = phase_of(trigger) else {
            // Defensive: accepts() filters every trigger this block does not handle.
            return skip!("Skipped: not a dependency classification trigger");
        };
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let chain = ChainContext::extract_from(&trigger.payload);
        let success = (phase == ClassificationPhase::After)
            .then(|| trigger.parse_payload::<ProjectCompletedPayload>().is_ok_and(|p| p.success));
        // A review may preview another policy; nothing else overrides the registry.
        let policy_preview = (phase == ClassificationPhase::Review)
            .then(|| trigger.parse_payload::<DependencyReviewRequestedPayload>().ok())
            .flatten()
            .and_then(|p| p.policy);
        let entry = require_project!(self, project);
        let policy = policy_preview.or(entry.update_policy);
        let source = Arc::clone(&self.source);
        let events_dir = self.events_dir.clone();
        let git = self.git.clone();

        Box::pin(async move {
            let root = PathBuf::from(&entry.path);
            let holds = foundry_sdk::dependency_holds::read_holds(&root).map_err(|e| {
                tracing::warn!(project = %project, error = %e, "dependency holds unreadable; no holds applied");
                format!(".dependency-holds.json unreadable, so no holds applied: {e}")
            });
            let advisory_project = project.clone();
            let advisories = match tokio::task::spawn_blocking(move || {
                latest_advisories(&events_dir, &advisory_project)
            })
            .await
            {
                Ok(a) => a,
                Err(e) => Advisories {
                    findings: Vec::new(),
                    source: Some(format!("advisory lookup failed: {e}")),
                },
            };
            let today = chrono::Utc::now().date_naive();
            let classification = dependency_updates::classify(
                &root,
                &entry.stack,
                holds,
                advisories,
                source.as_ref(),
                today,
            )
            .await;
            let mut classification = classification;
            if let Some(git) = git {
                let (revision, warning) = checkout_state(
                    git.as_ref(),
                    &root,
                    &entry.branch,
                    phase == ClassificationPhase::Review,
                )
                .await;
                classification.revision = revision;
                classification.checkout_warning = warning;
            }
            let decided = brief::build(policy, &classification);
            let rendered = brief::render(&decided, &classification);

            let summary = format!(
                "{project}: {} outdated ({} to apply, {} major, {} held); {} not classified [{}]",
                classification.outdated.len(),
                decided.apply.len(),
                decided.majors.len(),
                decided.held_by_policy.len() + decided.held_by_hold.len(),
                classification.unclassified.len(),
                match phase {
                    ClassificationPhase::Before => "before maintain",
                    ClassificationPhase::After => "after maintain",
                    ClassificationPhase::Review => "review",
                },
            );
            tracing::info!(%project, ?phase, outdated = classification.outdated.len(), "dependency updates classified");

            let workflow = match phase {
                ClassificationPhase::Review => None,
                ClassificationPhase::Before | ClassificationPhase::After => {
                    Some("maintain".to_string())
                }
            };
            let payload = DependencyUpdatesClassifiedPayload {
                project: project.clone(),
                phase,
                workflow,
                success,
                classification,
                brief: decided,
                chain,
            };
            let result: TaskBlockResult = super::emit_result(
                summary,
                EventType::DependencyUpdatesClassified,
                &project,
                throttle,
                &payload,
            )?;
            Ok(result.with_output(Some(rendered), None))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::payload::{
        ClassificationPhase, DependencyUpdatesClassifiedPayload, ProjectSupplyChainScan,
        SupplyChainFinding, SupplyChainScannedPayload,
    };
    use foundry_sdk::registry::{Stack, UpdatePolicy};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use super::super::test_helpers;
    use super::{ClassifyDependencyUpdates, latest_advisories};
    use crate::dependency_updates::fakes::FakeVersionSource;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname='app'\n[dependencies]\nserde='1.0.100'\nrand='0.8'\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Cargo.lock"),
            "version=4\n[[package]]\nname='serde'\nversion='1.0.100'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n[[package]]\nname='rand'\nversion='0.8.5'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n",
        )
        .unwrap();
        dir
    }

    fn block(
        dir: &std::path::Path,
        policy: Option<UpdatePolicy>,
        events: &std::path::Path,
    ) -> ClassifyDependencyUpdates {
        let mut entry = test_helpers::project_entry("my-project", dir.to_str().unwrap());
        entry.stack = Stack::Rust;
        entry.update_policy = policy;
        let source = FakeVersionSource::with(&[
            ("serde", &["1.0.100", "1.0.228", "1.1.0"]),
            ("rand", &["0.8.5", "0.9.2"]),
        ]);
        ClassifyDependencyUpdates::with_source(
            Arc::new(source),
            test_helpers::registry_with_entry(entry),
            events.to_path_buf(),
        )
    }

    fn gates_trigger(workflow: &str) -> Event {
        test_event!(EventType::GateResolutionCompleted, "my-project", {
            "project": "my-project",
            "workflow": workflow,
            "gates": [{"name": "fmt", "command": "cargo fmt --check", "required": true}],
            "actions": {"maintain": true},
        })
    }

    assert_block_meta!(
        ClassifyDependencyUpdates::with_source(
            Arc::new(FakeVersionSource::with(&[])),
            test_helpers::empty_registry(),
            std::path::PathBuf::from("/nonexistent"),
        ),
        kind: Observer,
        sinks_on: [GateResolutionCompleted, ProjectMaintenanceCompleted, DependencyReviewRequested],
    );

    #[test]
    fn accepts_returns_false_when_gate_resolution_is_not_for_maintain() {
        let dir = tempfile::tempdir().unwrap();
        let b = block(dir.path(), None, dir.path());
        assert!(!b.accepts(&gates_trigger("iterate")));
        assert!(b.accepts(&gates_trigger("maintain")));
    }

    #[test]
    fn accepts_returns_true_when_maintenance_completes_or_a_review_is_requested() {
        let dir = tempfile::tempdir().unwrap();
        let b = block(dir.path(), None, dir.path());
        let done =
            test_event!(EventType::ProjectMaintenanceCompleted, "my-project", {"success": true});
        let review = test_event!(EventType::DependencyReviewRequested, "my-project", {});
        assert!(b.accepts(&done));
        assert!(b.accepts(&review));
    }

    #[tokio::test]
    async fn before_maintain_it_emits_the_brief_and_carries_the_gates_forward() {
        let dir = repo();
        let events = tempfile::tempdir().unwrap();
        let b = block(dir.path(), Some(UpdatePolicy::Patch), events.path());

        let result = b.execute(&gates_trigger("maintain")).await.unwrap();

        assert!(result.success, "{}", result.summary);
        assert_eq!(result.events.len(), 1);
        let event = &result.events[0];
        assert_eq!(event.event_type, EventType::DependencyUpdatesClassified);
        assert_eq!(event.payload["gates"][0]["name"], "fmt");
        assert_eq!(event.payload["actions"]["maintain"], true);
        let p: DependencyUpdatesClassifiedPayload = event.parse_payload().unwrap();
        assert_eq!(p.phase, ClassificationPhase::Before);
        assert_eq!(p.workflow.as_deref(), Some("maintain"));
        assert_eq!(p.brief.policy, UpdatePolicy::Patch);
        assert!(p.brief.policy_set);
        let applied: Vec<(&str, &str)> =
            p.brief.apply.iter().map(|u| (u.package.as_str(), u.to.as_str())).collect();
        assert_eq!(
            applied,
            [("serde", "1.1.0")],
            "a caret requirement admits the minor as a lockfile move"
        );
        assert_eq!(p.brief.majors.len(), 1);
        let output = result.raw_output.unwrap();
        assert!(output.contains("Apply exactly these dependency updates"), "{output}");
        assert!(
            result.summary.contains("2 outdated (1 to apply, 1 major, 0 held)"),
            "{}",
            result.summary
        );
    }

    #[tokio::test]
    async fn after_maintain_it_records_the_outcome_and_phase() {
        let dir = repo();
        let events = tempfile::tempdir().unwrap();
        let b = block(dir.path(), None, events.path());
        let done = test_event!(EventType::ProjectMaintenanceCompleted, "my-project", {
            "project": "my-project", "success": false, "summary": "gates failed", "workflow": "maintain",
        });

        let result = b.execute(&done).await.unwrap();
        let p: DependencyUpdatesClassifiedPayload = result.events[0].parse_payload().unwrap();

        assert_eq!(p.phase, ClassificationPhase::After);
        assert_eq!(p.success, Some(false));
        assert!(!p.brief.policy_set);
    }

    #[tokio::test]
    async fn a_project_missing_from_the_registry_is_a_block_failure() {
        let events = tempfile::tempdir().unwrap();
        let b = ClassifyDependencyUpdates::with_source(
            Arc::new(FakeVersionSource::with(&[])),
            test_helpers::empty_registry(),
            events.path().to_path_buf(),
        );
        let result = b.execute(&gates_trigger("maintain")).await.unwrap();
        assert!(!result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn malformed_holds_are_reported_and_classification_continues() {
        let dir = repo();
        std::fs::write(dir.path().join(".dependency-holds.json"), "{ nope").unwrap();
        let events = tempfile::tempdir().unwrap();
        let b = block(dir.path(), None, events.path());
        let review = test_event!(EventType::DependencyReviewRequested, "my-project", {});

        let result = b.execute(&review).await.unwrap();
        let p: DependencyUpdatesClassifiedPayload = result.events[0].parse_payload().unwrap();

        assert!(p.classification.holds_warning.unwrap().contains("no holds applied"));
        assert_eq!(p.classification.outdated.len(), 2);
        assert_eq!(p.phase, ClassificationPhase::Review);
        assert_eq!(p.workflow, None);
    }

    fn write_scan(
        dir: &std::path::Path,
        project: &str,
        occurred: chrono::DateTime<chrono::Utc>,
        fix: &str,
    ) {
        let payload = SupplyChainScannedPayload {
            projects: vec![ProjectSupplyChainScan {
                project: project.to_string(),
                stack: "rust".to_string(),
                findings: vec![SupplyChainFinding {
                    cve: "RUSTSEC-2026-0001".to_string(),
                    package: "serde".to_string(),
                    version: Some("1.0.100".to_string()),
                    fix_version: Some(fix.to_string()),
                    ..SupplyChainFinding::default()
                }],
                ..ProjectSupplyChainScan::default()
            }],
            ..SupplyChainScannedPayload::default()
        };
        let mut event = Event::new(
            EventType::SupplyChainScanned,
            "system".to_string(),
            Throttle::Full,
            serde_json::to_value(payload).unwrap(),
        );
        event.occurred_at = occurred;
        let path = dir.join(format!("{}.jsonl", occurred.format("%Y-%m")));
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(&serde_json::to_string(&event).unwrap());
        text.push('\n');
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_newest_recent_scan_supplies_advisories() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        write_scan(dir.path(), "my-project", now - chrono::Duration::hours(30), "1.0.150");
        write_scan(dir.path(), "my-project", now - chrono::Duration::hours(20), "1.0.200");
        let a = latest_advisories(dir.path(), "my-project");
        assert_eq!(a.findings.len(), 1);
        assert_eq!(a.findings[0].fix_version.as_deref(), Some("1.0.200"));
        assert!(a.source.unwrap().starts_with("supply-chain scan of "));
    }

    #[test]
    fn stale_or_missing_scans_say_so() {
        let dir = tempfile::tempdir().unwrap();
        let a = latest_advisories(dir.path(), "my-project");
        assert!(a.findings.is_empty());
        assert!(a.source.unwrap().contains("no supply-chain scan"));

        write_scan(dir.path(), "other", chrono::Utc::now() - chrono::Duration::hours(2), "1.0.0");
        let a = latest_advisories(dir.path(), "my-project");
        assert!(a.source.unwrap().contains("did not cover this project"));
    }

    #[tokio::test]
    async fn a_recent_advisory_reaches_the_brief_as_a_security_fix() {
        let dir = repo();
        let events = tempfile::tempdir().unwrap();
        write_scan(
            events.path(),
            "my-project",
            chrono::Utc::now() - chrono::Duration::hours(3),
            "1.0.228",
        );
        let b = block(dir.path(), Some(UpdatePolicy::Patch), events.path());

        let result = b.execute(&gates_trigger("maintain")).await.unwrap();
        let p: DependencyUpdatesClassifiedPayload = result.events[0].parse_payload().unwrap();

        let serde = p.brief.apply.iter().find(|u| u.package == "serde").unwrap();
        assert_eq!(serde.security.as_deref(), Some("RUSTSEC-2026-0001"));
    }

    #[tokio::test]
    async fn a_review_can_preview_another_policy_without_touching_the_registry() {
        let dir = repo();
        let events = tempfile::tempdir().unwrap();
        let b = block(dir.path(), Some(UpdatePolicy::Patch), events.path());
        let review =
            test_event!(EventType::DependencyReviewRequested, "my-project", {"policy": "major"});

        let result = b.execute(&review).await.unwrap();
        let p: DependencyUpdatesClassifiedPayload = result.events[0].parse_payload().unwrap();

        assert_eq!(p.brief.policy, UpdatePolicy::Major);
        let registered = b.registry.read().unwrap().projects[0].update_policy;
        assert_eq!(registered, Some(UpdatePolicy::Patch));
    }

    #[tokio::test]
    async fn only_a_review_may_override_the_policy() {
        let dir = repo();
        let events = tempfile::tempdir().unwrap();
        let b = block(dir.path(), Some(UpdatePolicy::Patch), events.path());
        let mut trigger = gates_trigger("maintain");
        trigger.payload["policy"] = serde_json::json!("major");

        let result = b.execute(&trigger).await.unwrap();
        let p: DependencyUpdatesClassifiedPayload = result.events[0].parse_payload().unwrap();

        assert_eq!(p.brief.policy, UpdatePolicy::Patch);
    }

    fn git_repo_behind(dir: &std::path::Path) -> tempfile::TempDir {
        let git = |cwd: &std::path::Path, args: &[&str]| {
            let out =
                std::process::Command::new("git").current_dir(cwd).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "T"]);
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "one"]);
        git(dir, &["remote", "add", "origin", remote.path().to_str().unwrap()]);
        git(dir, &["push", "-q", "origin", "main"]);
        // Someone else pushes a commit the checkout does not have.
        let other = tempfile::tempdir().unwrap();
        git(other.path(), &["clone", "-q", remote.path().to_str().unwrap(), "."]);
        git(other.path(), &["config", "user.email", "t@example.com"]);
        git(other.path(), &["config", "user.name", "T"]);
        std::fs::write(other.path().join("x"), "x").unwrap();
        git(other.path(), &["add", "-A"]);
        git(other.path(), &["commit", "-q", "-m", "two"]);
        git(other.path(), &["push", "-q", "origin", "main"]);
        remote
    }

    #[tokio::test]
    async fn a_review_warns_when_the_checkout_is_behind_its_remote() {
        let dir = repo();
        let _remote = git_repo_behind(dir.path());
        let events = tempfile::tempdir().unwrap();
        let b = block(dir.path(), None, events.path())
            .with_git(Arc::new(crate::gateway::ProcessShellGateway));
        let review = test_event!(EventType::DependencyReviewRequested, "my-project", {});

        let result = b.execute(&review).await.unwrap();
        let p: DependencyUpdatesClassifiedPayload = result.events[0].parse_payload().unwrap();

        assert!(p.classification.revision.is_some());
        let warning = p.classification.checkout_warning.unwrap();
        assert!(warning.contains("1 commit(s) behind origin/main"), "{warning}");
    }
}
