use std::path::Path;

use anyhow::{Result, bail};
use foundry_core::registry::{ActionFlags, InstallConfig, ProjectEntry, Registry, Stack};

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

    println!("{:<20} {:<12} {:<6} ACTIONS", "NAME", "STACK", "SKIP");
    println!("{}", "-".repeat(70));

    for p in &registry.projects {
        let skip = if p.skip.unwrap_or(false) { "yes" } else { "no" };
        let actions = format_actions(&p.actions);
        println!("{:<20} {:<12} {:<6} {}", p.name, p.stack, skip, actions);
    }

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
    println!(
        "Skip:      {}",
        if project.skip.unwrap_or(false) {
            "yes"
        } else {
            "no"
        }
    );
    println!("Actions:   {}", format_actions(&project.actions));

    if let Some(ref install) = project.install {
        match install {
            InstallConfig::Command(cmd) => println!("Install:   command: {cmd}"),
            InstallConfig::Brew(formula) => println!("Install:   brew: {formula}"),
        }
    }

    if let Some(timeout) = project.timeout_secs {
        println!("Timeout:   {timeout}s");
    } else {
        println!("Timeout:   1800s (default)");
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
        actions: ActionFlags {
            iterate,
            maintain,
            push,
            audit,
            release,
        },
        install,
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
    skip: Option<bool>,
    iterate: Option<bool>,
    maintain: Option<bool>,
    push: Option<bool>,
    audit_flag: Option<bool>,
    release: Option<bool>,
    install_command: Option<&str>,
    install_brew: Option<&str>,
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
        project.skip = Some(v);
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
    if let Some(v) = timeout_secs {
        project.timeout_secs = Some(v);
    }

    registry.save(registry_path)?;
    println!("Updated project '{name}'.");
    Ok(())
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
