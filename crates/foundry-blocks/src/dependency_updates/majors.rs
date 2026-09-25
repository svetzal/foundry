//! The majors lane: which major upgrades become their own `foundry task`.
//!
//! A major upgrade never happens inside the nightly maintain session. After
//! maintenance, each major a project may take becomes one task with one
//! objective — `Upgrade <pkg> from <a> to <b> in <project>: adapt call sites,
//! keep all gates green` — run through the normal task formation: its own
//! worktree, the project's gates, and trunk only when complete and green.
//!
//! [`plan`] is pure. It decides, per major:
//!
//! - **Proposed** when the project's policy is `minor` or `patch` and the
//!   major is not a security fix. The summary prints the command to run it.
//! - **Deferred** when maintenance for the project did not succeed.
//! - **Deduped** when a task for the same project, package and target is in
//!   flight or left preserved work (a remainder, a defect, a blocked decision).
//! - **Overflow** past the per-project or per-night cap. Reported, not dropped.
//! - **Dispatch** otherwise.

use std::fmt::Write as _;

use foundry_sdk::payload::{MajorUpgrade, MajorUpgradeStatus, PlannedUpdate};
use foundry_sdk::registry::UpdatePolicy;

/// Default cap on major-upgrade tasks per project per night.
pub const DEFAULT_PER_PROJECT_CAP: u32 = 2;
/// Default cap on major-upgrade tasks per night, across every project.
pub const DEFAULT_PER_NIGHT_CAP: u32 = 6;

/// The dispatch caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub per_project: u32,
    pub per_night: u32,
}

impl Caps {
    /// Caps from `FOUNDRY_MAJOR_TASKS_PER_PROJECT` and
    /// `FOUNDRY_MAJOR_TASKS_PER_NIGHT`, falling back to the defaults when a
    /// variable is unset or not a number.
    pub fn from_env() -> Self {
        let read = |name: &str, default: u32| {
            std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
        };
        Self {
            per_project: read("FOUNDRY_MAJOR_TASKS_PER_PROJECT", DEFAULT_PER_PROJECT_CAP),
            per_night: read("FOUNDRY_MAJOR_TASKS_PER_NIGHT", DEFAULT_PER_NIGHT_CAP),
        }
    }
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            per_project: DEFAULT_PER_PROJECT_CAP,
            per_night: DEFAULT_PER_NIGHT_CAP,
        }
    }
}

/// One project's majors, with what the lane needs to decide them.
#[derive(Debug, Clone)]
pub struct ProjectMajors {
    pub project: String,
    pub policy: UpdatePolicy,
    pub maintain_succeeded: bool,
    pub majors: Vec<PlannedUpdate>,
}

/// A task from the history that could block a dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorTask {
    pub project: String,
    pub objective: String,
    /// Why it blocks: "in flight since …", "preserved remainder at …".
    pub blocking: String,
}

/// The objective a major-upgrade task is dispatched with.
pub fn objective(project: &str, update: &PlannedUpdate) -> String {
    let mut text = format!(
        "Upgrade {} from {} to {} in {project}: adapt call sites, keep all gates green.",
        update.package, update.from, update.to
    );
    let _ = write!(
        text,
        " It is a {} dependency declared in {}.",
        update.ecosystem, update.manifest
    );
    if let Some(id) = &update.security {
        let _ = write!(text, " The upgrade fixes {id}.");
    }
    text.push_str(" Change only what this upgrade needs.");
    text
}

/// The `(package, target, project)` an upgrade objective names, for dedupe.
/// `None` for any other objective.
pub fn parse_objective(objective: &str) -> Option<(String, String, String)> {
    let rest = objective.strip_prefix("Upgrade ")?;
    let (head, _) = rest.split_once(':')?;
    let (package, rest) = head.split_once(" from ")?;
    let (_, rest) = rest.split_once(" to ")?;
    let (target, project) = rest.split_once(" in ")?;
    Some((package.to_string(), target.to_string(), project.to_string()))
}

/// Quote `text` for a POSIX shell.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The command that runs an upgrade task by hand.
pub fn command(project: &str, objective: &str) -> String {
    format!("foundry task {project} {}", shell_quote(objective))
}

fn blocking<'a>(
    prior: &'a [PriorTask],
    project: &str,
    update: &PlannedUpdate,
) -> Option<&'a PriorTask> {
    prior.iter().find(|p| {
        p.project == project
            && parse_objective(&p.objective).is_some_and(|(package, target, in_project)| {
                package == update.package && target == update.to && in_project == project
            })
    })
}

/// Decide every major. Projects are taken in name order and, within a
/// project, security upgrades first, so the caps fall the same way each night.
pub fn plan(projects: &[ProjectMajors], prior: &[PriorTask], caps: Caps) -> Vec<MajorUpgrade> {
    let mut ordered: Vec<&ProjectMajors> = projects.iter().collect();
    ordered.sort_by(|a, b| a.project.cmp(&b.project));
    let mut night = 0u32;
    let mut out = Vec::new();
    for project in ordered {
        let mut majors: Vec<&PlannedUpdate> = project.majors.iter().collect();
        majors.sort_by(|a, b| {
            b.security
                .is_some()
                .cmp(&a.security.is_some())
                .then(a.ecosystem.cmp(&b.ecosystem))
                .then(a.package.cmp(&b.package))
        });
        let mut per_project = 0u32;
        for update in majors {
            let objective = objective(&project.project, update);
            let (status, reason) =
                if project.policy != UpdatePolicy::Major && update.security.is_none() {
                    (
                        MajorUpgradeStatus::Proposed,
                        Some(format!(
                            "policy is {}; majors are proposed, not dispatched",
                            project.policy
                        )),
                    )
                } else if !project.maintain_succeeded {
                    (
                        MajorUpgradeStatus::Deferred,
                        Some("maintenance for the project did not succeed".to_string()),
                    )
                } else if let Some(prior) = blocking(prior, &project.project, update) {
                    (MajorUpgradeStatus::Deduped, Some(prior.blocking.clone()))
                } else if per_project >= caps.per_project {
                    (
                        MajorUpgradeStatus::Overflow,
                        Some(format!("over the per-project cap of {}", caps.per_project)),
                    )
                } else if night >= caps.per_night {
                    (
                        MajorUpgradeStatus::Overflow,
                        Some(format!("over the per-night cap of {}", caps.per_night)),
                    )
                } else {
                    per_project += 1;
                    night += 1;
                    (MajorUpgradeStatus::Dispatch, None)
                };
            out.push(MajorUpgrade {
                project: project.project.clone(),
                ecosystem: update.ecosystem,
                manifest: update.manifest.clone(),
                package: update.package.clone(),
                from: update.from.clone(),
                to: update.to.clone(),
                security: update.security.clone(),
                command: command(&project.project, &objective),
                objective,
                status,
                reason,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_sdk::payload::{ChangeKind, Ecosystem, UpdateClass};

    fn major(package: &str, to: &str) -> PlannedUpdate {
        PlannedUpdate {
            ecosystem: Ecosystem::Hex,
            manifest: ".".to_string(),
            package: package.to_string(),
            from: "1.0.0".to_string(),
            to: to.to_string(),
            class: UpdateClass::Major,
            change: ChangeKind::Manifest,
            security: None,
            beyond_policy: false,
            beyond_hold: false,
        }
    }

    fn project(name: &str, policy: UpdatePolicy, majors: Vec<PlannedUpdate>) -> ProjectMajors {
        ProjectMajors {
            project: name.to_string(),
            policy,
            maintain_succeeded: true,
            majors,
        }
    }

    fn statuses(plan: &[MajorUpgrade]) -> Vec<(&str, &str, MajorUpgradeStatus)> {
        plan.iter()
            .map(|m| (m.project.as_str(), m.package.as_str(), m.status))
            .collect()
    }

    #[test]
    fn the_objective_reads_as_the_brief_specifies_and_parses_back() {
        let text = objective("bedrock", &major("phoenix", "2.0.0"));
        assert!(text.starts_with(
            "Upgrade phoenix from 1.0.0 to 2.0.0 in bedrock: adapt call sites, keep all gates green."
        ));
        assert_eq!(
            parse_objective(&text),
            Some(("phoenix".to_string(), "2.0.0".to_string(), "bedrock".to_string()))
        );
        assert_eq!(parse_objective("Fix the flaky test"), None);
    }

    #[test]
    fn the_command_is_shell_safe() {
        assert_eq!(command("p", "it's done"), r"foundry task p 'it'\''s done'");
    }

    #[test]
    fn minor_and_patch_projects_get_proposals_unless_it_is_security() {
        let mut security = major("jose", "2.0.0");
        security.security = Some("CVE-1".to_string());
        let plan = plan(
            &[project(
                "app",
                UpdatePolicy::Minor,
                vec![major("req", "1.0.0"), security],
            )],
            &[],
            Caps::default(),
        );
        assert_eq!(
            statuses(&plan),
            [
                ("app", "jose", MajorUpgradeStatus::Dispatch),
                ("app", "req", MajorUpgradeStatus::Proposed)
            ]
        );
        assert!(plan[1].command.starts_with("foundry task app 'Upgrade req from"));
    }

    #[test]
    fn caps_overflow_per_project_then_per_night() {
        let caps = Caps {
            per_project: 2,
            per_night: 3,
        };
        let plan = plan(
            &[
                project(
                    "a",
                    UpdatePolicy::Major,
                    vec![
                        major("x", "2.0.0"),
                        major("y", "2.0.0"),
                        major("z", "2.0.0"),
                    ],
                ),
                project("b", UpdatePolicy::Major, vec![major("p", "2.0.0"), major("q", "2.0.0")]),
            ],
            &[],
            caps,
        );
        assert_eq!(
            statuses(&plan),
            [
                ("a", "x", MajorUpgradeStatus::Dispatch),
                ("a", "y", MajorUpgradeStatus::Dispatch),
                ("a", "z", MajorUpgradeStatus::Overflow),
                ("b", "p", MajorUpgradeStatus::Dispatch),
                ("b", "q", MajorUpgradeStatus::Overflow),
            ]
        );
        assert_eq!(plan[2].reason.as_deref(), Some("over the per-project cap of 2"));
        assert_eq!(plan[4].reason.as_deref(), Some("over the per-night cap of 3"));
    }

    #[test]
    fn a_blocking_prior_task_dedupes_without_using_the_cap() {
        let prior = vec![PriorTask {
            project: "a".to_string(),
            objective: objective("a", &major("x", "2.0.0")),
            blocking: "preserved remainder at foundry-task/a-123".to_string(),
        }];
        let caps = Caps {
            per_project: 1,
            per_night: 6,
        };
        let plan = plan(
            &[project(
                "a",
                UpdatePolicy::Major,
                vec![major("x", "2.0.0"), major("y", "2.0.0")],
            )],
            &prior,
            caps,
        );
        assert_eq!(
            statuses(&plan),
            [
                ("a", "x", MajorUpgradeStatus::Deduped),
                ("a", "y", MajorUpgradeStatus::Dispatch)
            ]
        );
        assert_eq!(plan[0].reason.as_deref(), Some("preserved remainder at foundry-task/a-123"));
    }

    #[test]
    fn a_prior_task_for_another_target_does_not_block() {
        let prior = vec![PriorTask {
            project: "a".to_string(),
            objective: objective("a", &major("x", "2.0.0")),
            blocking: "in flight".to_string(),
        }];
        let plan = plan(
            &[project("a", UpdatePolicy::Major, vec![major("x", "3.0.0")])],
            &prior,
            Caps::default(),
        );
        assert_eq!(plan[0].status, MajorUpgradeStatus::Dispatch);
    }

    #[test]
    fn a_failed_maintenance_defers_its_majors() {
        let mut p = project("a", UpdatePolicy::Major, vec![major("x", "2.0.0")]);
        p.maintain_succeeded = false;
        let plan = plan(&[p], &[], Caps::default());
        assert_eq!(plan[0].status, MajorUpgradeStatus::Deferred);
    }

    #[test]
    fn caps_read_from_the_environment_fall_back_to_defaults() {
        assert_eq!(
            Caps::default(),
            Caps {
                per_project: 2,
                per_night: 6
            }
        );
    }
}
