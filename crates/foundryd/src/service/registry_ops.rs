use std::path::Path;
use std::sync::{Arc, RwLock};

use tonic::{Request, Response, Status};

use foundry_core::registry::{
    InstallConfig, InstallsSkill, ProjectEdits, ProjectSpec, Registry, RegistryMutationError,
    parse_stack,
};

use crate::proto::{
    Project, RegistryAddRequest, RegistryAddResponse, RegistryEditRequest, RegistryEditResponse,
    RegistryRemoveRequest, RegistryRemoveResponse,
};

fn mutation_error_to_status(err: RegistryMutationError) -> Status {
    match err {
        RegistryMutationError::DuplicateName(name) => {
            Status::already_exists(format!("project '{name}' already exists"))
        }
        RegistryMutationError::NotFound(name) => {
            Status::not_found(format!("project '{name}' not found"))
        }
        RegistryMutationError::InvalidStack(s) => {
            Status::invalid_argument(format!("invalid stack '{s}'"))
        }
        RegistryMutationError::ConflictingInstall => {
            Status::invalid_argument("provide at most one of install_command or install_brew")
        }
    }
}

pub(super) fn project_to_proto(entry: &foundry_core::registry::ProjectEntry) -> Project {
    let (install_command, install_brew) = match &entry.install {
        Some(InstallConfig::Command(cmd)) => (cmd.clone(), String::new()),
        Some(InstallConfig::Brew(formula)) => (String::new(), formula.clone()),
        None => (String::new(), String::new()),
    };
    let (installs_skill_bool, installs_skill_command) = match &entry.installs_skill {
        Some(InstallsSkill::Default(true)) => (true, String::new()),
        Some(InstallsSkill::Custom { command }) => (false, command.clone()),
        _ => (false, String::new()),
    };
    let _ = installs_skill_bool; // not in proto yet — silence lint
    let _ = installs_skill_command;
    Project {
        name: entry.name.clone(),
        path: entry.path.clone(),
        stack: entry.stack.to_string(),
        agent: entry.agent.clone(),
        repo: entry.repo.clone(),
        branch: entry.branch.clone(),
        skip: entry.skip.clone().unwrap_or_default(),
        iterate: entry.actions.iterate,
        maintain: entry.actions.maintain,
        push: entry.actions.push,
        audit: entry.actions.audit,
        release: entry.actions.release,
        install_command,
        install_brew,
        notes: entry.notes.clone().unwrap_or_default(),
        timeout_secs: entry.timeout_secs.unwrap_or(0),
    }
}

pub(super) fn add(
    registry: &Arc<RwLock<Registry>>,
    registry_path: &Path,
    request: Request<RegistryAddRequest>,
) -> Result<Response<RegistryAddResponse>, Status> {
    let req = request.into_inner();

    let stack = parse_stack(if req.stack.is_empty() {
        "rust"
    } else {
        &req.stack
    })
    .map_err(mutation_error_to_status)?;

    let branch = if req.branch.is_empty() {
        "main".to_string()
    } else {
        req.branch.clone()
    };

    // Validate mutual exclusivity client-side before calling add_project.
    if !req.install_command.is_empty() && !req.install_brew.is_empty() {
        return Err(mutation_error_to_status(RegistryMutationError::ConflictingInstall));
    }

    let spec = ProjectSpec {
        name: req.name.clone(),
        path: req.path.clone(),
        stack,
        agent: req.agent.clone(),
        repo: req.repo.clone(),
        branch,
        iterate: req.iterate,
        maintain: req.maintain,
        push: req.push,
        audit: req.audit,
        release: req.release,
        install_command: if req.install_command.is_empty() {
            None
        } else {
            Some(req.install_command.clone())
        },
        install_brew: if req.install_brew.is_empty() {
            None
        } else {
            Some(req.install_brew.clone())
        },
        notes: if req.notes.is_empty() {
            None
        } else {
            Some(req.notes.clone())
        },
        timeout_secs: if req.timeout_secs == 0 {
            None
        } else {
            Some(req.timeout_secs)
        },
    };

    let entry_proto = {
        let mut reg = registry.write().expect("registry lock poisoned");
        let entry = reg.add_project(spec).map_err(mutation_error_to_status)?;
        let proto = project_to_proto(entry);
        reg.save(registry_path)
            .map_err(|e| Status::internal(format!("failed to save registry: {e}")))?;
        proto
    };

    tracing::info!(project = %req.name, "registry_add: project added");

    Ok(Response::new(RegistryAddResponse {
        project: Some(entry_proto),
    }))
}

pub(super) fn remove(
    registry: &Arc<RwLock<Registry>>,
    registry_path: &Path,
    request: Request<RegistryRemoveRequest>,
) -> Result<Response<RegistryRemoveResponse>, Status> {
    let req = request.into_inner();

    {
        let mut reg = registry.write().expect("registry lock poisoned");
        reg.remove_project(&req.name).map_err(mutation_error_to_status)?;
        reg.save(registry_path)
            .map_err(|e| Status::internal(format!("failed to save registry: {e}")))?;
    }

    tracing::info!(project = %req.name, "registry_remove: project removed");

    Ok(Response::new(RegistryRemoveResponse {}))
}

#[allow(clippy::too_many_lines)]
pub(super) fn edit(
    registry: &Arc<RwLock<Registry>>,
    registry_path: &Path,
    request: Request<RegistryEditRequest>,
) -> Result<Response<RegistryEditResponse>, Status> {
    let req = request.into_inner();

    let stack = if req.stack.is_empty() {
        None
    } else {
        Some(parse_stack(&req.stack).map_err(mutation_error_to_status)?)
    };

    // Validate mutual exclusivity before building edits.
    if !req.install_command.is_empty() && !req.install_brew.is_empty() {
        return Err(mutation_error_to_status(RegistryMutationError::ConflictingInstall));
    }

    let skip = if req.clear_skip {
        Some(None)
    } else if req.skip.is_empty() {
        None
    } else {
        Some(Some(req.skip.clone()))
    };

    let edits = ProjectEdits {
        path: if req.path.is_empty() {
            None
        } else {
            Some(req.path.clone())
        },
        stack,
        agent: if req.agent.is_empty() {
            None
        } else {
            Some(req.agent.clone())
        },
        repo: if req.repo.is_empty() {
            None
        } else {
            Some(req.repo.clone())
        },
        branch: if req.branch.is_empty() {
            None
        } else {
            Some(req.branch.clone())
        },
        skip,
        iterate: if req.clear_iterate {
            Some(false)
        } else if req.iterate {
            Some(true)
        } else {
            None
        },
        maintain: if req.clear_maintain {
            Some(false)
        } else if req.maintain {
            Some(true)
        } else {
            None
        },
        push: if req.clear_push {
            Some(false)
        } else if req.push {
            Some(true)
        } else {
            None
        },
        audit: if req.clear_audit {
            Some(false)
        } else if req.audit {
            Some(true)
        } else {
            None
        },
        release: if req.clear_release {
            Some(false)
        } else if req.release {
            Some(true)
        } else {
            None
        },
        install_command: if req.install_command.is_empty() {
            None
        } else {
            Some(req.install_command.clone())
        },
        install_brew: if req.install_brew.is_empty() {
            None
        } else {
            Some(req.install_brew.clone())
        },
        clear_install: req.clear_install,
        // notes: empty string → clear (edit_project treats "" as "unset")
        notes: if req.clear_notes {
            Some(String::new())
        } else if req.notes.is_empty() {
            None
        } else {
            Some(req.notes.clone())
        },
        timeout_secs: if req.timeout_secs == 0 {
            None
        } else {
            Some(req.timeout_secs)
        },
        clear_timeout: req.clear_timeout,
    };

    let entry_proto = {
        let mut reg = registry.write().expect("registry lock poisoned");
        let entry = reg.edit_project(&req.name, edits).map_err(mutation_error_to_status)?;
        let proto = project_to_proto(entry);
        reg.save(registry_path)
            .map_err(|e| Status::internal(format!("failed to save registry: {e}")))?;
        proto
    };

    tracing::info!(project = %req.name, "registry_edit: project updated");

    Ok(Response::new(RegistryEditResponse {
        project: Some(entry_proto),
    }))
}
