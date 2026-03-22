use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;

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
        } => commands::emit(&cli.addr, &event_type, &project, &throttle, payload).await,
        Commands::Status { workflow_id } => commands::status(&cli.addr, workflow_id).await,
        Commands::Watch { project } => commands::watch(&cli.addr, project).await,
        Commands::Trace { event_id } => commands::trace(&cli.addr, &event_id).await,
    }
}
