use std::path::Path;

use anyhow::{Result, bail};
use comfy_table::{ContentArrangement, Table};
use foundry_sdk::registry::{
    ActionFlags, InstallConfig, InstallsSkill, ProjectEdits, ProjectSpec, Registry,
    derive_default_skill_install_command,
};

use crate::proto::{
    RegistryAddRequest, RegistryEditRequest, RegistryRemoveRequest, foundry_client::FoundryClient,
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

pub async fn add(registry_path: &Path, addr: &str, offline: bool, spec: ProjectSpec) -> Result<()> {
    let name = spec.name.clone();
    if !offline {
        match FoundryClient::connect(addr.to_string()).await {
            Ok(mut client) => {
                let req = RegistryAddRequest {
                    name: spec.name,
                    path: spec.path,
                    stack: spec.stack.to_string(),
                    agent: spec.agent,
                    repo: spec.repo,
                    branch: spec.branch,
                    iterate: spec.iterate,
                    maintain: spec.maintain,
                    push: spec.push,
                    audit: spec.audit,
                    release: spec.release,
                    install_command: spec.install_command.unwrap_or_default(),
                    install_brew: spec.install_brew.unwrap_or_default(),
                    notes: spec.notes.unwrap_or_default(),
                    timeout_secs: spec.timeout_secs.unwrap_or(0),
                };
                client
                    .registry_add(req)
                    .await
                    .map_err(|s| anyhow::anyhow!("daemon error: {} — {}", s.code(), s.message()))?;
                println!("Added project '{name}' to registry.");
                return Ok(());
            }
            Err(_) => {
                eprintln!("warning: daemon not reachable, falling back to direct file mutation");
            }
        }
    }

    // Offline path — mutate registry.json directly.
    add_offline(registry_path, spec)
}

fn add_offline(registry_path: &Path, spec: ProjectSpec) -> Result<()> {
    let name = spec.name.clone();
    let mut registry = load_or_init(registry_path)?;
    registry.add_project(spec).map_err(|e| anyhow::anyhow!("{e}"))?;
    registry.save(registry_path)?;
    println!("Added project '{name}' to registry.");
    Ok(())
}

pub async fn remove(registry_path: &Path, addr: &str, offline: bool, name: &str) -> Result<()> {
    if !offline {
        match FoundryClient::connect(addr.to_string()).await {
            Ok(mut client) => {
                let req = RegistryRemoveRequest {
                    name: name.to_string(),
                };
                client
                    .registry_remove(req)
                    .await
                    .map_err(|s| anyhow::anyhow!("daemon error: {} — {}", s.code(), s.message()))?;
                println!("Removed project '{name}' from registry.");
                return Ok(());
            }
            Err(_) => {
                eprintln!("warning: daemon not reachable, falling back to direct file mutation");
            }
        }
    }

    // Offline path — mutate registry.json directly.
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

pub async fn edit(
    registry_path: &Path,
    addr: &str,
    offline: bool,
    name: &str,
    edits: ProjectEdits,
) -> Result<()> {
    if !offline {
        match FoundryClient::connect(addr.to_string()).await {
            Ok(mut client) => {
                let (skip_str, clear_skip) = match &edits.skip {
                    None => (String::new(), false),
                    Some(None) => (String::new(), true),
                    Some(Some(reason)) => (reason.clone(), false),
                };
                let notes_str = edits.notes.as_deref().unwrap_or("").to_string();
                let clear_notes = edits.notes.as_deref().is_some_and(str::is_empty);
                let req = RegistryEditRequest {
                    name: name.to_string(),
                    path: edits.path.unwrap_or_default(),
                    stack: edits.stack.map(|s| s.to_string()).unwrap_or_default(),
                    agent: edits.agent.unwrap_or_default(),
                    repo: edits.repo.unwrap_or_default(),
                    branch: edits.branch.unwrap_or_default(),
                    skip: skip_str,
                    clear_skip,
                    iterate: edits.iterate.unwrap_or(false),
                    clear_iterate: edits.iterate.is_some_and(|v| !v),
                    maintain: edits.maintain.unwrap_or(false),
                    clear_maintain: edits.maintain.is_some_and(|v| !v),
                    push: edits.push.unwrap_or(false),
                    clear_push: edits.push.is_some_and(|v| !v),
                    audit: edits.audit.unwrap_or(false),
                    clear_audit: edits.audit.is_some_and(|v| !v),
                    release: edits.release.unwrap_or(false),
                    clear_release: edits.release.is_some_and(|v| !v),
                    install_command: edits.install_command.unwrap_or_default(),
                    install_brew: edits.install_brew.unwrap_or_default(),
                    clear_install: edits.clear_install,
                    notes: notes_str,
                    clear_notes,
                    timeout_secs: edits.timeout_secs.unwrap_or(0),
                    clear_timeout: edits.clear_timeout,
                };
                client
                    .registry_edit(req)
                    .await
                    .map_err(|s| anyhow::anyhow!("daemon error: {} — {}", s.code(), s.message()))?;
                println!("Updated project '{name}'.");
                return Ok(());
            }
            Err(_) => {
                eprintln!("warning: daemon not reachable, falling back to direct file mutation");
            }
        }
    }

    // Offline path — mutate registry.json directly.
    edit_offline(registry_path, name, edits)
}

fn edit_offline(registry_path: &Path, name: &str, edits: ProjectEdits) -> Result<()> {
    let mut registry = Registry::load(registry_path)?;
    registry.edit_project(name, edits).map_err(|e| anyhow::anyhow!("{e}"))?;
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
    use foundry_sdk::registry::{InstallConfig, InstallsSkill};

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
