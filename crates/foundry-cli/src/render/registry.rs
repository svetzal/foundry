//! Pure rendering for project registry display.

use std::fmt::Write as _;

use comfy_table::{ContentArrangement, Table};
use foundry_sdk::registry::{
    ActionFlags, InstallConfig, InstallsSkill, ProjectEntry, derive_default_skill_install_command,
};

/// Render a project's full detail view as a multi-line string.
pub fn project_detail(project: &ProjectEntry) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Name:      {}", project.name);
    let _ = writeln!(out, "Path:      {}", project.path);
    let _ = writeln!(out, "Stack:     {}", project.stack);
    let _ = writeln!(out, "Agent:     {}", project.agent);
    let _ = writeln!(out, "Repo:      {}", project.repo);
    let _ = writeln!(out, "Branch:    {}", project.branch);
    if let Some(ref reason) = project.skip {
        let _ = writeln!(out, "Skip:      {reason}");
    } else {
        let _ = writeln!(out, "Skip:      no");
    }
    let _ = writeln!(out, "Actions:   {}", format_actions(&project.actions));

    if let Some(ref notes) = project.notes {
        let _ = writeln!(out, "Notes:     {notes}");
    }
    if let Some(ref install) = project.install {
        match install {
            InstallConfig::Command(cmd) => {
                let _ = writeln!(out, "Install:   command: {cmd}");
            }
            InstallConfig::Brew(formula) => {
                let _ = writeln!(out, "Install:   brew: {formula}");
            }
        }
    }
    if let Some(ref is) = project.installs_skill {
        let _ = writeln!(
            out,
            "{}",
            format_installs_skill_line(is, project.install.as_ref(), &project.name)
        );
    }
    if let Some(timeout) = project.timeout_secs {
        let _ = writeln!(out, "Timeout:   {timeout}s");
    } else {
        let _ = writeln!(out, "Timeout:   3600s (default)");
    }
    out
}

/// Render the project list as a `comfy_table` string (ends with `\n`).
pub fn project_table(projects: &[ProjectEntry]) -> String {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["Name", "Stack", "Skip", "Actions", "Skill"]);

    for p in projects {
        let skip = if p.skip.is_some() { "yes" } else { "no" };
        table.add_row(vec![
            p.name.as_str(),
            p.stack.to_string().as_str(),
            skip,
            format_actions(&p.actions).as_str(),
            format_installs_skill_cell(p.installs_skill.as_ref()),
        ]);
    }

    let mut out = String::new();
    let _ = writeln!(out, "{table}");
    out
}

/// Format the full "Installs skill: ..." display line for `foundry registry show`.
fn format_installs_skill_line(
    installs_skill: &InstallsSkill,
    install: Option<&InstallConfig>,
    project_name: &str,
) -> String {
    match installs_skill {
        InstallsSkill::Default(true) => {
            let cmd = derive_default_skill_install_command(install, project_name);
            format!("Installs skill: yes (default -- runs {cmd})")
        }
        InstallsSkill::Default(false) => "Installs skill: no (explicitly disabled)".to_string(),
        InstallsSkill::Custom { command } => format!("Installs skill: command: {command}"),
    }
}

/// Format the short cell label for the "Skill" column in `foundry registry list`.
///
/// Returns `"auto"`, `"cmd"`, `"off"`, or `""`.
fn format_installs_skill_cell(installs_skill: Option<&InstallsSkill>) -> &'static str {
    match installs_skill {
        Some(InstallsSkill::Default(true)) => "auto",
        Some(InstallsSkill::Custom { .. }) => "cmd",
        Some(InstallsSkill::Default(false)) => "off",
        None => "",
    }
}

fn format_actions(actions: &ActionFlags) -> String {
    let mut flags = vec![];
    if actions.iterate {
        flags.push("iterate");
    }
    if actions.maintain {
        flags.push("maintain");
    }
    if actions.push {
        flags.push("push");
    }
    if actions.audit {
        flags.push("audit");
    }
    if actions.release {
        flags.push("release");
    }
    if flags.is_empty() {
        "none".to_string()
    } else {
        flags.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::registry::{ActionFlags, InstallConfig, InstallsSkill, ProjectEntry, Stack};

    use super::{
        format_actions, format_installs_skill_cell, format_installs_skill_line, project_detail,
        project_table,
    };

    fn make_project(name: &str) -> ProjectEntry {
        ProjectEntry {
            name: name.to_string(),
            path: format!("/tmp/{name}"),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: format!("owner/{name}"),
            branch: "main".to_string(),
            skip: None,
            actions: ActionFlags::default(),
            install: None,
            installs_skill: None,
            notes: None,
            timeout_secs: None,
            audit_exceptions: vec![],
        }
    }

    // --- format_installs_skill_line (moved from registry_commands.rs) ---

    #[test]
    fn line_default_true_with_brew_formula() {
        let line = format_installs_skill_line(
            &InstallsSkill::Default(true),
            Some(&InstallConfig::Brew("gilt".to_string())),
            "my-project",
        );
        assert_eq!(line, "Installs skill: yes (default -- runs gilt init --global --force)");
    }

    #[test]
    fn line_default_true_with_no_install_falls_back_to_project_name() {
        let line = format_installs_skill_line(&InstallsSkill::Default(true), None, "my-project");
        assert_eq!(line, "Installs skill: yes (default -- runs my-project init --global --force)");
    }

    #[test]
    fn line_default_false() {
        let line = format_installs_skill_line(&InstallsSkill::Default(false), None, "my-project");
        assert_eq!(line, "Installs skill: no (explicitly disabled)");
    }

    #[test]
    fn line_custom_command() {
        let line = format_installs_skill_line(
            &InstallsSkill::Custom {
                command: "gilt skill-init --global --force".to_string(),
            },
            None,
            "my-project",
        );
        assert_eq!(line, "Installs skill: command: gilt skill-init --global --force");
    }

    // --- format_installs_skill_cell (moved from registry_commands.rs) ---

    #[test]
    fn cell_default_true_returns_auto() {
        assert_eq!(format_installs_skill_cell(Some(&InstallsSkill::Default(true))), "auto");
    }

    #[test]
    fn cell_custom_returns_cmd() {
        assert_eq!(
            format_installs_skill_cell(Some(&InstallsSkill::Custom {
                command: "anything".to_string()
            })),
            "cmd"
        );
    }

    #[test]
    fn cell_default_false_returns_off() {
        assert_eq!(format_installs_skill_cell(Some(&InstallsSkill::Default(false))), "off");
    }

    #[test]
    fn cell_none_returns_empty_string() {
        assert_eq!(format_installs_skill_cell(None), "");
    }

    // --- format_actions ---

    #[test]
    fn format_actions_no_flags_returns_none() {
        assert_eq!(format_actions(&ActionFlags::default()), "none");
    }

    #[test]
    fn format_actions_all_flags() {
        let flags = ActionFlags {
            iterate: true,
            maintain: true,
            push: true,
            audit: true,
            release: true,
        };
        assert_eq!(format_actions(&flags), "iterate, maintain, push, audit, release");
    }

    // --- project_detail ---

    #[test]
    fn project_detail_contains_all_core_fields() {
        let p = make_project("my-project");
        let out = project_detail(&p);
        assert!(out.contains("Name:      my-project"), "got: {out}");
        assert!(out.contains("Stack:     rust"), "got: {out}");
        assert!(out.contains("Agent:     claude"), "got: {out}");
        assert!(out.contains("Branch:    main"), "got: {out}");
    }

    #[test]
    fn project_detail_skip_shows_reason_when_set() {
        let mut p = make_project("proj");
        p.skip = Some("reason for skip".to_string());
        let out = project_detail(&p);
        assert!(out.contains("Skip:      reason for skip"), "got: {out}");
    }

    #[test]
    fn project_detail_skip_shows_no_when_absent() {
        let p = make_project("proj");
        let out = project_detail(&p);
        assert!(out.contains("Skip:      no"), "got: {out}");
    }

    #[test]
    fn project_detail_default_timeout_label() {
        let p = make_project("proj");
        let out = project_detail(&p);
        assert!(out.contains("Timeout:   3600s (default)"), "got: {out}");
    }

    #[test]
    fn project_detail_explicit_timeout() {
        let mut p = make_project("proj");
        p.timeout_secs = Some(7200);
        let out = project_detail(&p);
        assert!(out.contains("Timeout:   7200s"), "got: {out}");
    }

    #[test]
    fn project_detail_install_skill_line_present_when_set() {
        let mut p = make_project("proj");
        p.installs_skill = Some(InstallsSkill::Default(true));
        let out = project_detail(&p);
        assert!(out.contains("Installs skill:"), "got: {out}");
    }

    // --- project_table ---

    #[test]
    fn project_table_contains_project_names() {
        let projects = vec![make_project("alpha"), make_project("beta")];
        let out = project_table(&projects);
        assert!(out.contains("alpha"), "got: {out}");
        assert!(out.contains("beta"), "got: {out}");
    }

    #[test]
    fn project_table_shows_skip_yes_when_skipped() {
        let mut p = make_project("proj");
        p.skip = Some("reason".to_string());
        let out = project_table(&[p]);
        assert!(out.contains("yes"), "got: {out}");
    }
}
