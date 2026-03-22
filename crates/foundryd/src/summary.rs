use std::fmt::Write as _;

use chrono::{DateTime, Utc};

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

/// Aggregate results for a full maintenance run.
#[derive(Debug, Clone)]
pub(crate) struct MaintenanceRunSummary {
    pub(crate) run_at: DateTime<Utc>,
    pub(crate) total_duration_secs: Option<u64>,
    pub(crate) projects: Vec<ProjectResult>,
}

fn format_duration(secs: Option<u64>) -> String {
    match secs {
        Some(s) => format!("{s}s"),
        None => "\u{2014}".to_string(),
    }
}

/// Render a maintenance run summary as markdown.
pub(crate) fn render(summary: &MaintenanceRunSummary) -> String {
    let mut out = String::new();

    // Header
    let run_at = summary.run_at.format("%Y-%m-%d %H:%M:%S UTC");
    writeln!(out, "# Foundry Maintenance Run \u{2014} {run_at}").unwrap();
    writeln!(out).unwrap();

    // Project status table
    writeln!(out, "## Project Status").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| Project | Status | Duration |").unwrap();
    writeln!(out, "|---------|--------|----------|").unwrap();

    for project in &summary.projects {
        let status_str = match &project.status {
            ProjectStatus::Success => "\u{2705} success".to_string(),
            ProjectStatus::Failed(_) => "\u{274c} failed".to_string(),
            ProjectStatus::Skipped(_) => "\u{23ed} skipped".to_string(),
        };
        let duration_str = format_duration(project.duration_secs);
        writeln!(out, "| {} | {} | {} |", project.name, status_str, duration_str).unwrap();
    }

    // Failures section — only when there are failures
    let failures: Vec<&ProjectResult> = summary
        .projects
        .iter()
        .filter(|p| matches!(p.status, ProjectStatus::Failed(_)))
        .collect();

    if !failures.is_empty() {
        writeln!(out).unwrap();
        writeln!(out, "## Failures").unwrap();
        for project in failures {
            writeln!(out).unwrap();
            writeln!(out, "### {}", project.name).unwrap();
            if let ProjectStatus::Failed(reason) = &project.status {
                writeln!(out, "{reason}").unwrap();
            }
        }
    }

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

    writeln!(out).unwrap();
    writeln!(out, "## Summary").unwrap();
    writeln!(out, "- Total projects: {total}").unwrap();
    writeln!(out, "- Succeeded: {succeeded}").unwrap();
    writeln!(out, "- Failed: {failed}").unwrap();
    writeln!(out, "- Skipped: {skipped}").unwrap();
    writeln!(out, "- Total duration: {}", format_duration(summary.total_duration_secs)).unwrap();
    writeln!(out, "- Average duration: {}", format_duration(average_duration)).unwrap();

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 21, 2, 0, 0).unwrap()
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
        };

        let md = render(&summary);
        assert!(md.contains("### proj-a"));
        assert!(md.contains("test suite failed"));
        assert!(md.contains("### proj-b"));
        assert!(md.contains("build error"));
        assert!(md.contains("- Failed: 2"));
    }
}
