use std::path::Path;
use std::sync::{Arc, RwLock};

use tonic::{Request, Response, Status};

use foundry_sdk::registry::{
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

pub(super) fn project_to_proto(entry: &foundry_sdk::registry::ProjectEntry) -> Project {
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

// Sequential field mapping from protobuf request to domain struct — length reflects field count.
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use tempfile::NamedTempFile;
    use tonic::Request;

    use foundry_sdk::registry::{
        ActionFlags, InstallConfig, ProjectEntry, Registry, RegistryMutationError, Stack,
    };

    use crate::proto::{RegistryAddRequest, RegistryEditRequest, RegistryRemoveRequest};

    use super::{add, edit, mutation_error_to_status, project_to_proto, remove};

    fn empty_registry() -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        }))
    }

    fn add_request(name: &str) -> Request<RegistryAddRequest> {
        Request::new(RegistryAddRequest {
            name: name.to_string(),
            path: format!("/tmp/{name}"),
            stack: "rust".to_string(),
            agent: "claude".to_string(),
            repo: format!("owner/{name}"),
            branch: "main".to_string(),
            iterate: true,
            maintain: false,
            push: false,
            audit: false,
            release: false,
            install_command: String::new(),
            install_brew: String::new(),
            notes: String::new(),
            timeout_secs: 0,
        })
    }

    fn tmp_path() -> (NamedTempFile, std::path::PathBuf) {
        let f = NamedTempFile::new().unwrap();
        let p = f.path().to_path_buf();
        (f, p)
    }

    #[test]
    fn mutation_error_already_exists_maps_to_already_exists_code() {
        let s = mutation_error_to_status(RegistryMutationError::DuplicateName("p".to_string()));
        assert_eq!(s.code(), tonic::Code::AlreadyExists);
        assert!(s.message().contains('p'));
    }

    #[test]
    fn mutation_error_not_found_maps_to_not_found_code() {
        let s = mutation_error_to_status(RegistryMutationError::NotFound("q".to_string()));
        assert_eq!(s.code(), tonic::Code::NotFound);
    }

    #[test]
    fn mutation_error_invalid_stack_maps_to_invalid_argument() {
        let s = mutation_error_to_status(RegistryMutationError::InvalidStack("bad".to_string()));
        assert_eq!(s.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn mutation_error_conflicting_install_maps_to_invalid_argument() {
        let s = mutation_error_to_status(RegistryMutationError::ConflictingInstall);
        assert_eq!(s.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn project_to_proto_no_install_defaults_to_empty_strings() {
        let entry = ProjectEntry {
            name: "alpha".to_string(),
            path: "/tmp/alpha".to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: "owner/alpha".to_string(),
            branch: "main".to_string(),
            skip: None,
            actions: ActionFlags::default(),
            install: None,
            installs_skill: None,
            notes: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        };
        let proto = project_to_proto(&entry);
        assert_eq!(proto.install_command, "");
        assert_eq!(proto.install_brew, "");
        assert_eq!(proto.skip, "");
        assert_eq!(proto.notes, "");
        assert_eq!(proto.timeout_secs, 0);
    }

    #[test]
    fn project_to_proto_command_install_splits_correctly() {
        let entry = ProjectEntry {
            name: "beta".to_string(),
            path: "/tmp/beta".to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            actions: ActionFlags::default(),
            install: Some(InstallConfig::Command("./install.sh".to_string())),
            installs_skill: None,
            notes: Some("a note".to_string()),
            timeout_secs: Some(120),
            audit_exceptions: Vec::new(),
        };
        let proto = project_to_proto(&entry);
        assert_eq!(proto.install_command, "./install.sh");
        assert_eq!(proto.install_brew, "");
        assert_eq!(proto.notes, "a note");
        assert_eq!(proto.timeout_secs, 120);
    }

    #[test]
    fn project_to_proto_brew_install_splits_correctly() {
        let entry = ProjectEntry {
            name: "gamma".to_string(),
            path: "/tmp/gamma".to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            actions: ActionFlags::default(),
            install: Some(InstallConfig::Brew("svetzal/tap/foundry".to_string())),
            installs_skill: None,
            notes: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        };
        let proto = project_to_proto(&entry);
        assert_eq!(proto.install_command, "");
        assert_eq!(proto.install_brew, "svetzal/tap/foundry");
    }

    #[test]
    fn add_inserts_project_and_returns_proto() {
        let reg = empty_registry();
        let (_f, path) = tmp_path();

        let resp = add(&reg, &path, add_request("proj-a")).expect("add should succeed");
        let proto = resp.into_inner().project.unwrap();

        assert_eq!(proto.name, "proj-a");
        assert!(proto.iterate);
        assert_eq!(reg.read().unwrap().projects.len(), 1);
    }

    #[test]
    fn add_with_conflicting_install_returns_invalid_argument() {
        let reg = empty_registry();
        let (_f, path) = tmp_path();
        let req = Request::new(RegistryAddRequest {
            name: "x".to_string(),
            path: "/tmp/x".to_string(),
            stack: "rust".to_string(),
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            iterate: false,
            maintain: false,
            push: false,
            audit: false,
            release: false,
            install_command: "cmd".to_string(),
            install_brew: "brew".to_string(),
            notes: String::new(),
            timeout_secs: 0,
        });
        let err = add(&reg, &path, req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn remove_deletes_project() {
        let reg = empty_registry();
        let (_f, path) = tmp_path();
        add(&reg, &path, add_request("to-rm")).unwrap();
        remove(
            &reg,
            &path,
            Request::new(RegistryRemoveRequest {
                name: "to-rm".to_string(),
            }),
        )
        .unwrap();
        assert!(reg.read().unwrap().projects.is_empty());
    }

    #[test]
    fn remove_unknown_project_returns_not_found() {
        let reg = empty_registry();
        let (_f, path) = tmp_path();
        let err = remove(
            &reg,
            &path,
            Request::new(RegistryRemoveRequest {
                name: "ghost".to_string(),
            }),
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[test]
    fn edit_updates_agent_field() {
        let reg = empty_registry();
        let (_f, path) = tmp_path();
        add(&reg, &path, add_request("editable")).unwrap();

        let resp = edit(
            &reg,
            &path,
            Request::new(RegistryEditRequest {
                name: "editable".to_string(),
                path: String::new(),
                stack: String::new(),
                agent: "gemini".to_string(),
                repo: String::new(),
                branch: String::new(),
                skip: String::new(),
                clear_skip: false,
                iterate: false,
                clear_iterate: false,
                maintain: false,
                clear_maintain: false,
                push: false,
                clear_push: false,
                audit: false,
                clear_audit: false,
                release: false,
                clear_release: false,
                install_command: String::new(),
                install_brew: String::new(),
                clear_install: false,
                notes: String::new(),
                clear_notes: false,
                timeout_secs: 0,
                clear_timeout: false,
            }),
        )
        .unwrap();

        let proto = resp.into_inner().project.unwrap();
        assert_eq!(proto.agent, "gemini");
    }

    #[test]
    fn edit_unknown_project_returns_not_found() {
        let reg = empty_registry();
        let (_f, path) = tmp_path();
        let err = edit(
            &reg,
            &path,
            Request::new(RegistryEditRequest {
                name: "ghost".to_string(),
                path: String::new(),
                stack: String::new(),
                agent: String::new(),
                repo: String::new(),
                branch: String::new(),
                skip: String::new(),
                clear_skip: false,
                iterate: false,
                clear_iterate: false,
                maintain: false,
                clear_maintain: false,
                push: false,
                clear_push: false,
                audit: false,
                clear_audit: false,
                release: false,
                clear_release: false,
                install_command: String::new(),
                install_brew: String::new(),
                clear_install: false,
                notes: String::new(),
                clear_notes: false,
                timeout_secs: 0,
                clear_timeout: false,
            }),
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }
}
