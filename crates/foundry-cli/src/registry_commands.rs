use std::path::Path;

use anyhow::{Result, bail};
use foundry_sdk::registry::{
    InstallConfig, InstallsSkill, ProjectEdits, ProjectEntry, ProjectSpec, Registry, parse_stack,
};

use crate::daemon::{connect_daemon_required, status_to_anyhow};
use crate::proto::{
    Project, RegistryAddRequest, RegistryEditRequest, RegistryListRequest, RegistryRemoveRequest,
    RegistryShowRequest,
};
use crate::render;

/// Parameters for constructing a `ProjectSpec` from CLI arguments.
// Five action booleans (iterate/maintain/push/audit/release) are inherent to the domain model.
#[allow(clippy::struct_excessive_bools)]
pub struct SpecArgs {
    pub name: String,
    pub path: String,
    pub stack: String,
    pub agent: String,
    pub repo: String,
    pub branch: String,
    pub iterate: bool,
    pub maintain: bool,
    pub push: bool,
    pub audit: bool,
    pub release: bool,
    pub install_command: Option<String>,
    pub install_brew: Option<String>,
    pub notes: Option<String>,
    pub timeout_secs: Option<u64>,
}

/// Parameters for constructing `ProjectEdits` from CLI arguments.
pub struct EditArgs {
    pub path: Option<String>,
    pub stack: Option<String>,
    pub agent: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub skip: Option<String>,
    pub iterate: Option<bool>,
    pub maintain: Option<bool>,
    pub push: Option<bool>,
    pub audit: Option<bool>,
    pub release: Option<bool>,
    pub install_command: Option<String>,
    pub install_brew: Option<String>,
    pub notes: Option<String>,
    pub timeout_secs: Option<u64>,
}

pub fn init(registry_path: &Path, offline: bool) -> Result<()> {
    if !offline {
        bail!("`foundry registry init` is an offline recovery command; rerun with `--offline`");
    }

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

pub async fn list(registry_path: &Path, addr: &str, offline: bool) -> Result<()> {
    if offline {
        return list_offline(registry_path);
    }

    let mut client = connect_daemon_required(addr, &registry_offline_hint("list")).await?;
    let response = client
        .registry_list(RegistryListRequest {})
        .await
        .map_err(status_to_anyhow)?
        .into_inner();

    render_project_table(&response.projects)
}

fn list_offline(registry_path: &Path) -> Result<()> {
    let registry = Registry::load(registry_path)?;

    if registry.projects.is_empty() {
        println!("No projects in registry.");
        return Ok(());
    }

    print!("{}", render::registry::project_table(&registry.projects));
    Ok(())
}

pub async fn show(registry_path: &Path, addr: &str, offline: bool, name: &str) -> Result<()> {
    if offline {
        return show_offline(registry_path, name);
    }

    let mut client =
        connect_daemon_required(addr, &registry_offline_hint(&format!("show {name}"))).await?;
    let response = client
        .registry_show(RegistryShowRequest {
            name: name.to_string(),
        })
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    let project = response
        .project
        .ok_or_else(|| anyhow::anyhow!("daemon returned no project for '{name}'"))?;

    print!("{}", render::registry::project_detail(&project_from_proto(&project)?));
    Ok(())
}

fn show_offline(registry_path: &Path, name: &str) -> Result<()> {
    let registry = Registry::load(registry_path)?;

    let Some(project) = registry.projects.iter().find(|p| p.name == name) else {
        bail!("Project '{name}' not found in registry");
    };

    print!("{}", render::registry::project_detail(project));

    Ok(())
}

pub async fn add_from_args(
    registry_path: &Path,
    addr: &str,
    offline: bool,
    args: SpecArgs,
) -> Result<()> {
    if offline {
        return add(registry_path, addr, true, spec_from_args(args)?).await;
    }

    let name = args.name.clone();
    let mut client =
        connect_daemon_required(addr, &registry_offline_hint(&format!("add --name {name}")))
            .await?;
    client
        .registry_add(RegistryAddRequest {
            name: args.name,
            path: args.path,
            stack: args.stack,
            agent: args.agent,
            repo: args.repo,
            branch: args.branch,
            iterate: args.iterate,
            maintain: args.maintain,
            push: args.push,
            audit: args.audit,
            release: args.release,
            install_command: args.install_command.unwrap_or_default(),
            install_brew: args.install_brew.unwrap_or_default(),
            notes: args.notes.unwrap_or_default(),
            timeout_secs: args.timeout_secs.unwrap_or(0),
        })
        .await
        .map_err(status_to_anyhow)?;
    println!("Added project '{name}' to registry.");
    Ok(())
}

pub async fn add(registry_path: &Path, addr: &str, offline: bool, spec: ProjectSpec) -> Result<()> {
    let name = spec.name.clone();
    if offline {
        add_offline(registry_path, spec)?;
        println!("Added project '{name}' to registry.");
        return Ok(());
    }

    let mut client =
        connect_daemon_required(addr, &registry_offline_hint(&format!("add --name {name}")))
            .await?;
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
    client.registry_add(req).await.map_err(status_to_anyhow)?;
    println!("Added project '{name}' to registry.");
    Ok(())
}

fn add_offline(registry_path: &Path, spec: ProjectSpec) -> Result<()> {
    let mut registry = load_or_init(registry_path)?;
    registry.add_project(spec).map_err(|e| anyhow::anyhow!("{e}"))?;
    registry.save(registry_path)?;
    Ok(())
}

pub async fn remove(registry_path: &Path, addr: &str, offline: bool, name: &str) -> Result<()> {
    if offline {
        remove_offline(registry_path, name)?;
        println!("Removed project '{name}' from registry.");
        return Ok(());
    }

    let mut client =
        connect_daemon_required(addr, &registry_offline_hint(&format!("remove {name}"))).await?;
    let req = RegistryRemoveRequest {
        name: name.to_string(),
    };
    client.registry_remove(req).await.map_err(status_to_anyhow)?;
    println!("Removed project '{name}' from registry.");
    Ok(())
}

fn remove_offline(registry_path: &Path, name: &str) -> Result<()> {
    let mut registry = Registry::load(registry_path)?;
    let before = registry.projects.len();
    registry.projects.retain(|p| p.name != name);
    if registry.projects.len() == before {
        bail!("Project '{name}' not found in registry");
    }
    registry.save(registry_path)?;
    Ok(())
}

pub async fn edit_from_args(
    registry_path: &Path,
    addr: &str,
    offline: bool,
    name: &str,
    args: EditArgs,
) -> Result<()> {
    if offline {
        return edit(registry_path, addr, true, name, edits_from_args(args)?).await;
    }

    let req = edit_request_from_args(name, &args);
    let mut client =
        connect_daemon_required(addr, &registry_offline_hint(&format!("edit {name}"))).await?;
    client.registry_edit(req).await.map_err(status_to_anyhow)?;
    println!("Updated project '{name}'.");
    Ok(())
}

pub async fn edit(
    registry_path: &Path,
    addr: &str,
    offline: bool,
    name: &str,
    edits: ProjectEdits,
) -> Result<()> {
    let req = edit_request(name, &edits);
    if offline {
        edit_offline(registry_path, name, edits)?;
        println!("Updated project '{name}'.");
        return Ok(());
    }

    let mut client =
        connect_daemon_required(addr, &registry_offline_hint(&format!("edit {name}"))).await?;
    client.registry_edit(req).await.map_err(status_to_anyhow)?;
    println!("Updated project '{name}'.");
    Ok(())
}

/// Build a `RegistryEditRequest` from a project name and its edit set.
fn edit_request(name: &str, edits: &ProjectEdits) -> RegistryEditRequest {
    let (skip_str, clear_skip) = match &edits.skip {
        None => (String::new(), false),
        Some(None) => (String::new(), true),
        Some(Some(reason)) => (reason.clone(), false),
    };
    let notes_str = edits.notes.as_deref().unwrap_or("").to_string();
    let clear_notes = edits.notes.as_deref().is_some_and(str::is_empty);
    RegistryEditRequest {
        name: name.to_string(),
        path: edits.path.clone().unwrap_or_default(),
        stack: edits.stack.as_ref().map(ToString::to_string).unwrap_or_default(),
        agent: edits.agent.clone().unwrap_or_default(),
        repo: edits.repo.clone().unwrap_or_default(),
        branch: edits.branch.clone().unwrap_or_default(),
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
        install_command: edits.install_command.clone().unwrap_or_default(),
        install_brew: edits.install_brew.clone().unwrap_or_default(),
        clear_install: edits.clear_install,
        notes: notes_str,
        clear_notes,
        timeout_secs: edits.timeout_secs.unwrap_or(0),
        clear_timeout: edits.clear_timeout,
    }
}

fn edit_request_from_args(name: &str, args: &EditArgs) -> RegistryEditRequest {
    let (skip_str, clear_skip) = match &args.skip {
        None => (String::new(), false),
        Some(skip) if skip.is_empty() => (String::new(), true),
        Some(skip) => (skip.clone(), false),
    };
    let notes_str = args.notes.as_deref().unwrap_or("").to_string();
    let clear_notes = args.notes.as_deref().is_some_and(str::is_empty);
    RegistryEditRequest {
        name: name.to_string(),
        path: args.path.clone().unwrap_or_default(),
        stack: args.stack.clone().unwrap_or_default(),
        agent: args.agent.clone().unwrap_or_default(),
        repo: args.repo.clone().unwrap_or_default(),
        branch: args.branch.clone().unwrap_or_default(),
        skip: skip_str,
        clear_skip,
        iterate: args.iterate.unwrap_or(false),
        clear_iterate: args.iterate.is_some_and(|value| !value),
        maintain: args.maintain.unwrap_or(false),
        clear_maintain: args.maintain.is_some_and(|value| !value),
        push: args.push.unwrap_or(false),
        clear_push: args.push.is_some_and(|value| !value),
        audit: args.audit.unwrap_or(false),
        clear_audit: args.audit.is_some_and(|value| !value),
        release: args.release.unwrap_or(false),
        clear_release: args.release.is_some_and(|value| !value),
        install_command: args.install_command.clone().unwrap_or_default(),
        install_brew: args.install_brew.clone().unwrap_or_default(),
        clear_install: false,
        notes: notes_str,
        clear_notes,
        timeout_secs: args.timeout_secs.unwrap_or(0),
        clear_timeout: false,
    }
}

fn edit_offline(registry_path: &Path, name: &str, edits: ProjectEdits) -> Result<()> {
    let mut registry = Registry::load(registry_path)?;
    registry.edit_project(name, edits).map_err(|e| anyhow::anyhow!("{e}"))?;
    registry.save(registry_path)?;
    Ok(())
}

/// Build a `ProjectSpec` from clap-parsed arguments.
pub fn spec_from_args(args: SpecArgs) -> Result<ProjectSpec> {
    let stack = parse_stack(&args.stack).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(ProjectSpec {
        name: args.name,
        path: args.path,
        stack,
        agent: args.agent,
        repo: args.repo,
        branch: args.branch,
        iterate: args.iterate,
        maintain: args.maintain,
        push: args.push,
        audit: args.audit,
        release: args.release,
        install_command: args.install_command,
        install_brew: args.install_brew,
        notes: args.notes,
        timeout_secs: args.timeout_secs,
    })
}

/// Build `ProjectEdits` from clap-parsed arguments.
pub fn edits_from_args(args: EditArgs) -> Result<ProjectEdits> {
    let stack = args
        .stack
        .as_deref()
        .map(parse_stack)
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let skip = args.skip.map(|s| if s.is_empty() { None } else { Some(s) });
    Ok(ProjectEdits {
        path: args.path,
        stack,
        agent: args.agent,
        repo: args.repo,
        branch: args.branch,
        skip,
        iterate: args.iterate,
        maintain: args.maintain,
        push: args.push,
        audit: args.audit,
        release: args.release,
        install_command: args.install_command,
        install_brew: args.install_brew,
        notes: args.notes,
        timeout_secs: args.timeout_secs,
        ..Default::default()
    })
}

/// Load an existing registry or create a new empty one if the file doesn't exist.
fn load_or_init(path: &Path) -> Result<Registry> {
    if path.exists() {
        Registry::load(path).map_err(Into::into)
    } else {
        Ok(Registry {
            version: 2,
            projects: vec![],
        })
    }
}

fn registry_offline_hint(command_suffix: &str) -> String {
    format!("foundry registry {command_suffix} --offline")
}

fn render_project_table(projects: &[Project]) -> Result<()> {
    if projects.is_empty() {
        println!("No projects in registry.");
        return Ok(());
    }

    let entries = projects.iter().map(project_from_proto).collect::<Result<Vec<_>>>()?;
    print!("{}", render::registry::project_table(&entries));
    Ok(())
}

fn project_from_proto(project: &Project) -> Result<ProjectEntry> {
    let stack = parse_stack(&project.stack).map_err(|e| anyhow::anyhow!("{e}"))?;
    let install = if !project.install_command.is_empty() {
        Some(InstallConfig::Command(project.install_command.clone()))
    } else if !project.install_brew.is_empty() {
        Some(InstallConfig::Brew(project.install_brew.clone()))
    } else {
        None
    };
    let installs_skill = if !project.installs_skill_command.is_empty() {
        Some(InstallsSkill::Custom {
            command: project.installs_skill_command.clone(),
        })
    } else if project.installs_skill_explicitly_disabled {
        Some(InstallsSkill::Default(false))
    } else if project.installs_skill_default {
        Some(InstallsSkill::Default(true))
    } else {
        None
    };

    Ok(ProjectEntry {
        name: project.name.clone(),
        path: project.path.clone(),
        stack,
        agent: project.agent.clone(),
        repo: project.repo.clone(),
        branch: project.branch.clone(),
        skip: if project.skip.is_empty() {
            None
        } else {
            Some(project.skip.clone())
        },
        actions: foundry_sdk::registry::ActionFlags {
            iterate: project.iterate,
            maintain: project.maintain,
            push: project.push,
            audit: project.audit,
            release: project.release,
        },
        install,
        installs_skill,
        notes: if project.notes.is_empty() {
            None
        } else {
            Some(project.notes.clone())
        },
        timeout_secs: if project.timeout_secs == 0 {
            None
        } else {
            Some(project.timeout_secs)
        },
        audit_exceptions: project.audit_exceptions.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::{EditArgs, SpecArgs, edit_request, edits_from_args, spec_from_args};
    use foundry_sdk::registry::ProjectEdits;

    // --- spec_from_args ---

    #[test]
    fn spec_from_args_parses_valid_stack() {
        let spec = spec_from_args(SpecArgs {
            name: "proj".to_string(),
            path: "/tmp/proj".to_string(),
            stack: "rust".to_string(),
            agent: "claude".to_string(),
            repo: "o/proj".to_string(),
            branch: "main".to_string(),
            iterate: false,
            maintain: false,
            push: false,
            audit: false,
            release: false,
            install_command: None,
            install_brew: None,
            notes: None,
            timeout_secs: None,
        })
        .expect("valid stack");
        assert_eq!(spec.stack.to_string(), "rust");
        assert_eq!(spec.name, "proj");
    }

    #[test]
    fn spec_from_args_returns_err_for_invalid_stack() {
        let result = spec_from_args(SpecArgs {
            name: "proj".to_string(),
            path: "/tmp/proj".to_string(),
            stack: "cobol".to_string(),
            agent: "claude".to_string(),
            repo: "o/proj".to_string(),
            branch: "main".to_string(),
            iterate: false,
            maintain: false,
            push: false,
            audit: false,
            release: false,
            install_command: None,
            install_brew: None,
            notes: None,
            timeout_secs: None,
        });
        assert!(result.is_err());
    }

    // --- edits_from_args ---

    #[test]
    fn edits_from_args_skip_none_yields_none() {
        let edits = edits_from_args(EditArgs {
            path: None,
            stack: None,
            agent: None,
            repo: None,
            branch: None,
            skip: None,
            iterate: None,
            maintain: None,
            push: None,
            audit: None,
            release: None,
            install_command: None,
            install_brew: None,
            notes: None,
            timeout_secs: None,
        })
        .expect("ok");
        assert!(edits.skip.is_none());
    }

    #[test]
    fn edits_from_args_skip_empty_string_clears() {
        let edits = edits_from_args(EditArgs {
            path: None,
            stack: None,
            agent: None,
            repo: None,
            branch: None,
            skip: Some(String::new()),
            iterate: None,
            maintain: None,
            push: None,
            audit: None,
            release: None,
            install_command: None,
            install_brew: None,
            notes: None,
            timeout_secs: None,
        })
        .expect("ok");
        assert_eq!(edits.skip, Some(None));
    }

    #[test]
    fn edits_from_args_skip_reason_sets_value() {
        let edits = edits_from_args(EditArgs {
            path: None,
            stack: None,
            agent: None,
            repo: None,
            branch: None,
            skip: Some("reason".to_string()),
            iterate: None,
            maintain: None,
            push: None,
            audit: None,
            release: None,
            install_command: None,
            install_brew: None,
            notes: None,
            timeout_secs: None,
        })
        .expect("ok");
        assert_eq!(edits.skip, Some(Some("reason".to_string())));
    }

    #[test]
    fn edits_from_args_invalid_stack_returns_err() {
        let result = edits_from_args(EditArgs {
            path: None,
            stack: Some("cobol".to_string()),
            agent: None,
            repo: None,
            branch: None,
            skip: None,
            iterate: None,
            maintain: None,
            push: None,
            audit: None,
            release: None,
            install_command: None,
            install_brew: None,
            notes: None,
            timeout_secs: None,
        });
        assert!(result.is_err());
    }

    #[test]
    fn edits_from_args_valid_stack_parses() {
        let edits = edits_from_args(EditArgs {
            path: None,
            stack: Some("python".to_string()),
            agent: None,
            repo: None,
            branch: None,
            skip: None,
            iterate: None,
            maintain: None,
            push: None,
            audit: None,
            release: None,
            install_command: None,
            install_brew: None,
            notes: None,
            timeout_secs: None,
        })
        .expect("valid stack");
        assert!(edits.stack.is_some());
    }

    // --- edit_request ---

    #[test]
    fn edit_request_skip_none_yields_empty_and_no_clear() {
        let edits = ProjectEdits {
            skip: None,
            ..Default::default()
        };
        let req = edit_request("p", &edits);
        assert_eq!(req.skip, "");
        assert!(!req.clear_skip);
    }

    #[test]
    fn edit_request_skip_some_none_sets_clear() {
        let edits = ProjectEdits {
            skip: Some(None),
            ..Default::default()
        };
        let req = edit_request("p", &edits);
        assert_eq!(req.skip, "");
        assert!(req.clear_skip);
    }

    #[test]
    fn edit_request_skip_some_reason_sets_value() {
        let edits = ProjectEdits {
            skip: Some(Some("archived".to_string())),
            ..Default::default()
        };
        let req = edit_request("p", &edits);
        assert_eq!(req.skip, "archived");
        assert!(!req.clear_skip);
    }

    #[test]
    fn edit_request_iterate_false_sets_clear_iterate() {
        let edits = ProjectEdits {
            iterate: Some(false),
            ..Default::default()
        };
        let req = edit_request("p", &edits);
        assert!(!req.iterate);
        assert!(req.clear_iterate);
    }

    #[test]
    fn edit_request_iterate_true_sets_field_not_clear() {
        let edits = ProjectEdits {
            iterate: Some(true),
            ..Default::default()
        };
        let req = edit_request("p", &edits);
        assert!(req.iterate);
        assert!(!req.clear_iterate);
    }
}
