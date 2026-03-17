use crate::config::Config;
use crate::ipc::{IpcClient, Request, Response};
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "comfyui-dl",
    about = "CLI client for the comfyui-downloader daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
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
    /// List downloaded models in the catalog
    List,
    /// Delete a model by job ID
    Delete {
        /// Job ID to delete
        id: Uuid,
    },
    /// Trigger an immediate update check
    CheckUpdates,
    /// Cancel a queued or active download by ID
    Cancel { id: Uuid },
    /// Set the CivitAI API key in the config file
    SetKey {
        /// Your CivitAI API key
        key: String,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();

    // SetKey runs without a daemon connection — it writes directly to the config file.
    if let Some(Command::SetKey { key }) = cli.command {
        let mut config = Config::load()?;
        config.civitai.api_key = Some(key);
        config.save()?;
        println!("API key saved to {}", Config::config_path().display());
        return Ok(());
    }

    let config = Config::load()?;
    let mut client = IpcClient::connect(&config.daemon.socket_path).await?;

    let is_status = cli.command.is_none() || matches!(cli.command, Some(Command::Status));

    let req = match cli.command {
        None | Some(Command::Status) => Request::GetStatus,
        Some(Command::Add { url, model_type }) => Request::AddDownload { url, model_type },
        Some(Command::List) => Request::ListModels,
        Some(Command::Delete { id }) => Request::DeleteModel { id },
        Some(Command::CheckUpdates) => Request::CheckUpdates,
        Some(Command::Cancel { id }) => Request::Cancel { id },
        Some(Command::SetKey { .. }) => unreachable!(),
    };

    let response = client.send(&req).await?;

    if is_status {
        print_status(&response)?;
    } else {
        println!("{}", serde_json::to_string_pretty(&response)?);
    }
    Ok(())
}

fn print_status(response: &Response) -> Result<()> {
    let data = match response {
        Response::Ok(data) => data,
        Response::Err { message } => bail!("daemon error: {message}"),
    };

    let active = data["active"].as_array();
    let queued = data["queued"].as_u64().unwrap_or(0);
    let queued_jobs = data["queued_jobs"].as_array();
    let free_bytes = data["free_bytes"].as_u64().unwrap_or(0);

    if let Some(jobs) = active {
        if jobs.is_empty() {
            println!("No active downloads.");
        } else {
            println!(
                "{}",
                if jobs.len() == 1 {
                    "Downloading:".to_string()
                } else {
                    format!("Downloading ({}):", jobs.len())
                }
            );
            println!();
            for job in jobs {
                print_active_job(job);
            }
        }
    } else {
        println!("No active downloads.");
    }

    if queued > 0 {
        println!("Queued: {queued}");
        if let Some(jobs) = queued_jobs {
            for job in jobs {
                print_queued_job(job);
            }
        }
        println!();
    }

    println!("Free disk space: {}", format_bytes(free_bytes));

    Ok(())
}

fn print_active_job(job: &serde_json::Value) {
    let name = job["model_name"].as_str().unwrap_or("Unknown model");
    let bytes_received = job["bytes_received"].as_u64().unwrap_or(0);
    let total_bytes = job["total_bytes"].as_u64();
    let dest_path = job["dest_path"].as_str();
    let model_type = job["model_type"].as_str();
    let download_reason = job["download_reason"].as_str();
    let started_at = job["started_at"]
        .as_str()
        .and_then(|s| s.parse::<DateTime<Utc>>().ok());

    print!("  {name}");
    if let Some(mt) = model_type {
        print!("  [{mt}]");
    }
    println!();

    if let Some(total) = total_bytes {
        let pct = if total > 0 {
            (bytes_received as f64 / total as f64 * 100.0) as u64
        } else {
            0
        };
        let bar = progress_bar(pct, 30);
        println!(
            "  {bar} {pct:>3}%  ({} / {})",
            format_bytes(bytes_received),
            format_bytes(total),
        );

        if let Some(started) = started_at {
            let elapsed = Utc::now().signed_duration_since(started);
            let elapsed_secs = elapsed.num_seconds().max(1) as f64;
            if bytes_received > 0 && total > bytes_received {
                let remaining_bytes = total - bytes_received;
                let speed = bytes_received as f64 / elapsed_secs;
                let eta_secs = (remaining_bytes as f64 / speed) as u64;
                println!(
                    "  ETA: {}  ({}/s)",
                    format_duration(eta_secs),
                    format_bytes(speed as u64),
                );
            }
        }
    } else {
        println!("  {} downloaded", format_bytes(bytes_received));
    }

    if let Some(path) = dest_path {
        println!("  Path: {path}");
    }

    if download_reason == Some("update_available") {
        println!("  \u{2191} Upgrade from previous version");
    }

    println!();
}

fn print_queued_job(job: &serde_json::Value) {
    let url = job["url"].as_str().unwrap_or("?");
    let model_type = job["model_type"].as_str();
    let download_reason = job["download_reason"].as_str();

    print!("  \u{23f3} {url}");
    if let Some(mt) = model_type {
        print!("  [{mt}]");
    }
    if download_reason == Some("update_available") {
        print!("  (upgrade)");
    }
    println!();
}

fn progress_bar(pct: u64, width: usize) -> String {
    let filled = (pct as usize * width / 100).min(width);
    let empty = width - filled;
    format!(
        "[{}{}]",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
    )
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;

    if hours > 0 {
        format!("{hours}h {mins:02}m {s:02}s")
    } else if mins > 0 {
        format!("{mins}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1 KiB");
        assert_eq!(format_bytes(1_048_576), "1.0 MiB");
        assert_eq!(format_bytes(1_073_741_824), "1.00 GiB");
        assert_eq!(format_bytes(5_368_709_120), "5.00 GiB");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3661), "1h 01m 01s");
    }

    #[test]
    fn test_progress_bar() {
        let bar = progress_bar(50, 10);
        assert_eq!(
            bar,
            "[\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}]"
        );

        let bar_full = progress_bar(100, 5);
        assert_eq!(bar_full, "[\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}]");

        let bar_empty = progress_bar(0, 5);
        assert_eq!(bar_empty, "[\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}]");
    }
}
