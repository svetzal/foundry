use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use foundry_sdk::payload::{
    Ecosystem, HeldUpdate, LapsedHold, MajorUpgrade, MajorUpgradeStatus, PlannedUpdate, StaleHold,
    UnclassifiedScope, UpdateClass,
};
use foundry_sdk::registry::UpdatePolicy;

use crate::wln;

/// Status of a single project in a maintenance run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ProjectStatus {
    Success,
    Failed(String),
    Skipped(String),
}

/// Result for a single project in a maintenance run.
#[derive(Debug, Clone)]
pub(crate) struct ProjectResult {
    pub(crate) name: String,
    pub(crate) status: ProjectStatus,
    pub(crate) duration_secs: Option<u64>,
}

/// A single release tag audit result.
#[derive(Debug, Clone)]
pub(crate) struct ReleaseAuditEntry {
    pub(crate) name: String,
    pub(crate) tag: String,
    pub(crate) status: String,
}

/// A single auto-release result.
#[derive(Debug, Clone)]
pub(crate) struct AutoReleaseEntry {
    pub(crate) name: String,
    pub(crate) new_tag: Option<String>,
    pub(crate) success: bool,
}

/// A single local install result.
#[derive(Debug, Clone)]
pub(crate) struct LocalInstallEntry {
    pub(crate) name: String,
    pub(crate) method: String,
    pub(crate) success: bool,
}

/// A push-enabled project whose branch holds commits its remote does not.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UnpushedEntry {
    pub(crate) name: String,
    pub(crate) status: UnpushedStatus,
}

/// How far a project is ahead of its remote, or why that is unknown.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum UnpushedStatus {
    Ahead { commits: u32, branch: String },
    Unknown(String),
}

/// A project whose audit did not run, and why.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScannerFailureEntry {
    pub(crate) name: String,
    pub(crate) error: String,
}

/// A project not worked on because its checkout was on another branch.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WrongBranchEntry {
    pub(crate) name: String,
    pub(crate) reason: String,
}

/// A dependency move maintenance made, found by comparing the classification
/// before the maintain agent ran with the one after maintenance completed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AppliedUpdate {
    pub(crate) ecosystem: Ecosystem,
    pub(crate) manifest: String,
    pub(crate) package: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) class: UpdateClass,
    /// `false` when the brief did not list this move: the agent went beyond it.
    pub(crate) in_brief: bool,
}

/// One project's dependency outcome for the night.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectDependencyReport {
    pub(crate) name: String,
    pub(crate) policy: UpdatePolicy,
    pub(crate) policy_set: bool,
    /// `None` when there was no after-maintenance classification to compare.
    pub(crate) applied: Option<Vec<AppliedUpdate>>,
    /// Moves the brief listed that did not happen.
    pub(crate) not_applied: Vec<PlannedUpdate>,
    pub(crate) held_by_policy: Vec<HeldUpdate>,
    pub(crate) held_by_hold: Vec<HeldUpdate>,
    pub(crate) lapsed_holds: Vec<LapsedHold>,
    pub(crate) stale_holds: Vec<StaleHold>,
    pub(crate) vendored: Vec<String>,
    pub(crate) unclassified: Vec<UnclassifiedScope>,
    pub(crate) holds_warning: Option<String>,
}

impl ProjectDependencyReport {
    fn applied_count(&self, class: UpdateClass) -> usize {
        self.applied.iter().flatten().filter(|a| a.class == class).count()
    }

    fn beyond_brief(&self) -> impl Iterator<Item = &AppliedUpdate> {
        self.applied
            .iter()
            .flatten()
            .filter(|a| !a.in_brief || a.class == UpdateClass::Major)
    }
}

/// What the majors lane decided for the night.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct MajorsSummary {
    pub(crate) upgrades: Vec<MajorUpgrade>,
    pub(crate) dispatch_enabled: bool,
    pub(crate) per_project_cap: u32,
    pub(crate) per_night_cap: u32,
    pub(crate) history_warning: Option<String>,
}

/// Aggregate results for a full maintenance run.
#[derive(Debug, Clone)]
pub(crate) struct MaintenanceRunSummary {
    pub(crate) run_at: DateTime<Utc>,
    pub(crate) total_duration_secs: Option<u64>,
    pub(crate) projects: Vec<ProjectResult>,
    pub(crate) release_audits: Vec<ReleaseAuditEntry>,
    pub(crate) auto_releases: Vec<AutoReleaseEntry>,
    pub(crate) local_installs: Vec<LocalInstallEntry>,
    /// Push-enabled projects left with unpushed commits after the run.
    pub(crate) unpushed: Vec<UnpushedEntry>,
    /// Projects whose audit did not run.
    pub(crate) scanner_failures: Vec<ScannerFailureEntry>,
    /// Projects skipped because the checkout was on the wrong branch.
    pub(crate) wrong_branch: Vec<WrongBranchEntry>,
    /// Per-project dependency outcomes, for projects that ran maintenance.
    pub(crate) dependencies: Vec<ProjectDependencyReport>,
    pub(crate) majors: MajorsSummary,
}

fn format_duration(secs: Option<u64>) -> String {
    match secs {
        Some(s) => format!("{s}s"),
        None => "\u{2014}".to_string(),
    }
}

fn render_release_audits(summary: &MaintenanceRunSummary, out: &mut String) {
    if summary.release_audits.is_empty() {
        return;
    }
    wln!(out);
    wln!(out, "## Release Audit");
    wln!(out);
    wln!(out, "| Project | Tag | Status |");
    wln!(out, "|---------|-----|--------|");
    for entry in &summary.release_audits {
        let status_icon = if entry.status == "clean" {
            "\u{2705}"
        } else {
            "\u{26a0}\u{fe0f}"
        };
        wln!(out, "| {} | {} | {} {} |", entry.name, entry.tag, status_icon, entry.status);
    }
}

fn render_auto_releases(summary: &MaintenanceRunSummary, out: &mut String) {
    if summary.auto_releases.is_empty() {
        return;
    }
    wln!(out);
    wln!(out, "## Auto-Releases");
    wln!(out);
    wln!(out, "| Project | Tag | Status |");
    wln!(out, "|---------|-----|--------|");
    for entry in &summary.auto_releases {
        let status_icon = if entry.success {
            "\u{2705}"
        } else {
            "\u{274c}"
        };
        let tag_str = entry.new_tag.as_deref().unwrap_or("\u{2014}");
        wln!(out, "| {} | {} | {} |", entry.name, tag_str, status_icon);
    }
}

fn render_local_installs(summary: &MaintenanceRunSummary, out: &mut String) {
    if summary.local_installs.is_empty() {
        return;
    }
    wln!(out);
    wln!(out, "## Local Installs");
    wln!(out);
    wln!(out, "| Project | Method | Status |");
    wln!(out, "|---------|--------|--------|");
    for entry in &summary.local_installs {
        let status_icon = if entry.success {
            "\u{2705}"
        } else {
            "\u{274c}"
        };
        wln!(out, "| {} | {} | {} |", entry.name, entry.method, status_icon);
    }
}

/// Loud, top-of-report warning for commits the run left unpushed. Placed
/// before the status table so it cannot be missed.
fn render_unpushed(summary: &MaintenanceRunSummary, out: &mut String) {
    if summary.unpushed.is_empty() {
        return;
    }
    wln!(out, "## \u{26a0}\u{fe0f} Unpushed commits");
    wln!(out);
    wln!(
        out,
        "These push-enabled projects hold commits that are not on their remote. \
         Nothing published them; see each project's Commit and Push result."
    );
    wln!(out);
    wln!(out, "| Project | Unpushed |");
    wln!(out, "|---------|----------|");
    for entry in &summary.unpushed {
        let detail = match &entry.status {
            UnpushedStatus::Ahead { commits, branch } => {
                format!("**{commits} commit(s) ahead of origin/{branch}**")
            }
            UnpushedStatus::Unknown(reason) => format!("could not check: {reason}"),
        };
        wln!(out, "| {} | {detail} |", entry.name);
    }
    wln!(out);
}

/// A markdown table cell: pipes escaped, line breaks flattened.
fn cell(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").replace('|', "\\|")
}

/// Audits that did not run. A failed scan is not a clean scan, so it is listed
/// beside the unpushed commits, not folded into the status table.
fn render_scanner_failures(summary: &MaintenanceRunSummary, out: &mut String) {
    if summary.scanner_failures.is_empty() {
        return;
    }
    wln!(out, "## \u{26a0}\u{fe0f} Scanner failures");
    wln!(out);
    wln!(
        out,
        "The dependency audit did not run for these projects. They are not known to be clean."
    );
    wln!(out);
    wln!(out, "| Project | Error |");
    wln!(out, "|---------|-------|");
    for entry in &summary.scanner_failures {
        wln!(out, "| {} | {} |", entry.name, cell(&entry.error));
    }
    wln!(out);
}

/// Projects the run did not work on because the checkout was on another
/// branch. Until someone restores the branch, every nightly skips them.
fn render_wrong_branch(summary: &MaintenanceRunSummary, out: &mut String) {
    if summary.wrong_branch.is_empty() {
        return;
    }
    wln!(out, "## \u{26a0}\u{fe0f} Projects skipped: wrong branch");
    wln!(out);
    wln!(
        out,
        "These checkouts are not on their configured branch, so maintenance skipped them."
    );
    wln!(out);
    wln!(out, "| Project | Reason |");
    wln!(out, "|---------|--------|");
    for entry in &summary.wrong_branch {
        wln!(out, "| {} | {} |", entry.name, cell(&entry.reason));
    }
    wln!(out);
}

fn policy_label(policy: UpdatePolicy, set: bool) -> String {
    if set {
        policy.to_string()
    } else {
        format!("{policy} (not set)")
    }
}

fn major_status_label(status: MajorUpgradeStatus, dispatch_enabled: bool) -> &'static str {
    match (status, dispatch_enabled) {
        (MajorUpgradeStatus::Dispatch, true) => "dispatched",
        (MajorUpgradeStatus::Dispatch, false) => "would dispatch (dry run)",
        (MajorUpgradeStatus::Deduped, _) => "deduped",
        (MajorUpgradeStatus::Overflow, _) => "overflow",
        (MajorUpgradeStatus::Deferred, _) => "deferred",
        (MajorUpgradeStatus::Proposed, _) => "proposed",
    }
}

fn majors_cell(summary: &MaintenanceRunSummary, project: &str) -> String {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for m in summary.majors.upgrades.iter().filter(|m| m.project == project) {
        let label = major_status_label(m.status, summary.majors.dispatch_enabled);
        match counts.iter_mut().find(|(l, _)| *l == label) {
            Some((_, n)) => *n += 1,
            None => counts.push((label, 1)),
        }
    }
    if counts.is_empty() {
        "\u{2014}".to_string()
    } else {
        counts.iter().map(|(l, n)| format!("{n} {l}")).collect::<Vec<_>>().join(", ")
    }
}

/// The cross-project dependency picture, near the top with the other
/// warnings: one row per project, then the things that need a person.
fn render_dependency_drift(summary: &MaintenanceRunSummary, out: &mut String) {
    if summary.dependencies.is_empty() {
        return;
    }
    wln!(out, "## Dependency drift");
    wln!(out);
    wln!(
        out,
        "| Project | Policy | Applied (patch/minor/major) | Held by policy | Held by holds | Majors | Not classified |"
    );
    wln!(
        out,
        "|---------|--------|-----------------------------|----------------|---------------|--------|----------------|"
    );
    for d in &summary.dependencies {
        let applied = if d.applied.is_some() {
            format!(
                "{}/{}/{}",
                d.applied_count(UpdateClass::Patch),
                d.applied_count(UpdateClass::Minor),
                d.applied_count(UpdateClass::Major)
            )
        } else {
            "unknown".to_string()
        };
        wln!(
            out,
            "| {} | {} | {applied} | {} | {} | {} | {} |",
            d.name,
            policy_label(d.policy, d.policy_set),
            d.held_by_policy.len(),
            d.held_by_hold.len(),
            majors_cell(summary, &d.name),
            d.unclassified.len()
        );
    }
    wln!(out);
    render_drift_notes(summary, out);
}

/// The dependency items that need a person, under the drift table.
fn render_drift_notes(summary: &MaintenanceRunSummary, out: &mut String) {
    let beyond: Vec<String> = summary
        .dependencies
        .iter()
        .flat_map(|d| {
            d.beyond_brief().map(move |a| {
                format!("{}: {} {} -> {} ({})", d.name, a.package, a.from, a.to, a.class)
            })
        })
        .collect();
    if !beyond.is_empty() {
        wln!(
            out,
            "**\u{26a0}\u{fe0f} Applied beyond the brief:** {}",
            cell(&beyond.join("; "))
        );
        wln!(out);
    }
    let unset: Vec<&str> = summary
        .dependencies
        .iter()
        .filter(|d| !d.policy_set)
        .map(|d| d.name.as_str())
        .collect();
    if !unset.is_empty() {
        wln!(
            out,
            "**No update policy set** (behaving as {}): {}",
            UpdatePolicy::DEFAULT,
            unset.join(", ")
        );
        wln!(out);
    }
    let lapsed: Vec<String> = summary
        .dependencies
        .iter()
        .flat_map(|d| {
            d.lapsed_holds.iter().map(move |l| {
                format!(
                    "{}: {} (max {}, expired {}; {})",
                    d.name, l.package, l.max, l.expired_on, l.reason
                )
            })
        })
        .collect();
    if !lapsed.is_empty() {
        wln!(out, "**Lapsed holds \u{2014} re-decide:** {}", cell(&lapsed.join("; ")));
        wln!(out);
    }
    let stale: Vec<String> = summary
        .dependencies
        .iter()
        .flat_map(|d| {
            d.stale_holds.iter().map(move |h| {
                format!("{}: {} (locked {} is above cap {})", d.name, h.package, h.locked, h.max)
            })
        })
        .collect();
    if !stale.is_empty() {
        wln!(out, "**Stale holds \u{2014} re-decide:** {}", cell(&stale.join("; ")));
        wln!(out);
    }
    if let Some(w) = &summary.majors.history_warning {
        wln!(out, "**Majors lane:** {}", cell(w));
        wln!(out);
    }
}

fn render_update_list(out: &mut String, title: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    wln!(out, "{title} ({}):", lines.len());
    for line in lines {
        wln!(out, "- {line}");
    }
    wln!(out);
}

/// One major upgrade's line in a project's detail.
fn major_line(m: &MajorUpgrade, dispatch_enabled: bool) -> String {
    let mut line = format!(
        "{}: {} {} -> {}",
        major_status_label(m.status, dispatch_enabled),
        m.package,
        m.from,
        m.to
    );
    if let Some(id) = &m.security {
        let _ = write!(line, " (security {id})");
    }
    if let Some(reason) = &m.reason {
        let _ = write!(line, " \u{2014} {reason}");
    }
    if m.status != MajorUpgradeStatus::Dispatch || !dispatch_enabled {
        let _ = write!(line, "; run: `{}`", m.command);
    }
    line
}

fn applied_line(a: &AppliedUpdate) -> String {
    let note = if a.in_brief && a.class != UpdateClass::Major {
        ""
    } else {
        " \u{2014} **beyond the brief**"
    };
    format!(
        "[{} {}] {} {} -> {} ({}){note}",
        a.ecosystem, a.manifest, a.package, a.from, a.to, a.class
    )
}

fn planned_line(u: &PlannedUpdate) -> String {
    format!(
        "[{} {}] {} {} -> {} ({})",
        u.ecosystem, u.manifest, u.package, u.from, u.to, u.class
    )
}

fn held_line(h: &HeldUpdate) -> String {
    format!(
        "[{} {}] {} {} -> {} ({}): {}",
        h.ecosystem, h.manifest, h.package, h.from, h.to, h.class, h.reason
    )
}

/// One project's dependency detail.
fn render_project_dependencies(
    summary: &MaintenanceRunSummary,
    d: &ProjectDependencyReport,
    out: &mut String,
) {
    wln!(out);
    wln!(out, "### {} \u{2014} policy {}", d.name, policy_label(d.policy, d.policy_set));
    wln!(out);
    match &d.applied {
        None => {
            wln!(out, "Applied: unknown (no after-maintenance classification).");
            wln!(out);
        }
        Some(applied) if applied.is_empty() => {
            wln!(out, "Applied: none.");
            wln!(out);
        }
        Some(applied) => {
            render_update_list(
                out,
                "Applied",
                &applied.iter().map(applied_line).collect::<Vec<_>>(),
            );
        }
    }
    render_update_list(
        out,
        "In the brief but not applied",
        &d.not_applied.iter().map(planned_line).collect::<Vec<_>>(),
    );
    render_update_list(
        out,
        "Held back by policy",
        &d.held_by_policy.iter().map(held_line).collect::<Vec<_>>(),
    );
    render_update_list(
        out,
        "Held by holds",
        &d.held_by_hold.iter().map(held_line).collect::<Vec<_>>(),
    );
    let majors: Vec<String> = summary
        .majors
        .upgrades
        .iter()
        .filter(|m| m.project == d.name)
        .map(|m| major_line(m, summary.majors.dispatch_enabled))
        .collect();
    render_update_list(out, "Major upgrades", &majors);
    render_update_list(
        out,
        "Lapsed holds \u{2014} re-decide",
        &d.lapsed_holds
            .iter()
            .map(|l| {
                format!("{} (max {}, expired {}): {}", l.package, l.max, l.expired_on, l.reason)
            })
            .collect::<Vec<_>>(),
    );
    render_update_list(
        out,
        "Stale holds",
        &d.stale_holds
            .iter()
            .map(|h| {
                format!(
                    "[{} {}] {}: stale hold: locked {} is above cap {}, re-decide ({})",
                    h.ecosystem, h.manifest, h.package, h.locked, h.max, h.reason
                )
            })
            .collect::<Vec<_>>(),
    );
    render_update_list(out, "Vendored, updated upstream", &d.vendored.clone());
    render_update_list(
        out,
        "Not classified",
        &d.unclassified
            .iter()
            .map(|u| format!("{}: {}", u.scope, u.reason))
            .collect::<Vec<_>>(),
    );
    if let Some(w) = &d.holds_warning {
        wln!(out, "Warning: {w}");
        wln!(out);
    }
}

/// Per-project dependency detail, after the status table.
fn render_dependency_details(summary: &MaintenanceRunSummary, out: &mut String) {
    if summary.dependencies.is_empty() {
        return;
    }
    wln!(out);
    wln!(out, "## Dependencies");
    for d in &summary.dependencies {
        render_project_dependencies(summary, d, out);
    }
}

/// Render a maintenance run summary as markdown.
pub(crate) fn render(summary: &MaintenanceRunSummary) -> String {
    let mut out = String::new();

    // Header
    let run_at = summary.run_at.format("%Y-%m-%d %H:%M:%S UTC");
    wln!(out, "# Foundry Maintenance Run \u{2014} {run_at}");
    wln!(out);

    render_unpushed(summary, &mut out);
    render_scanner_failures(summary, &mut out);
    render_wrong_branch(summary, &mut out);
    render_dependency_drift(summary, &mut out);

    // Project status table
    wln!(out, "## Project Status");
    wln!(out);
    wln!(out, "| Project | Status | Duration |");
    wln!(out, "|---------|--------|----------|");

    for project in &summary.projects {
        let status_str = match &project.status {
            ProjectStatus::Success => "\u{2705} success".to_string(),
            ProjectStatus::Failed(_) => "\u{274c} failed".to_string(),
            ProjectStatus::Skipped(_) => "\u{23ed} skipped".to_string(),
        };
        let duration_str = format_duration(project.duration_secs);
        wln!(out, "| {} | {} | {} |", project.name, status_str, duration_str);
    }

    // Failures section — only when there are failures
    let failures: Vec<&ProjectResult> = summary
        .projects
        .iter()
        .filter(|p| matches!(p.status, ProjectStatus::Failed(_)))
        .collect();

    if !failures.is_empty() {
        wln!(out);
        wln!(out, "## Failures");
        for project in failures {
            wln!(out);
            wln!(out, "### {}", project.name);
            if let ProjectStatus::Failed(reason) = &project.status {
                wln!(out, "{reason}");
            }
        }
    }

    render_dependency_details(summary, &mut out);
    render_release_audits(summary, &mut out);
    render_auto_releases(summary, &mut out);
    render_local_installs(summary, &mut out);

    // Timing summary
    let total = summary.projects.len();
    let succeeded = summary.projects.iter().filter(|p| p.status == ProjectStatus::Success).count();
    let failed = summary
        .projects
        .iter()
        .filter(|p| matches!(p.status, ProjectStatus::Failed(_)))
        .count();
    let skipped = summary
        .projects
        .iter()
        .filter(|p| matches!(p.status, ProjectStatus::Skipped(_)))
        .count();

    let projects_with_duration: Vec<u64> =
        summary.projects.iter().filter_map(|p| p.duration_secs).collect();
    let average_duration = if projects_with_duration.is_empty() {
        None
    } else {
        Some(projects_with_duration.iter().sum::<u64>() / projects_with_duration.len() as u64)
    };

    wln!(out);
    wln!(out, "## Summary");
    wln!(out, "- Total projects: {total}");
    wln!(out, "- Succeeded: {succeeded}");
    wln!(out, "- Failed: {failed}");
    wln!(out, "- Skipped: {skipped}");
    wln!(out, "- Projects with unpushed commits: {}", summary.unpushed.len());
    wln!(out, "- Projects with scanner failures: {}", summary.scanner_failures.len());
    wln!(out, "- Projects skipped (wrong branch): {}", summary.wrong_branch.len());
    wln!(out, "- Total duration: {}", format_duration(summary.total_duration_secs));
    wln!(out, "- Average duration: {}", format_duration(average_duration));

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 21, 2, 0, 0).unwrap()
    }

    fn summary_with_unpushed(unpushed: Vec<UnpushedEntry>) -> MaintenanceRunSummary {
        MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: Some(10),
            projects: vec![ProjectResult {
                name: "foundry".to_string(),
                status: ProjectStatus::Success,
                duration_secs: Some(10),
            }],
            release_audits: vec![],
            auto_releases: vec![],
            local_installs: vec![],
            unpushed,
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        }
    }

    #[test]
    fn render_puts_unpushed_commits_first_and_loud() {
        let md = render(&summary_with_unpushed(vec![
            UnpushedEntry {
                name: "foundry".to_string(),
                status: UnpushedStatus::Ahead {
                    commits: 4,
                    branch: "main".to_string(),
                },
            },
            UnpushedEntry {
                name: "gilt-cli".to_string(),
                status: UnpushedStatus::Unknown("could not resolve origin/main".to_string()),
            },
        ]));

        let warning = md.find("Unpushed commits").expect("warning section present");
        let table = md.find("## Project Status").unwrap();
        assert!(warning < table, "the warning comes before the status table");
        assert!(md.contains("| foundry | **4 commit(s) ahead of origin/main** |"), "{md}");
        assert!(md.contains("| gilt-cli | could not check: could not resolve origin/main |"));
        assert!(md.contains("- Projects with unpushed commits: 2"));
    }

    #[test]
    fn render_lists_scanner_failures_and_wrong_branch_projects_up_top() {
        let mut summary = summary_with_unpushed(vec![]);
        summary.scanner_failures = vec![ScannerFailureEntry {
            name: "bedrock".to_string(),
            error: "no mix.exs was found in the current directory".to_string(),
        }];
        summary.wrong_branch = vec![WrongBranchEntry {
            name: "reaction_new".to_string(),
            reason: "wrong branch: chore/dependency-update-2026-09-24, expected main".to_string(),
        }];

        let md = render(&summary);

        let table = md.find("## Project Status").unwrap();
        let scanner = md.find("Scanner failures").expect("scanner failures section");
        let skipped = md.find("Projects skipped: wrong branch").expect("wrong-branch section");
        assert!(scanner < table && skipped < table, "both come before the status table");
        assert!(
            md.contains("| bedrock | no mix.exs was found in the current directory |"),
            "{md}"
        );
        assert!(md.contains(
            "| reaction_new | wrong branch: chore/dependency-update-2026-09-24, expected main |"
        ));
        assert!(md.contains("- Projects with scanner failures: 1"));
        assert!(md.contains("- Projects skipped (wrong branch): 1"));
    }

    #[test]
    fn render_escapes_pipes_and_newlines_in_table_cells() {
        let mut summary = summary_with_unpushed(vec![]);
        summary.scanner_failures = vec![ScannerFailureEntry {
            name: "p".to_string(),
            error: "a | b\nc".to_string(),
        }];
        let md = render(&summary);
        assert!(md.contains("| p | a \\| b c |"), "{md}");
    }

    #[test]
    fn render_omits_unpushed_section_when_everything_is_pushed() {
        let md = render(&summary_with_unpushed(vec![]));
        assert!(!md.contains("## \u{26a0}\u{fe0f} Unpushed commits"));
        assert!(md.contains("- Projects with unpushed commits: 0"));
    }

    #[test]
    fn render_all_succeeded() {
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: Some(90),
            projects: vec![
                ProjectResult {
                    name: "alpha".to_string(),
                    status: ProjectStatus::Success,
                    duration_secs: Some(45),
                },
                ProjectResult {
                    name: "beta".to_string(),
                    status: ProjectStatus::Success,
                    duration_secs: Some(45),
                },
            ],
            release_audits: vec![],
            auto_releases: vec![],
            local_installs: vec![],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };

        let md = render(&summary);

        assert!(md.contains("# Foundry Maintenance Run \u{2014} 2026-03-21 02:00:00 UTC"));
        assert!(md.contains("| alpha | \u{2705} success | 45s |"));
        assert!(md.contains("| beta | \u{2705} success | 45s |"));
        assert!(!md.contains("## Failures"));
        assert!(md.contains("- Total projects: 2"));
        assert!(md.contains("- Succeeded: 2"));
        assert!(md.contains("- Failed: 0"));
        assert!(md.contains("- Skipped: 0"));
        assert!(md.contains("- Total duration: 90s"));
        assert!(md.contains("- Average duration: 45s"));
    }

    #[test]
    fn render_mixed_success_and_failure() {
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: Some(57),
            projects: vec![
                ProjectResult {
                    name: "my-project".to_string(),
                    status: ProjectStatus::Success,
                    duration_secs: Some(45),
                },
                ProjectResult {
                    name: "other-project".to_string(),
                    status: ProjectStatus::Failed("cargo clippy failed: error[E0308]".to_string()),
                    duration_secs: Some(12),
                },
            ],
            release_audits: vec![],
            auto_releases: vec![],
            local_installs: vec![],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };

        let md = render(&summary);

        assert!(md.contains("| my-project | \u{2705} success | 45s |"));
        assert!(md.contains("| other-project | \u{274c} failed | 12s |"));
        assert!(md.contains("## Failures"));
        assert!(md.contains("### other-project"));
        assert!(md.contains("cargo clippy failed: error[E0308]"));
        assert!(md.contains("- Succeeded: 1"));
        assert!(md.contains("- Failed: 1"));
        assert!(md.contains("- Skipped: 0"));
        assert!(md.contains("- Total duration: 57s"));
        assert!(md.contains("- Average duration: 28s"));
    }

    #[test]
    fn render_all_skipped() {
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: None,
            projects: vec![
                ProjectResult {
                    name: "repo-a".to_string(),
                    status: ProjectStatus::Skipped("no Cargo.toml".to_string()),
                    duration_secs: None,
                },
                ProjectResult {
                    name: "repo-b".to_string(),
                    status: ProjectStatus::Skipped("archived".to_string()),
                    duration_secs: None,
                },
            ],
            release_audits: vec![],
            auto_releases: vec![],
            local_installs: vec![],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };

        let md = render(&summary);

        assert!(md.contains("| repo-a | \u{23ed} skipped | \u{2014} |"));
        assert!(md.contains("| repo-b | \u{23ed} skipped | \u{2014} |"));
        assert!(!md.contains("## Failures"));
        assert!(md.contains("- Succeeded: 0"));
        assert!(md.contains("- Failed: 0"));
        assert!(md.contains("- Skipped: 2"));
        assert!(md.contains("- Total duration: \u{2014}"));
        assert!(md.contains("- Average duration: \u{2014}"));
    }

    #[test]
    fn render_empty_project_list() {
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: Some(0),
            projects: vec![],
            release_audits: vec![],
            auto_releases: vec![],
            local_installs: vec![],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };

        let md = render(&summary);

        assert!(md.contains("# Foundry Maintenance Run \u{2014} 2026-03-21 02:00:00 UTC"));
        assert!(md.contains("## Project Status"));
        assert!(!md.contains("## Failures"));
        assert!(md.contains("- Total projects: 0"));
        assert!(md.contains("- Average duration: \u{2014}"));
    }

    #[test]
    fn failures_section_absent_when_no_failures() {
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: Some(10),
            projects: vec![ProjectResult {
                name: "clean".to_string(),
                status: ProjectStatus::Success,
                duration_secs: Some(10),
            }],
            release_audits: vec![],
            auto_releases: vec![],
            local_installs: vec![],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };

        let md = render(&summary);
        assert!(!md.contains("## Failures"));
    }

    #[test]
    fn markdown_table_header_present() {
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: None,
            projects: vec![],
            release_audits: vec![],
            auto_releases: vec![],
            local_installs: vec![],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };

        let md = render(&summary);
        assert!(md.contains("| Project | Status | Duration |"));
        assert!(md.contains("|---------|--------|----------|"));
    }

    #[test]
    fn render_project_with_special_characters_in_name() {
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: Some(5),
            projects: vec![ProjectResult {
                name: "org/repo-name_v2".to_string(),
                status: ProjectStatus::Success,
                duration_secs: Some(5),
            }],
            release_audits: vec![],
            auto_releases: vec![],
            local_installs: vec![],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };

        let md = render(&summary);
        assert!(md.contains("| org/repo-name_v2 | \u{2705} success | 5s |"));
    }

    #[test]
    fn render_very_long_project_name() {
        let long_name = "a".repeat(120);
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: Some(3),
            projects: vec![ProjectResult {
                name: long_name.clone(),
                status: ProjectStatus::Success,
                duration_secs: Some(3),
            }],
            release_audits: vec![],
            auto_releases: vec![],
            local_installs: vec![],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };

        let md = render(&summary);
        assert!(md.contains(&long_name));
        assert!(md.contains("\u{2705} success"));
    }

    #[test]
    fn render_multiple_failures_all_appear_in_failures_section() {
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: Some(30),
            projects: vec![
                ProjectResult {
                    name: "proj-a".to_string(),
                    status: ProjectStatus::Failed("test suite failed".to_string()),
                    duration_secs: Some(15),
                },
                ProjectResult {
                    name: "proj-b".to_string(),
                    status: ProjectStatus::Failed("build error".to_string()),
                    duration_secs: Some(15),
                },
            ],
            release_audits: vec![],
            auto_releases: vec![],
            local_installs: vec![],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };

        let md = render(&summary);
        assert!(md.contains("### proj-a"));
        assert!(md.contains("test suite failed"));
        assert!(md.contains("### proj-b"));
        assert!(md.contains("build error"));
        assert!(md.contains("- Failed: 2"));
    }

    #[test]
    fn render_release_audit_section() {
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: Some(10),
            projects: vec![],
            release_audits: vec![
                ReleaseAuditEntry {
                    name: "alpha".to_string(),
                    tag: "v1.0.0".to_string(),
                    status: "clean".to_string(),
                },
                ReleaseAuditEntry {
                    name: "beta".to_string(),
                    tag: "v2.1.0".to_string(),
                    status: "dirty".to_string(),
                },
            ],
            auto_releases: vec![],
            local_installs: vec![],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };
        let md = render(&summary);
        assert!(md.contains("## Release Audit"));
        assert!(md.contains("| alpha | v1.0.0 |"));
        assert!(md.contains("| beta | v2.1.0 |"));
        assert!(md.contains("clean"));
        assert!(md.contains("dirty"));
    }

    #[test]
    fn render_auto_releases_section() {
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: Some(10),
            projects: vec![],
            release_audits: vec![],
            auto_releases: vec![
                AutoReleaseEntry {
                    name: "alpha".to_string(),
                    new_tag: Some("v1.0.1".to_string()),
                    success: true,
                },
                AutoReleaseEntry {
                    name: "beta".to_string(),
                    new_tag: None,
                    success: false,
                },
            ],
            local_installs: vec![],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };
        let md = render(&summary);
        assert!(md.contains("## Auto-Releases"));
        assert!(md.contains("| alpha | v1.0.1 |"));
        assert!(md.contains("| beta |"));
    }

    #[test]
    fn render_local_installs_section() {
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: Some(10),
            projects: vec![],
            release_audits: vec![],
            auto_releases: vec![],
            local_installs: vec![
                LocalInstallEntry {
                    name: "alpha".to_string(),
                    method: "cargo".to_string(),
                    success: true,
                },
                LocalInstallEntry {
                    name: "beta".to_string(),
                    method: "brew".to_string(),
                    success: false,
                },
            ],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };
        let md = render(&summary);
        assert!(md.contains("## Local Installs"));
        assert!(md.contains("| alpha | cargo |"));
        assert!(md.contains("| beta | brew |"));
    }

    #[test]
    fn empty_new_sections_not_rendered() {
        let summary = MaintenanceRunSummary {
            run_at: fixed_time(),
            total_duration_secs: Some(10),
            projects: vec![],
            release_audits: vec![],
            auto_releases: vec![],
            local_installs: vec![],
            unpushed: vec![],
            scanner_failures: vec![],
            wrong_branch: vec![],
            dependencies: vec![],
            majors: MajorsSummary::default(),
        };
        let md = render(&summary);
        assert!(!md.contains("## Release Audit"));
        assert!(!md.contains("## Auto-Releases"));
        assert!(!md.contains("## Local Installs"));
    }
}
