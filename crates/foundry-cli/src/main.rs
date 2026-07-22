use anyhow::Result;
use clap::{Parser, Subcommand};

mod campaign_commands;
mod commands;
mod daemon;
mod event_commands;
mod gates_commands;
mod init_commands;
mod registry_commands;
mod render;
mod sentinel_commands;
mod workflow_commands;

pub mod proto {
    #![allow(clippy::all, clippy::pedantic)]
    tonic::include_proto!("foundry");
}

#[derive(Parser)]
#[command(name = "foundry", about = "Foundry — engineering workflow controller")]
#[command(version)]
struct Cli {
    /// Daemon address to connect to
    #[arg(long, default_value = "http://127.0.0.1:50051", global = true)]
    addr: String,

    /// Skip daemon RPCs and use explicit offline file access where supported
    #[arg(long, global = true)]
    offline: bool,

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

        /// Throttle level: full, `dry_run`
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

        /// Filter to workflows whose trace contains the given span id.
        #[arg(long)]
        span: Option<String>,
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

        /// Print events in chronological order (legacy 0.16 format).
        ///
        /// By default `trace` renders the OTel-shaped span tree. Use `--flat`
        /// to fall back to the pre-0.17 event-tree view.
        #[arg(long, default_value_t = false)]
        flat: bool,
    },

    /// Trigger a maintenance run for all or specific projects
    Run {
        /// Limit run to a single project by name
        #[arg(long)]
        project: Option<String>,

        /// Throttle level: full, `dry_run`
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

        /// Agent backend to run on: claude, opencode, or codex
        /// (overrides the daemon default for this run)
        #[arg(long)]
        agent: Option<String>,
    },

    /// Run one user-provided coding task on a project
    Task {
        /// Project name from registry
        project: String,

        /// Concrete task description for the coding agent
        description: String,

        /// Agent backend to run on: claude, opencode, or codex
        /// (overrides the daemon default for this run)
        #[arg(long)]
        agent: Option<String>,
    },

    /// Scout a project for intent drift (bug candidates)
    Scout {
        /// Project name from registry
        project: String,

        /// Agent backend to run on: claude, opencode, or codex
        /// (overrides the daemon default for this run)
        #[arg(long)]
        agent: Option<String>,
    },

    /// Check GitHub Actions pipeline health and remediate failures
    Pipeline {
        /// Project name from registry
        project: String,

        /// Agent backend to run on: claude, opencode, or codex
        /// (overrides the daemon default for this run)
        #[arg(long)]
        agent: Option<String>,
    },

    /// Run an agent-driven release workflow for a project
    Release {
        /// Project name from registry
        project: String,

        /// Version bump type: patch, minor, or major (auto-detected if omitted)
        #[arg(long)]
        bump: Option<String>,
    },

    /// Install (or remove) the Foundry skill for Claude agents
    Init {
        /// Install into the project (.claude/skills/foundry/) instead of globally
        #[arg(long)]
        local: bool,

        /// Accepted for compatibility; global is the default (no-op)
        #[arg(long)]
        global: bool,

        /// Overwrite files even if an installed version is newer (downgrade)
        #[arg(long)]
        force: bool,

        /// Remove the installed Foundry skill and clean the lock entry
        #[arg(long)]
        remove: bool,

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

    /// Manage scheduled sentinels (proactive workflow triggers)
    #[command(subcommand)]
    Sentinel(SentinelCommands),

    /// Manage durable objective campaigns
    #[command(subcommand)]
    Campaign(CampaignCommands),
}

#[derive(Subcommand)]
enum CampaignCommands {
    /// Add a campaign from a JSON definition file
    Add { file: std::path::PathBuf },
    /// List campaigns
    List,
    /// Show one campaign
    Show { name: String },
    /// Derive and dispatch one next objective from live state
    Advance { name: String },
    /// Pause automatic advancement
    Pause { name: String },
    /// Record an owner decision and reactivate an escalated campaign
    Decide {
        name: String,
        #[arg(long)]
        decision: String,
    },
    /// Mark an authorized campaign complete with an auditable reason
    Complete {
        name: String,
        #[arg(long)]
        reason: String,
    },
    /// Resume an authorized paused or escalated campaign
    Resume {
        name: String,
        /// Add owner-authorized cycles to the campaign budget before resuming
        #[arg(long, default_value_t = 0)]
        add_cycles: u64,
    },
}

#[derive(Subcommand)]
enum SentinelCommands {
    /// List all sentinels
    List,

    /// Show details for a single sentinel
    Show {
        /// Sentinel name
        name: String,
    },

    /// Mark a sentinel as enabled
    Enable {
        /// Sentinel name
        name: String,
    },

    /// Mark a sentinel as disabled
    Disable {
        /// Sentinel name
        name: String,
    },
}

#[derive(Subcommand)]
enum RegistryCommands {
    /// Create an empty registry file during explicit offline recovery
    Init,

    /// List all projects from the daemon-owned registry
    List,

    /// Show details for a project from the daemon-owned registry
    Show {
        /// Project name
        name: String,
    },

    /// Add a project to the daemon-owned registry
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

    /// Remove a project from the daemon-owned registry
    Remove {
        /// Project name
        name: String,
    },

    /// Edit a project's settings in the daemon-owned registry
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

async fn handle_registry_command(
    sub: RegistryCommands,
    path: &std::path::Path,
    addr: &str,
    offline: bool,
) -> Result<()> {
    match sub {
        RegistryCommands::Init => registry_commands::init(path, offline),
        RegistryCommands::List => registry_commands::list(path, addr, offline).await,
        RegistryCommands::Show { name } => {
            registry_commands::show(path, addr, offline, &name).await
        }
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
        } => {
            let spec = registry_commands::SpecArgs {
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
            };
            registry_commands::add_from_args(path, addr, offline, spec).await
        }
        RegistryCommands::Remove { name } => {
            registry_commands::remove(path, addr, offline, &name).await
        }
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
        } => {
            let edits = registry_commands::EditArgs {
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
            };
            registry_commands::edit_from_args(path, addr, offline, &name, edits).await
        }
    }
}

async fn handle_sentinel_command(
    sub: SentinelCommands,
    path: &std::path::Path,
    addr: &str,
    offline: bool,
) -> Result<()> {
    match sub {
        SentinelCommands::List => sentinel_commands::list(path),
        SentinelCommands::Show { name } => sentinel_commands::show(path, &name),
        SentinelCommands::Enable { name } => {
            sentinel_commands::enable(path, addr, offline, &name).await
        }
        SentinelCommands::Disable { name } => {
            sentinel_commands::disable(path, addr, offline, &name).await
        }
    }
}

async fn handle_campaign_command(
    command: CampaignCommands,
    campaigns_path: &std::path::Path,
    addr: &str,
    offline: bool,
) -> Result<()> {
    match command {
        CampaignCommands::Add { file } => {
            campaign_commands::add(campaigns_path, &foundry_sdk::paths::registry_path(), &file)
        }
        CampaignCommands::List => campaign_commands::list(campaigns_path),
        CampaignCommands::Show { name } => campaign_commands::show(campaigns_path, &name),
        CampaignCommands::Advance { name } => {
            campaign_commands::advance(addr, campaigns_path, &name).await
        }
        CampaignCommands::Pause { name } => {
            campaign_commands::pause(campaigns_path, addr, offline, &name).await
        }
        CampaignCommands::Decide { name, decision } => {
            campaign_commands::decide(campaigns_path, addr, offline, &name, &decision).await
        }
        CampaignCommands::Complete { name, reason } => {
            campaign_commands::complete(campaigns_path, addr, offline, &name, &reason).await
        }
        CampaignCommands::Resume { name, add_cycles } => {
            campaign_commands::resume(campaigns_path, addr, offline, &name, add_cycles).await
        }
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
        } => event_commands::emit(&cli.addr, &event_type, &project, &throttle, payload, wait).await,
        Commands::Status { workflow_id, span } => {
            event_commands::status(&cli.addr, workflow_id, span).await
        }
        Commands::Watch { project } => event_commands::watch(&cli.addr, project).await,
        Commands::Trace {
            event_id,
            verbose,
            flat,
        } => event_commands::trace(&cli.addr, &event_id, verbose, flat).await,
        Commands::Run { project, throttle } => {
            workflow_commands::run(&cli.addr, project, &throttle).await
        }
        Commands::Validate { projects, all } => {
            workflow_commands::validate(
                &cli.addr,
                projects,
                all,
                &foundry_sdk::paths::registry_path(),
            )
            .await
        }
        Commands::Iterate { project, agent } => {
            workflow_commands::iterate(&cli.addr, &project, agent.as_deref()).await
        }
        Commands::Task {
            project,
            description,
            agent,
        } => workflow_commands::task(&cli.addr, &project, &description, agent.as_deref()).await,
        Commands::Scout { project, agent } => {
            workflow_commands::scout(&cli.addr, &project, agent.as_deref()).await
        }
        Commands::Pipeline { project, agent } => {
            workflow_commands::pipeline(&cli.addr, &project, agent.as_deref()).await
        }
        Commands::Release { project, bump } => {
            workflow_commands::release(&cli.addr, &project, bump).await
        }
        Commands::History { date, project } => {
            event_commands::history(date.as_deref(), project.as_deref())
        }
        Commands::Init {
            local,
            global: _, // accepted for registry compatibility; global is the default
            force,
            remove,
            json,
        } => {
            if remove {
                init_commands::remove(local, json)
            } else {
                init_commands::run(local, force, json)
            }
        }
        Commands::Gates { project, dir, init } => {
            let project_dir = gates_commands::resolve_project_dir(
                project.as_deref(),
                dir.as_deref(),
                &foundry_sdk::paths::registry_path(),
            )?;
            if init {
                gates_commands::init(&project_dir)
            } else {
                gates_commands::show(&project_dir)
            }
        }
        Commands::Registry(sub) => {
            handle_registry_command(
                sub,
                &foundry_sdk::paths::registry_path(),
                &cli.addr,
                cli.offline,
            )
            .await
        }
        Commands::Campaign(sub) => {
            let campaigns = foundry_sdk::paths::campaigns_path();
            handle_campaign_command(sub, &campaigns, &cli.addr, cli.offline).await
        }
        Commands::Sentinel(sub) => {
            handle_sentinel_command(
                sub,
                &foundry_sdk::paths::sentinels_path(),
                &cli.addr,
                cli.offline,
            )
            .await
        }
    }
}
