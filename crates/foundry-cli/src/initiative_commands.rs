//! Initiative commands use typed RPCs unless recovery explicitly selects offline.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Subcommand;
use foundry_sdk::campaign::CampaignStore;
use foundry_sdk::initiative::{Finding, Initiative};

use crate::daemon::{connect_daemon_required, status_to_anyhow};
use crate::{proto, render};

#[derive(Subcommand)]
pub enum Commands {
    /// Add a definition \[mutates\] (example: foundry initiative add initiative.json)
    Add { file: PathBuf },
    /// List initiatives (example: foundry initiative list --json)
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show the outcome, members and questions (example: foundry initiative show shared)
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Request one outcome assessment through the daemon \[mutates\]
    Advance {
        name: String,
        /// Pin this steer to a member and release its owner hold
        #[arg(long)]
        member: Option<String>,
        #[arg(long)]
        origin: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Record binding policy; answer only named --question IDs \[mutates\]
    Decide {
        name: String,
        #[arg(long)]
        decision: String,
        /// Answer this exact question ID (repeatable); omit for policy only
        #[arg(long = "question")]
        question_ids: Vec<String>,
        #[arg(long = "member")]
        members: Vec<String>,
        /// Extend the shared initiative budget by this many cycles; required
        /// (> 0) once the shared budget is spent
        #[arg(long, default_value_t = 0)]
        add_cycles: u64,
    },
    /// Record nonbinding status without answering questions or resuming work \[mutates\]
    /// (example: foundry initiative note shared --text "Mac verification landed")
    Note {
        name: String,
        #[arg(long)]
        text: String,
        #[arg(long)]
        json: bool,
    },
    /// Reassess an operational blocker from new evidence \[mutates\]
    /// (example: `foundry initiative recover shared --recovery recovery_123 --evidence "Worker settled"`)
    Recover {
        name: String,
        #[arg(long = "recovery")]
        recovery_id: String,
        #[arg(long, required = true)]
        evidence: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Hold a member until an owner release or named milestones \[mutates\]
    Hold {
        name: String,
        #[arg(long)]
        member: String,
        #[arg(long = "until")]
        until: Vec<String>,
        #[arg(long)]
        reason: String,
    },
    /// Relay an existing finding to members \[mutates\]
    Relay {
        name: String,
        #[arg(long)]
        finding: String,
        #[arg(long = "member", required = true)]
        members: Vec<String>,
    },
    /// Dispatch one task through initiative budget and pacing \[mutates\]
    Dispatch {
        name: String,
        project: String,
        objective: String,
        #[arg(long)]
        member: Option<String>,
        #[arg(long)]
        origin: Option<String>,
    },
    /// Record one owner question covering applicable members \[mutates\]
    Ask {
        name: String,
        #[arg(long)]
        question: String,
        #[arg(long = "member")]
        members: Vec<String>,
    },
    /// Record owner-verified evidence for a member milestone \[mutates\]
    Milestone {
        name: String,
        #[arg(long)]
        member: String,
        #[arg(long)]
        milestone: String,
        #[arg(long = "evidence", required = true)]
        evidence: Vec<String>,
    },
    /// Confirm reviewed outcome evidence and close linked Tracker intent \[mutates\]
    Complete {
        name: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        json: bool,
    },
    /// Stop an initiative \[mutates\]
    Cancel {
        name: String,
        #[arg(long)]
        reason: String,
    },
    /// Read or replace the Foundry-owned brief
    #[command(subcommand)]
    Brief(BriefCommands),
    /// Derive staged candidates from Tracker without changing Tracker \[mutates Foundry\]
    Derive {
        #[arg(long = "from")]
        from_ref: String,
        #[arg(long, value_delimiter = ',')]
        projects: Vec<String>,
        /// Owner authorizing this initiative (default: the invoking user)
        #[arg(long)]
        authorized_by: Option<String>,
        /// JSON array of integration gate declarations to add to Tracker reviews
        #[arg(long)]
        integration_gates: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum BriefCommands {
    /// Store a brief \[mutates\] (example: foundry initiative brief set shared BRIEF.md)
    Set { name: String, file: PathBuf },
    /// Read the authoritative brief (example: foundry initiative brief show shared)
    Show { name: String },
}

#[derive(Subcommand)]
pub enum FindingCommands {
    /// Record a finding definition \[mutates\] (example: foundry finding add finding.json)
    Add { file: PathBuf },
    /// List findings newest first (example: foundry finding list shared --member caller)
    List {
        initiative: String,
        #[arg(long)]
        member: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

fn json<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(value)?))
}

pub(crate) fn to_proto(
    store: &CampaignStore,
    initiative: &Initiative,
) -> Result<proto::Initiative> {
    let mut value = serde_json::to_value(initiative)?;
    value["steering_history_count"] = serde_json::json!(initiative.steering_history.len());
    value["steering_history"] =
        serde_json::to_value(initiative.steering_history.last().into_iter().collect::<Vec<_>>())?;
    for field in ["steering_history", "pending_actions"] {
        if let Some(records) = value[field].as_array_mut() {
            for record in records {
                record["decision_json"] =
                    serde_json::Value::String(serde_json::to_string(&record["decision"])?);
                record.as_object_mut().context("invalid steering record")?.remove("decision");
                if let Some(cause) = record
                    .as_object_mut()
                    .context("invalid steering record")?
                    .remove("follow_up_of")
                {
                    record["follow_up_of_json"] =
                        serde_json::Value::String(serde_json::to_string(&cause)?);
                }
                if let Some(context) = record
                    .as_object_mut()
                    .context("invalid steering record")?
                    .remove("external_context")
                {
                    record["external_context_json"] = if context.is_null() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(serde_json::to_string(&context)?)
                    };
                }
            }
        }
    }
    if let Some(members) = value["members"].as_array_mut() {
        for member in members {
            if let Some(campaign) = member["name"].as_str().and_then(|name| store.find(name)) {
                member["campaign"] =
                    serde_json::to_value(crate::campaign_commands::summary_to_proto(campaign))?;
                member["project"] = serde_json::json!(campaign.project);
                if let Some(verdict) =
                    campaign.pending_run_result.as_ref().map(|result| &result.verdict).or_else(
                        || {
                            campaign.objective_history.iter().rev().find_map(|cycle| {
                                cycle.outcome.as_ref().map(|outcome| &outcome.verdict)
                            })
                        },
                    )
                {
                    member["last_verdict"] = serde_json::json!(verdict.tag());
                    member["reviewer_gaps"] = match verdict {
                        foundry_sdk::payload::TaskVerdict::Remainder { gaps } => {
                            serde_json::json!(gaps)
                        }
                        foundry_sdk::payload::TaskVerdict::Defect { diagnosis } => {
                            serde_json::json!([diagnosis])
                        }
                        _ => serde_json::json!([]),
                    };
                }
            }
        }
    }
    Ok(serde_json::from_value(value)?)
}

pub(crate) fn list_offline(path: &Path) -> Result<Vec<proto::Initiative>> {
    let store = CampaignStore::load(path)?;
    store
        .initiatives
        .iter()
        .map(|initiative| to_proto(&store, &store.initiative_view(&initiative.name)?))
        .collect()
}

pub async fn run(
    command: Commands,
    store_path: &Path,
    registry_path: &Path,
    addr: &str,
    offline: bool,
) -> Result<()> {
    let output = run_and_render(command, store_path, registry_path, addr, offline).await?;
    print!("{output}");
    Ok(())
}

pub async fn run_and_render(
    command: Commands,
    store_path: &Path,
    registry_path: &Path,
    addr: &str,
    offline: bool,
) -> Result<String> {
    if let Commands::Recover {
        name,
        recovery_id,
        evidence,
        json,
    } = command
    {
        return crate::recovery_commands::recover(
            addr,
            offline,
            crate::recovery_commands::Scope::Initiative,
            &name,
            &recovery_id,
            evidence,
            json,
        )
        .await;
    }
    if offline {
        return run_offline(command, store_path, registry_path);
    }
    run_online(command, addr).await
}

async fn decide_online(
    client: &mut proto::foundry_client::FoundryClient<tonic::transport::Channel>,
    request: proto::DecideInitiativeRequest,
) -> Result<String> {
    let initiative = client
        .record_initiative_decision(request)
        .await
        .map_err(|status| {
            if status.code() == tonic::Code::Unimplemented {
                anyhow::anyhow!("foundryd does not support question-specific initiative decisions; update the daemon before using initiative decide. No legacy decision was sent.")
            } else { status_to_anyhow(status) }
        })?
        .into_inner()
        .initiative
        .context("daemon returned no initiative")?;
    Ok(render::initiative::decision_recorded(&initiative))
}

async fn note_online(
    client: &mut proto::foundry_client::FoundryClient<tonic::transport::Channel>,
    name: String,
    text: String,
    as_json: bool,
) -> Result<String> {
    let initiative = client
        .record_initiative_status(proto::RecordInitiativeStatusRequest { name, text })
        .await
        .map_err(status_to_anyhow)?
        .into_inner()
        .initiative
        .context("daemon returned no initiative")?;
    if as_json {
        json(&initiative)
    } else {
        Ok(render::initiative::status_recorded(&initiative.name))
    }
}

async fn run_online(command: Commands, addr: &str) -> Result<String> {
    if let Commands::Brief(command) = command {
        return brief_online(command, addr).await;
    }
    let mut client =
        connect_daemon_required(addr, "foundry initiative --offline <command>").await?;
    match command {
        Commands::Add { file } => {
            let definition = std::fs::read_to_string(&file)
                .with_context(|| format!("read initiative definition {}", file.display()))?;
            add_definition(
                std::path::Path::new(""),
                std::path::Path::new(""),
                addr,
                false,
                &definition,
            )
            .await
        }
        Commands::List { json: as_json } => {
            let initiatives = client
                .list_initiatives(proto::ListInitiativesRequest {})
                .await
                .map_err(status_to_anyhow)?
                .into_inner()
                .initiatives;
            if as_json {
                json(&initiatives)
            } else {
                Ok(render::initiative::list(&initiatives))
            }
        }
        Commands::Show {
            name,
            json: as_json,
        } => {
            let initiative = client
                .get_initiative(proto::GetInitiativeRequest { name })
                .await
                .map_err(status_to_anyhow)?
                .into_inner()
                .initiative
                .context("daemon returned no initiative")?;
            if as_json {
                json(&initiative)
            } else {
                Ok(render::initiative::detail(&initiative))
            }
        }
        Commands::Decide {
            name,
            decision,
            members,
            add_cycles,
            question_ids,
        } => {
            decide_online(
                &mut client,
                proto::DecideInitiativeRequest {
                    name,
                    decision,
                    members,
                    add_cycles,
                    question_ids,
                },
            )
            .await
        }
        Commands::Note {
            name,
            text,
            json: as_json,
        } => note_online(&mut client, name, text, as_json).await,
        command @ (Commands::Hold { .. }
        | Commands::Relay { .. }
        | Commands::Dispatch { .. }
        | Commands::Ask { .. }
        | Commands::Milestone { .. }) => owner_command_online(command, &mut client).await,
        Commands::Complete {
            name,
            reason,
            json: as_json,
        } => complete_online(&mut client, name, reason, as_json).await,
        Commands::Cancel { name, reason } => {
            client
                .cancel_initiative(proto::EndInitiativeRequest {
                    name: name.clone(),
                    reason,
                })
                .await
                .map_err(status_to_anyhow)?;
            Ok(format!("Cancelled initiative '{name}'.\n"))
        }
        Commands::Brief(_) => unreachable!("brief handled before other online commands"),
        Commands::Recover { .. } => unreachable!("recovery handled before transport selection"),
        command @ (Commands::Advance { .. } | Commands::Derive { .. }) => {
            steering_command_online(command, &mut client).await
        }
    }
}

async fn complete_online(
    client: &mut proto::foundry_client::FoundryClient<tonic::transport::Channel>,
    name: String,
    reason: String,
    as_json: bool,
) -> Result<String> {
    let initiative = client
        .complete_initiative(proto::EndInitiativeRequest { name, reason })
        .await
        .map_err(status_to_anyhow)?
        .into_inner()
        .initiative
        .context("daemon returned no initiative")?;
    if as_json {
        json(&initiative)
    } else {
        Ok(render::initiative::completion(&initiative))
    }
}

async fn owner_command_online(
    command: Commands,
    client: &mut proto::foundry_client::FoundryClient<tonic::transport::Channel>,
) -> Result<String> {
    match command {
        Commands::Hold {
            name,
            member,
            until,
            reason,
        } => {
            owner_action(
                client,
                name,
                proto::act_on_initiative_request::Action::Hold(proto::HoldInitiativeMember {
                    member,
                    until,
                    reason,
                }),
                None,
            )
            .await
        }
        Commands::Relay {
            name,
            finding,
            members,
        } => {
            owner_action(
                client,
                name,
                proto::act_on_initiative_request::Action::Relay(proto::RelayInitiativeFinding {
                    finding_id: finding,
                    members,
                }),
                None,
            )
            .await
        }
        Commands::Dispatch {
            name,
            project,
            objective,
            member,
            origin,
        } => {
            owner_action(
                client,
                name,
                proto::act_on_initiative_request::Action::Dispatch(proto::DispatchInitiativeTask {
                    project,
                    objective,
                    member,
                }),
                origin,
            )
            .await
        }
        Commands::Ask {
            name,
            question,
            members,
        } => {
            owner_action(
                client,
                name,
                proto::act_on_initiative_request::Action::Ask(proto::AskInitiativeOwner {
                    question,
                    members,
                }),
                None,
            )
            .await
        }
        Commands::Milestone {
            name,
            member,
            milestone,
            evidence,
        } => {
            owner_action(
                client,
                name,
                proto::act_on_initiative_request::Action::Milestone(
                    proto::RecordInitiativeMilestone {
                        member,
                        milestone,
                        evidence,
                    },
                ),
                None,
            )
            .await
        }
        _ => unreachable!("only owner commands are routed here"),
    }
}
async fn steering_command_online(
    command: Commands,
    client: &mut proto::foundry_client::FoundryClient<tonic::transport::Channel>,
) -> Result<String> {
    match command {
        Commands::Advance {
            name,
            member,
            origin,
            json: as_json,
        } => {
            let response = client
                .advance_initiative(proto::AdvanceInitiativeRequest {
                    name: name.clone(),
                    member,
                    operator_origin: crate::origin::local_operator_origin(origin.as_deref()),
                })
                .await
                .map_err(status_to_anyhow)?
                .into_inner();
            if as_json {
                return json(&response);
            }
            Ok(render::initiative::advance(&name, &response))
        }
        Commands::Derive {
            from_ref,
            projects,
            authorized_by,
            integration_gates,
        } => {
            let source_repo =
                std::env::current_dir().context("cannot locate Tracker source repository")?;
            let response = client
                .derive_initiative(proto::DeriveInitiativeRequest {
                    from_ref,
                    projects,
                    authorized_by: authorized_by
                        .or_else(|| std::env::var("USER").ok())
                        .context("set --authorized-by to name the initiative owner")?,
                    source_repo: source_repo.to_string_lossy().into_owned(),
                    integration_gates: read_integration_gates(integration_gates.as_deref())?,
                })
                .await
                .map_err(status_to_anyhow)?
                .into_inner();
            let initiative = response.initiative.context("daemon returned no initiative")?;
            Ok(format!(
                "Derived initiative '{}'. First advance audits its candidates.\n",
                initiative.name
            ))
        }
        _ => unreachable!("only steering commands are routed here"),
    }
}

fn read_integration_gates(path: Option<&Path>) -> Result<Vec<proto::InitiativeEvidence>> {
    use foundry_sdk::initiative::DoneEvidence;
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let evidence: Vec<DoneEvidence> = serde_json::from_str(
        &std::fs::read_to_string(path)
            .with_context(|| format!("read integration gates {}", path.display()))?,
    )
    .with_context(|| format!("parse integration gates {}", path.display()))?;
    evidence.into_iter().map(|evidence| match evidence {
        DoneEvidence::Gate { command, host, required, artifacts } => {
            anyhow::ensure!(!command.trim().is_empty(), "an integration gate requires a command");
            Ok(proto::InitiativeEvidence { kind:"gate".into(), command, host, required, artifacts, statement:String::new() })
        },
        DoneEvidence::Review { .. } => anyhow::bail!("--integration-gates accepts only gate declarations; Tracker supplies outcome reviews"),
    }).collect()
}

async fn owner_action(
    client: &mut proto::foundry_client::FoundryClient<tonic::transport::Channel>,
    name: String,
    action: proto::act_on_initiative_request::Action,
    origin: Option<String>,
) -> Result<String> {
    let response = client
        .act_on_initiative(proto::ActOnInitiativeRequest {
            name: name.clone(),
            operator_origin: crate::origin::local_operator_origin(origin.as_deref()),
            action: Some(action),
        })
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    Ok(render::initiative::owner_action(&name, &response.event_id))
}

async fn brief_online(command: BriefCommands, addr: &str) -> Result<String> {
    let mut client =
        connect_daemon_required(addr, "foundry initiative --offline <command>").await?;
    match command {
        BriefCommands::Set { name, file } => {
            let content = std::fs::read_to_string(&file)
                .with_context(|| format!("read brief {}", file.display()))?;
            let brief = client
                .set_initiative_brief(proto::SetInitiativeBriefRequest {
                    name: name.clone(),
                    content,
                })
                .await
                .map_err(status_to_anyhow)?
                .into_inner()
                .brief
                .context("daemon returned no brief")?;
            Ok(format!(
                "Stored brief for '{name}' at {} (sha256 {}).\n",
                brief.path, brief.hash
            ))
        }
        BriefCommands::Show { name } => Ok(client
            .get_initiative_brief(proto::GetInitiativeRequest { name })
            .await
            .map_err(status_to_anyhow)?
            .into_inner()
            .content),
    }
}

async fn add_definition(
    path: &Path,
    registry: &Path,
    addr: &str,
    offline: bool,
    definition: &str,
) -> Result<String> {
    if offline {
        let initiative: Initiative = serde_json::from_str(definition)?;
        let name = initiative.name.clone();
        let projects = foundry_sdk::registry::Registry::load(registry)?
            .projects
            .into_iter()
            .map(|project| project.name)
            .collect::<Vec<_>>();
        let mut guard = CampaignStore::lock_exclusive(path)?;
        guard.store.add_initiative(initiative, &projects)?;
        guard.save()?;
        Ok(format!("Added initiative '{name}'.\n"))
    } else {
        let mut client =
            connect_daemon_required(addr, "foundry --offline initiative add <definition.json>")
                .await?;
        let initiative = client
            .add_initiative(proto::AddInitiativeRequest {
                definition_json: definition.to_string(),
            })
            .await
            .map_err(status_to_anyhow)?
            .into_inner()
            .initiative
            .context("daemon returned no initiative")?;
        Ok(format!("Added initiative '{}'.\n", initiative.name))
    }
}

fn run_offline(command: Commands, path: &Path, registry: &Path) -> Result<String> {
    match command {
        Commands::Advance { .. }
        | Commands::Complete { .. }
        | Commands::Derive { .. }
        | Commands::Hold { .. }
        | Commands::Relay { .. }
        | Commands::Dispatch { .. }
        | Commands::Ask { .. }
        | Commands::Milestone { .. } => {
            anyhow::bail!("initiative steering and derivation require the daemon; remove --offline")
        }
        Commands::List { json: as_json } => {
            let initiatives = list_offline(path)?;
            if as_json {
                json(&initiatives)
            } else {
                Ok(render::initiative::list(&initiatives))
            }
        }
        Commands::Show {
            name,
            json: as_json,
        } => {
            let store = CampaignStore::load(path)?;
            let initiative = to_proto(&store, &store.initiative_view(&name)?)?;
            if as_json {
                json(&initiative)
            } else {
                Ok(render::initiative::detail(&initiative))
            }
        }
        Commands::Brief(BriefCommands::Show { name }) => {
            Ok(CampaignStore::load(path)?.show_brief(&name)?)
        }
        command => {
            let mut guard = CampaignStore::lock_exclusive(path)?;
            let output = match command {
                Commands::Add { file } => {
                    let initiative: Initiative =
                        serde_json::from_str(&std::fs::read_to_string(file)?)?;
                    let name = initiative.name.clone();
                    let projects = foundry_sdk::registry::Registry::load(registry)?
                        .projects
                        .into_iter()
                        .map(|project| project.name)
                        .collect::<Vec<_>>();
                    guard.store.add_initiative(initiative, &projects)?;
                    format!("Added initiative '{name}'.\n")
                }
                Commands::Decide {
                    name,
                    decision,
                    members,
                    add_cycles,
                    question_ids,
                } => {
                    guard.store.decide_initiative_answering_with_budget(
                        &name,
                        &decision,
                        &members,
                        &question_ids,
                        add_cycles,
                        Utc::now(),
                    )?;
                    render::initiative::decision_recorded(&to_proto(
                        &guard.store,
                        &guard.store.initiative_view(&name)?,
                    )?)
                }
                Commands::Note {
                    name,
                    text,
                    json: as_json,
                } => {
                    guard.store.record_initiative_status(&name, &text, Utc::now())?;
                    if as_json {
                        json(&to_proto(&guard.store, &guard.store.initiative_view(&name)?)?)?
                    } else {
                        render::initiative::status_recorded(&name)
                    }
                }
                Commands::Cancel { name, reason } => {
                    let _transition = guard.store.cancel_initiative(&name, &reason, Utc::now())?;
                    format!("Cancelled initiative '{name}'.\n")
                }
                Commands::Brief(BriefCommands::Set { name, file }) => {
                    let home = path.parent().context("campaign store has no home directory")?;
                    let brief =
                        guard.set_initiative_brief(&name, home, &std::fs::read_to_string(file)?)?;
                    return Ok(format!(
                        "Stored brief for '{name}' at {} (sha256 {}).\n",
                        brief.path, brief.hash
                    ));
                }
                _ => unreachable!("read and derive commands handled earlier"),
            };
            guard.save()?;
            Ok(output)
        }
    }
}

pub async fn finding(
    command: FindingCommands,
    path: &Path,
    addr: &str,
    offline: bool,
) -> Result<()> {
    let output = if offline {
        match command {
            FindingCommands::Add { file } => {
                let finding: Finding = serde_json::from_str(&std::fs::read_to_string(file)?)?;
                let mut guard = CampaignStore::lock_exclusive(path)?;
                let finding = guard.store.add_finding(finding)?;
                guard.save()?;
                format!("Recorded finding '{}'.\n", finding.id)
            }
            FindingCommands::List {
                initiative,
                member,
                json: as_json,
            } => {
                let findings = CampaignStore::load(path)?
                    .list_findings(&initiative)?
                    .into_iter()
                    .filter(|finding| {
                        member.as_ref().is_none_or(|member| {
                            finding.to.is_empty() || finding.to.contains(member)
                        })
                    })
                    .map(|finding| {
                        Ok(serde_json::from_value::<proto::Finding>(serde_json::to_value(
                            finding,
                        )?)?)
                    })
                    .collect::<Result<Vec<_>>>()?;
                if as_json {
                    json(&findings)?
                } else {
                    render::initiative::findings(&findings)
                }
            }
        }
    } else {
        let mut client =
            connect_daemon_required(addr, "foundry --offline finding <command>").await?;
        match command {
            FindingCommands::Add { file } => {
                let definition_json = std::fs::read_to_string(file)?;
                let finding = client
                    .add_finding(proto::AddFindingRequest { definition_json })
                    .await
                    .map_err(status_to_anyhow)?
                    .into_inner()
                    .finding
                    .context("daemon returned no finding")?;
                format!("Recorded finding '{}'.\n", finding.id)
            }
            FindingCommands::List {
                initiative,
                member,
                json: as_json,
            } => {
                let findings = client
                    .list_findings(proto::ListFindingsRequest {
                        initiative,
                        member: member.unwrap_or_default(),
                    })
                    .await
                    .map_err(status_to_anyhow)?
                    .into_inner()
                    .findings;
                if as_json {
                    json(&findings)?
                } else {
                    render::initiative::findings(&findings)
                }
            }
        }
    };
    print!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unreachable_online_reads_leave_client_store_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.json");
        let result = run_and_render(
            Commands::List { json: true },
            &path,
            &dir.path().join("registry.json"),
            "http://127.0.0.1:1",
            false,
        )
        .await;
        assert!(result.is_err());
        assert!(!path.exists());
        assert!(!path.with_extension("lock").exists());
    }
}

#[cfg(test)]
mod steering_transport_tests {
    use super::*;

    #[test]
    fn an_offline_show_keeps_why_the_last_steer_skipped_integration() {
        let reason = "integration skipped: other work holds initiative trunks: sample";
        let initiative: Initiative = serde_json::from_value(serde_json::json!({
            "name":"shared", "outcome":"The trunks agree", "projects":["sample"],
            "authorized_by":"Stacey", "members":[], "done_evidence":[],
            "steering_history":[{
                "id":"steer_1", "prompt":"", "decision":{"decision":"wait","reason":"busy"},
                "provider":null, "recorded_at":"2026-10-16T12:00:00Z",
                "integration_skipped":reason
            }]
        }))
        .unwrap();
        let shown = to_proto(&CampaignStore::default(), &initiative).unwrap();
        assert_eq!(shown.steering_history[0].integration_skipped.as_deref(), Some(reason));
    }

    #[test]
    fn integration_gate_file_accepts_typed_gates_and_rejects_reviews() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("gates.json");
        std::fs::write(&path, r#"[{"kind":"gate","command":"./check","host":"build-host","artifacts":["sample/contract.json"]}]"#).unwrap();
        let gates = read_integration_gates(Some(&path)).unwrap();
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0].kind, "gate");
        assert_eq!(gates[0].host.as_deref(), Some("build-host"));
        assert!(gates[0].required);
        assert_eq!(gates[0].artifacts, ["sample/contract.json"]);
        std::fs::write(&path, r#"[{"kind":"review","statement":"Bypass Tracker"}]"#).unwrap();
        assert!(
            read_integration_gates(Some(&path))
                .unwrap_err()
                .to_string()
                .contains("only gate declarations")
        );
        assert!(
            read_integration_gates(None).unwrap().is_empty(),
            "expected `read_integration_gates(None).unwrap()` to be empty: {:?}",
            read_integration_gates(None).unwrap()
        );
        std::fs::write(&path, "[]").unwrap();
        assert!(
            read_integration_gates(Some(&path)).unwrap().is_empty(),
            "expected `read_integration_gates(Some(&path)).unwrap()` to be empty: {:?}",
            read_integration_gates(Some(&path)).unwrap()
        );
    }

    #[tokio::test]
    async fn online_steering_never_reads_or_mutates_the_client_store() {
        let home = tempfile::tempdir().unwrap();
        let campaigns = home.path().join("campaigns.json");
        std::fs::write(&campaigns, "client store deliberately malformed").unwrap();
        let bytes = std::fs::read(&campaigns).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let result = run_and_render(
            Commands::Advance {
                name: "shared".into(),
                member: None,
                origin: None,
                json: false,
            },
            &campaigns,
            &home.path().join("registry.json"),
            &address,
            false,
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("daemon"));
        assert_eq!(std::fs::read(campaigns).unwrap(), bytes);
    }

    #[tokio::test]
    async fn offline_steering_and_derivation_do_not_create_recovery_state() {
        let home = tempfile::tempdir().unwrap();
        for command in [
            Commands::Advance {
                name: "shared".into(),
                member: None,
                origin: None,
                json: false,
            },
            Commands::Derive {
                from_ref: "et:goal:shared".into(),
                projects: vec!["sample".into()],
                authorized_by: Some("Stacey".into()),
                integration_gates: None,
            },
            Commands::Complete {
                name: "shared".into(),
                reason: "Owner confirmation".into(),
                json: true,
            },
        ] {
            let result = run_and_render(
                command,
                &home.path().join("campaigns.json"),
                &home.path().join("registry.json"),
                "http://127.0.0.1:1",
                true,
            )
            .await;
            assert!(result.unwrap_err().to_string().contains("require the daemon"));
            assert!(!home.path().join("campaigns.json").exists());
        }
    }
}
