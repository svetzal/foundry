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
    /// Record a decision for applicable members \[mutates\]
    Decide {
        name: String,
        #[arg(long)]
        decision: String,
        #[arg(long = "member")]
        members: Vec<String>,
    },
    /// Mark an owner-verified outcome complete \[mutates\]
    Complete {
        name: String,
        #[arg(long)]
        reason: String,
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
    if let Some(members) = value["members"].as_array_mut() {
        for member in members {
            if let Some(campaign) = member["name"].as_str().and_then(|name| store.find(name)) {
                member["campaign"] =
                    serde_json::to_value(crate::campaign_commands::summary_to_proto(campaign))?;
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
    if let Commands::Derive {
        from_ref,
        projects,
        authorized_by,
    } = command
    {
        let repo =
            std::env::current_dir().context("cannot locate the Tracker source repository")?;
        let mut initiative =
            foundry_sdk::initiative::derive::derive(&repo, &from_ref, projects).await?;
        initiative.authorized_by = Some(
            authorized_by
                .or_else(|| std::env::var("USER").ok())
                .context("set --authorized-by to name the initiative owner")?,
        );
        return add_definition(
            store_path,
            registry_path,
            addr,
            offline,
            &serde_json::to_string(&initiative)?,
        )
        .await;
    }
    if offline {
        return run_offline(command, store_path, registry_path);
    }
    run_online(command, addr).await
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
            let response = client
                .add_initiative(proto::AddInitiativeRequest {
                    definition_json: definition,
                })
                .await
                .map_err(status_to_anyhow)?
                .into_inner();
            let initiative = response.initiative.context("daemon returned no initiative")?;
            Ok(format!("Added initiative '{}'.\n", initiative.name))
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
        } => {
            let initiative = client
                .decide_initiative(proto::DecideInitiativeRequest {
                    name,
                    decision,
                    members,
                })
                .await
                .map_err(status_to_anyhow)?
                .into_inner()
                .initiative
                .context("daemon returned no initiative")?;
            Ok(format!("Recorded decision for initiative '{}'.\n", initiative.name))
        }
        Commands::Complete { name, reason } => {
            client
                .complete_initiative(proto::EndInitiativeRequest {
                    name: name.clone(),
                    reason,
                })
                .await
                .map_err(status_to_anyhow)?;
            Ok(format!("Completed initiative '{name}'.\n"))
        }
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
        Commands::Derive { .. } => unreachable!("derive handled before transport selection"),
    }
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
                } => {
                    guard.store.decide_initiative(&name, &decision, &members, Utc::now())?;
                    format!("Recorded decision for initiative '{name}'.\n")
                }
                Commands::Complete { name, reason } => {
                    let _transition =
                        guard.store.complete_initiative(&name, &reason, Utc::now())?;
                    format!("Completed initiative '{name}'.\n")
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
