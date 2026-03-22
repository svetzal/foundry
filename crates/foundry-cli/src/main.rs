use std::env;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod registry_commands;

pub mod proto {
    #![allow(clippy::all, clippy::pedantic)]
    tonic::include_proto!("foundry");
}

#[derive(Parser)]
#[command(name = "foundry", about = "Foundry — engineering workflow controller")]
#[command(version)]
struct Cli {
    /// Daemon address to connect to
    #[arg(long, default_value = "http://[::1]:50051", global = true)]
    addr: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Emit an event into the system
    Emit {
        /// Event type (e.g., `vulnerability_detected`)
        event_type: String,

        /// Target project
        #[arg(long)]
        project: String,

        /// Throttle level: full, `audit_only`, `dry_run`
        #[arg(long, default_value = "full")]
        throttle: String,

        /// Additional payload as JSON
        #[arg(long)]
        payload: Option<String>,

        /// Wait for processing to complete, then show the trace
        #[arg(long)]
        wait: bool,
    },

    /// Show status of active workflows
    Status {
        /// Specific workflow ID (omit for all active)
        workflow_id: Option<String>,
    },

    /// Stream live events in real-time
    Watch {
        /// Filter by project name (omit for all projects)
        #[arg(long)]
        project: Option<String>,
    },

    /// View the trace of a completed event chain
    Trace {
        /// The root event ID to look up
        event_id: String,
    },

    /// Trigger a maintenance run for all or specific projects
    Run {
        /// Limit run to a single project by name
        #[arg(long)]
        project: Option<String>,

        /// Throttle level: full, `audit_only`, `dry_run`
        #[arg(long, default_value = "full")]
        throttle: String,
    },

    /// Manage the project registry
    #[command(subcommand)]
    Registry(RegistryCommands),
}

#[derive(Subcommand)]
enum RegistryCommands {
    /// Create an empty registry file
    Init,

    /// List all projects
    List,

    /// Show details for a project
    Show {
        /// Project name
        name: String,
    },

    /// Add a project to the registry
    Add {
        /// Project name
        #[arg(long)]
        name: String,

        /// Absolute path to the project
        #[arg(long)]
        path: String,

        /// Technology stack: rust, python, typescript, elixir
        #[arg(long)]
        stack: String,

        /// AI agent name
        #[arg(long)]
        agent: String,

        /// GitHub repo slug (owner/repo)
        #[arg(long)]
        repo: String,

        /// Default branch
        #[arg(long, default_value = "main")]
        branch: String,

        /// Enable iterate action
        #[arg(long)]
        iterate: bool,

        /// Enable maintain action
        #[arg(long)]
        maintain: bool,

        /// Enable push action
        #[arg(long)]
        push: bool,

        /// Enable audit action
        #[arg(long)]
        audit: bool,

        /// Enable release action
        #[arg(long)]
        release: bool,

        /// Install via shell command
        #[arg(long)]
        install_command: Option<String>,

        /// Install via Homebrew formula
        #[arg(long)]
        install_brew: Option<String>,

        /// Command timeout in seconds
        #[arg(long)]
        timeout_secs: Option<u64>,
    },

    /// Remove a project from the registry
    Remove {
        /// Project name
        name: String,
    },

    /// Edit a project's settings
    Edit {
        /// Project name
        name: String,

        /// Update path
        #[arg(long)]
        path: Option<String>,

        /// Update stack
        #[arg(long)]
        stack: Option<String>,

        /// Update agent
        #[arg(long)]
        agent: Option<String>,

        /// Update repo
        #[arg(long)]
        repo: Option<String>,

        /// Update branch
        #[arg(long)]
        branch: Option<String>,

        /// Set skip flag
        #[arg(long)]
        skip: Option<bool>,

        /// Set iterate action
        #[arg(long)]
        iterate: Option<bool>,

        /// Set maintain action
        #[arg(long)]
        maintain: Option<bool>,

        /// Set push action
        #[arg(long)]
        push: Option<bool>,

        /// Set audit action
        #[arg(long)]
        audit: Option<bool>,

        /// Set release action
        #[arg(long)]
        release: Option<bool>,

        /// Set install command
        #[arg(long)]
        install_command: Option<String>,

        /// Set install brew formula
        #[arg(long)]
        install_brew: Option<String>,

        /// Set timeout in seconds
        #[arg(long)]
        timeout_secs: Option<u64>,
    },
}

/// Resolve the registry file path from env or default.
fn registry_path() -> PathBuf {
    if let Ok(p) = env::var("FOUNDRY_REGISTRY_PATH") {
        PathBuf::from(p)
    } else {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(format!("{home}/.foundry/registry.json"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Emit {
            event_type,
            project,
            throttle,
            payload,
            wait,
        } => commands::emit(&cli.addr, &event_type, &project, &throttle, payload, wait).await,
        Commands::Status { workflow_id } => commands::status(&cli.addr, workflow_id).await,
        Commands::Watch { project } => commands::watch(&cli.addr, project).await,
        Commands::Trace { event_id } => commands::trace(&cli.addr, &event_id).await,
        Commands::Run { project, throttle } => commands::run(&cli.addr, project, &throttle).await,
        Commands::Registry(sub) => {
            let path = registry_path();
            match sub {
                RegistryCommands::Init => registry_commands::init(&path),
                RegistryCommands::List => registry_commands::list(&path),
                RegistryCommands::Show { name } => registry_commands::show(&path, &name),
                RegistryCommands::Add {
                    name,
                    path: project_path,
                    stack,
                    agent,
                    repo,
                    branch,
                    iterate,
                    maintain,
                    push,
                    audit,
                    release,
                    install_command,
                    install_brew,
                    timeout_secs,
                } => registry_commands::add(
                    &path,
                    &name,
                    &project_path,
                    &stack,
                    &agent,
                    &repo,
                    &branch,
                    iterate,
                    maintain,
                    push,
                    audit,
                    release,
                    install_command.as_deref(),
                    install_brew.as_deref(),
                    timeout_secs,
                ),
                RegistryCommands::Remove { name } => registry_commands::remove(&path, &name),
                RegistryCommands::Edit {
                    name,
                    path: project_path,
                    stack,
                    agent,
                    repo,
                    branch,
                    skip,
                    iterate,
                    maintain,
                    push,
                    audit,
                    release,
                    install_command,
                    install_brew,
                    timeout_secs,
                } => registry_commands::edit(
                    &path,
                    &name,
                    project_path.as_deref(),
                    stack.as_deref(),
                    agent.as_deref(),
                    repo.as_deref(),
                    branch.as_deref(),
                    skip,
                    iterate,
                    maintain,
                    push,
                    audit,
                    release,
                    install_command.as_deref(),
                    install_brew.as_deref(),
                    timeout_secs,
                ),
            }
        }
    }
}
