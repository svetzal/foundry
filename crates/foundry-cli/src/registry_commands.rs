use std::path::Path;

use anyhow::{Result, bail};
use comfy_table::{ContentArrangement, Table};
use foundry_core::registry::{
    ActionFlags, InstallConfig, InstallsSkill, ProjectEntry, Registry, Stack,
    derive_default_skill_install_command,
};

pub fn init(registry_path: &Path) -> Result<()> {
    if registry_path.exists() {
        println!("Registry already exists at {}", registry_path.display());
        return Ok(());
    }

    let registry = Registry {
        version: 2,
        projects: vec![],
    };
    registry.save(registry_path)?;
    println!("Created empty registry at {}", registry_path.display());
    Ok(())
}

pub fn list(registry_path: &Path) -> Result<()> {
    let registry = Registry::load(registry_path)?;

    if registry.projects.is_empty() {
        println!("No projects in registry.");
        return Ok(());
    }

    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["Name", "Stack", "Skip", "Actions", "Skill"]);

    for p in &registry.projects {
        let skip = if p.skip.is_some() { "yes" } else { "no" };
        table.add_row(vec![
            p.name.as_str(),
            p.stack.to_string().as_str(),
            skip,
            format_actions(&p.actions).as_str(),
            format_installs_skill_cell(p.installs_skill.as_ref()),
        ]);
    }

    println!("{table}");

    Ok(())
}

pub fn show(registry_path: &Path, name: &str) -> Result<()> {
    let registry = Registry::load(registry_path)?;

    let Some(project) = registry.projects.iter().find(|p| p.name == name) else {
        bail!("Project '{name}' not found in registry");
    };

    println!("Name:      {}", project.name);
    println!("Path:      {}", project.path);
    println!("Stack:     {}", project.stack);
    println!("Agent:     {}", project.agent);
    println!("Repo:      {}", project.repo);
    println!("Branch:    {}", project.branch);
    if let Some(ref reason) = project.skip {
        println!("Skip:      {reason}");
    } else {
        println!("Skip:      no");
    }
    println!("Actions:   {}", format_actions(&project.actions));

    if let Some(ref notes) = project.notes {
        println!("Notes:     {notes}");
    }
    if let Some(ref install) = project.install {
        match install {
            InstallConfig::Command(cmd) => println!("Install:   command: {cmd}"),
            InstallConfig::Brew(formula) => println!("Install:   brew: {formula}"),
        }
    }
    if let Some(ref is) = project.installs_skill {
        println!("{}", format_installs_skill_line(is, project.install.as_ref(), &project.name));
    }

    if let Some(timeout) = project.timeout_secs {
        println!("Timeout:   {timeout}s");
    } else {
        println!("Timeout:   3600s (default)");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub fn add(
    registry_path: &Path,
    name: &str,
    path: &str,
    stack: &str,
    agent: &str,
    repo: &str,
    branch: &str,
    iterate: bool,
    maintain: bool,
    push: bool,
    audit: bool,
    release: bool,
    install_command: Option<&str>,
    install_brew: Option<&str>,
    notes: Option<&str>,
    timeout_secs: Option<u64>,
) -> Result<()> {
    let mut registry = load_or_init(registry_path)?;

    if registry.projects.iter().any(|p| p.name == name) {
        bail!("Project '{name}' already exists in registry");
    }

    let stack: Stack = serde_json::from_str(&format!("\"{stack}\"")).map_err(|_| {
        anyhow::anyhow!("Invalid stack: {stack}. Use: rust, python, typescript, elixir")
    })?;

    let install = match (install_command, install_brew) {
        (Some(cmd), _) => Some(InstallConfig::Command(cmd.to_string())),
        (_, Some(formula)) => Some(InstallConfig::Brew(formula.to_string())),
        _ => None,
    };

    registry.projects.push(ProjectEntry {
        name: name.to_string(),
        path: path.to_string(),
        stack,
        agent: agent.to_string(),
        repo: repo.to_string(),
        branch: branch.to_string(),
        skip: None,
        notes: notes.map(str::to_string),
        actions: ActionFlags {
            iterate,
            maintain,
            push,
            audit,
            release,
        },
        install,
        installs_skill: None,
        timeout_secs,
    });

    registry.save(registry_path)?;
    println!("Added project '{name}' to registry.");
    Ok(())
}

pub fn remove(registry_path: &Path, name: &str) -> Result<()> {
    let mut registry = Registry::load(registry_path)?;

    let before = registry.projects.len();
    registry.projects.retain(|p| p.name != name);

    if registry.projects.len() == before {
        bail!("Project '{name}' not found in registry");
    }

    registry.save(registry_path)?;
    println!("Removed project '{name}' from registry.");
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub fn edit(
    registry_path: &Path,
    name: &str,
    path: Option<&str>,
    stack: Option<&str>,
    agent: Option<&str>,
    repo: Option<&str>,
    branch: Option<&str>,
    skip: Option<&str>,
    iterate: Option<bool>,
    maintain: Option<bool>,
    push: Option<bool>,
    audit_flag: Option<bool>,
    release: Option<bool>,
    install_command: Option<&str>,
    install_brew: Option<&str>,
    notes: Option<&str>,
    timeout_secs: Option<u64>,
) -> Result<()> {
    let mut registry = Registry::load(registry_path)?;

    let Some(project) = registry.projects.iter_mut().find(|p| p.name == name) else {
        bail!("Project '{name}' not found in registry");
    };

    if let Some(v) = path {
        project.path = v.to_string();
    }
    if let Some(v) = stack {
        project.stack = serde_json::from_str(&format!("\"{v}\"")).map_err(|_| {
            anyhow::anyhow!("Invalid stack: {v}. Use: rust, python, typescript, elixir")
        })?;
    }
    if let Some(v) = agent {
        project.agent = v.to_string();
    }
    if let Some(v) = repo {
        project.repo = v.to_string();
    }
    if let Some(v) = branch {
        project.branch = v.to_string();
    }
    if let Some(v) = skip {
        if v.is_empty() {
            project.skip = None;
        } else {
            project.skip = Some(v.to_string());
        }
    }
    if let Some(v) = iterate {
        project.actions.iterate = v;
    }
    if let Some(v) = maintain {
        project.actions.maintain = v;
    }
    if let Some(v) = push {
        project.actions.push = v;
    }
    if let Some(v) = audit_flag {
        project.actions.audit = v;
    }
    if let Some(v) = release {
        project.actions.release = v;
    }
    if let Some(cmd) = install_command {
        project.install = Some(InstallConfig::Command(cmd.to_string()));
    }
    if let Some(formula) = install_brew {
        project.install = Some(InstallConfig::Brew(formula.to_string()));
    }
    if let Some(v) = notes {
        if v.is_empty() {
            project.notes = None;
        } else {
            project.notes = Some(v.to_string());
        }
    }
    if let Some(v) = timeout_secs {
        project.timeout_secs = Some(v);
    }

    registry.save(registry_path)?;
    println!("Updated project '{name}'.");
    Ok(())
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

/// Load an existing registry or create a new empty one if the file doesn't exist.
fn load_or_init(path: &Path) -> Result<Registry> {
    if path.exists() {
        Registry::load(path)
    } else {
        Ok(Registry {
            version: 2,
            projects: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use foundry_core::registry::{InstallConfig, InstallsSkill};

    use super::{format_installs_skill_cell, format_installs_skill_line};

    // --- format_installs_skill_line ---

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

    // --- format_installs_skill_cell ---

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
}
