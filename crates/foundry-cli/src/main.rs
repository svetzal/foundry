use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod gates_commands;
mod init_commands;
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

        /// Show raw output and payloads for each block
        #[arg(long)]
        verbose: bool,
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

    /// Validate project gate health without running iterate/maintain
    Validate {
        /// Project names to validate (omit for --all)
        projects: Vec<String>,

        /// Validate all projects in registry
        #[arg(long)]
        all: bool,
    },

    /// Show trace history from disk
    History {
        /// Date to show (YYYY-MM-DD); omit for recent 7 days
        date: Option<String>,

        /// Filter by project name
        #[arg(long)]
        project: Option<String>,
    },

    /// Run a single iteration cycle on a project
    Iterate {
        /// Project name from registry
        project: String,
    },

    /// Scout a project for intent drift (bug candidates)
    Scout {
        /// Project name from registry
        project: String,
    },

    /// Check GitHub Actions pipeline health and remediate failures
    Pipeline {
        /// Project name from registry
        project: String,
    },

    /// Run an agent-driven release workflow for a project
    Release {
        /// Project name from registry
        project: String,

        /// Version bump type: patch, minor, or major (auto-detected if omitted)
        #[arg(long)]
        bump: Option<String>,
    },

    /// Install the Foundry skill for Claude agents
    Init {
        /// Install globally (~/.claude/skills/) instead of locally (.claude/skills/)
        #[arg(long)]
        global: bool,

        /// Overwrite files even if an installed version is newer
        #[arg(long)]
        force: bool,

        /// Emit machine-readable JSON instead of human output
        #[arg(long)]
        json: bool,
    },

    /// Show or derive quality gates for a project
    Gates {
        /// Project name from registry
        project: Option<String>,

        /// Use a directory path instead of a registry project name
        #[arg(long)]
        dir: Option<String>,

        /// Derive gates by inspecting the project (writes .hone-gates.json)
        #[arg(long)]
        init: bool,
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

        /// Human-readable notes about the project
        #[arg(long)]
        notes: Option<String>,

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

        /// Set skip reason (empty string to clear)
        #[arg(long)]
        skip: Option<String>,

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

        /// Set notes
        #[arg(long)]
        notes: Option<String>,

        /// Set timeout in seconds
        #[arg(long)]
        timeout_secs: Option<u64>,
    },
}

fn handle_registry_command(sub: RegistryCommands, path: &std::path::Path) -> Result<()> {
    match sub {
        RegistryCommands::Init => registry_commands::init(path),
        RegistryCommands::List => registry_commands::list(path),
        RegistryCommands::Show { name } => registry_commands::show(path, &name),
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
            notes,
            timeout_secs,
        } => registry_commands::add(
            path,
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
            notes.as_deref(),
            timeout_secs,
        ),
        RegistryCommands::Remove { name } => registry_commands::remove(path, &name),
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
            notes,
            timeout_secs,
        } => registry_commands::edit(
            path,
            &name,
            project_path.as_deref(),
            stack.as_deref(),
            agent.as_deref(),
            repo.as_deref(),
            branch.as_deref(),
            skip.as_deref(),
            iterate,
            maintain,
            push,
            audit,
            release,
            install_command.as_deref(),
            install_brew.as_deref(),
            notes.as_deref(),
            timeout_secs,
        ),
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
        Commands::Trace { event_id, verbose } => {
            commands::trace(&cli.addr, &event_id, verbose).await
        }
        Commands::Run { project, throttle } => commands::run(&cli.addr, project, &throttle).await,
        Commands::Validate { projects, all } => {
            commands::validate(&cli.addr, projects, all, &foundry_core::paths::registry_path())
                .await
        }
        Commands::Iterate { project } => commands::iterate(&cli.addr, &project).await,
        Commands::Scout { project } => commands::scout(&cli.addr, &project).await,
        Commands::Pipeline { project } => commands::pipeline(&cli.addr, &project).await,
        Commands::Release { project, bump } => commands::release(&cli.addr, &project, bump).await,
        Commands::History { date, project } => {
            commands::history(date.as_deref(), project.as_deref())
        }
        Commands::Init {
            global,
            force,
            json,
        } => init_commands::run(global, force, json),
        Commands::Gates { project, dir, init } => {
            let project_dir = gates_commands::resolve_project_dir(
                project.as_deref(),
                dir.as_deref(),
                &foundry_core::paths::registry_path(),
            )?;
            if init {
                gates_commands::init(&project_dir)
            } else {
                gates_commands::show(&project_dir)
            }
        }
        Commands::Registry(sub) => {
            handle_registry_command(sub, &foundry_core::paths::registry_path())
        }
    }
}
