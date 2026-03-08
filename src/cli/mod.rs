use crate::config::Config;
use crate::ipc::{IpcClient, Request};
use anyhow::Result;
use clap::{Parser, Subcommand};
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "comfyui-dl", about = "CLI client for the comfyui-downloader daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enqueue a CivitAI model URL for download
    Add {
        url: String,
        /// Override model type (checkpoints, loras, vae, …)
        #[arg(long)]
        model_type: Option<String>,
    },
    /// Show daemon status and download queue
    Status,
    /// Trigger an immediate update check
    CheckUpdates,
    /// Cancel a queued or active download by ID
    Cancel { id: Uuid },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load()?;
    let mut client = IpcClient::connect(&config.daemon.socket_path).await?;

    let req = match cli.command {
        Command::Add { url, model_type } => Request::AddDownload { url, model_type },
        Command::Status => Request::GetStatus,
        Command::CheckUpdates => Request::CheckUpdates,
        Command::Cancel { id } => Request::Cancel { id },
    };

    let response = client.send(&req).await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}
