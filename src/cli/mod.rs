use crate::config::Config;
use crate::ipc::protocol::{FileVariantInfo, QueueItem, TemplateListing, VersionInfo};
use crate::ipc::{IpcClient, Request, Response};
use crate::templates::{TemplateBundle, TemplateFilter};
use crate::vram::{Feasibility, WorkloadKind};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use std::collections::BTreeSet;
use std::io::IsTerminal;
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
    Add {
        url: String,
        #[arg(long)]
        model_type: Option<String>,
    },
    Status,
    List,
    /// Delete a catalogued model by ID (full UUID or unique prefix).
    Delete {
        id: String,
    },
    CheckUpdates,
    /// Cancel a queued or active download by ID (full UUID or unique prefix).
    Cancel {
        id: String,
    },
    SetKey {
        key: String,
    },
    Updates,
    DownloadVersion {
        model_id: u64,
        version_id: u64,
    },
    /// Re-queue catalogued models whose files are missing on disk.
    RedownloadMissing {
        /// Re-queue all catalogued models, even ones still present on disk.
        #[arg(long)]
        all: bool,
    },
    /// Browse the ComfyUI default workflow templates and download their models.
    ///
    /// Each template is judged against the local GPU: bundles that fit
    /// entirely in VRAM, bundles that fit only with the text encoders and VAE
    /// on the CPU, and bundles that cannot run at all (hidden by default).
    Templates {
        /// Free text matched against title, description, tags and model family.
        query: Option<String>,
        /// Generation type: image, video, audio, 3d or llm (repeatable).
        #[arg(long = "type", value_name = "KIND")]
        kind: Vec<String>,
        /// Task tag, e.g. "image edit" or "text to video" (repeatable).
        #[arg(long, value_name = "TAG")]
        task: Vec<String>,
        /// Model family, e.g. z-image-turbo (repeatable).
        #[arg(long, value_name = "FAMILY")]
        model: Vec<String>,
        /// Exact template name (repeatable).
        #[arg(long, value_name = "NAME")]
        name: Vec<String>,
        /// Only bundles that fit entirely in VRAM.
        #[arg(long)]
        comfortable_only: bool,
        /// Include cloud-API templates, which download no weights.
        #[arg(long)]
        include_api: bool,
        /// Include bundles that cannot run on this GPU.
        #[arg(long)]
        include_unrunnable: bool,
        /// Re-fetch the catalog instead of using the local cache.
        #[arg(long)]
        refresh: bool,
        /// Print the resolved listing as JSON instead of picking.
        #[arg(long)]
        json: bool,
        /// Queue every matching template without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[allow(clippy::too_many_arguments)]
async fn run_templates(
    client: &mut IpcClient,
    query: Option<String>,
    kinds: Vec<String>,
    tasks: Vec<String>,
    families: Vec<String>,
    names: Vec<String>,
    comfortable_only: bool,
    include_api: bool,
    include_unrunnable: bool,
    refresh: bool,
    json: bool,
    yes: bool,
) -> Result<()> {
    let filter = TemplateFilter {
        text: query,
        kinds: kinds
            .iter()
            .map(|raw| parse_kind(raw))
            .collect::<Result<Vec<_>>>()?,
        tasks,
        families,
        names,
        include_api,
    };

    let data = ok_data(
        client
            .send(&Request::ListTemplates {
                filter,
                refresh,
                include_unrunnable,
            })
            .await?,
    )?;

    if json {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }

    let listing: TemplateListing =
        serde_json::from_value(data).context("parsing template listing")?;
    let mut bundles = listing.bundles;
    if comfortable_only {
        // Keep only bundles that fit outright; an unjudged bundle (no GPU
        // figure) stays, anything needing offload or unable to run does not.
        bundles.retain(|b| {
            matches!(b.feasibility, Some(Feasibility::Comfortable)) || b.feasibility.is_none()
        });
    }
    bundles.sort_by(|a, b| {
        a.feasibility
            .cmp(&b.feasibility)
            .then_with(|| a.template.title.cmp(&b.template.title))
    });

    match listing.gpu {
        Some(ref gpu) => println!(
            "GPU: {} ({} VRAM)",
            gpu.name,
            format_bytes(listing.vram_bytes.unwrap_or(gpu.vram_bytes))
        ),
        None => match listing.vram_bytes {
            Some(bytes) => println!("VRAM budget: {} (from config)", format_bytes(bytes)),
            None => println!("No GPU detected — feasibility is not being judged."),
        },
    }
    if listing.hidden_unrunnable > 0 {
        println!(
            "{} template(s) hidden: they cannot run on this GPU (--include-unrunnable to show).",
            listing.hidden_unrunnable
        );
    }

    if bundles.is_empty() {
        println!("No templates match that filter.");
        return Ok(());
    }

    let items: Vec<String> = bundles.iter().map(format_bundle).collect();
    let selected: Vec<usize> = if yes || !std::io::stdout().is_terminal() {
        (0..bundles.len()).collect()
    } else {
        println!("\nSpace toggles, Enter confirms. Everything is selected by default.");
        dialoguer::MultiSelect::new()
            .with_prompt("Models to download")
            .items(&items)
            .defaults(&vec![true; bundles.len()])
            .interact()
            .context("template selection cancelled")?
    };

    if selected.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }

    // Selecting a template takes its whole dependency set — diffusion model,
    // text encoders, VAE, LoRAs — deduplicated across templates.
    let mut seen = BTreeSet::new();
    let mut queue = Vec::new();
    let mut total_bytes: u64 = 0;
    for index in &selected {
        let Some(bundle) = bundles.get(*index) else {
            continue;
        };
        for model in &bundle.models {
            if !seen.insert(model.url.clone()) {
                continue;
            }
            total_bytes += model.size_bytes.unwrap_or(0);
            queue.push(QueueItem {
                url: model.url.clone(),
                model_type: Some(model.role.clone()),
            });
        }
    }

    println!(
        "\nQueueing {} file(s) from {} template(s), {} to download.",
        queue.len(),
        selected.len(),
        format_bytes(total_bytes)
    );

    let data = ok_data(client.send(&Request::AddDownloads { items: queue }).await?)?;
    let queued = data["queued"].as_array().map(|a| a.len()).unwrap_or(0);
    println!("{queued} job(s) queued. Track them with `comfyui-dl status`.");
    if let Some(errors) = data["errors"].as_array().filter(|e| !e.is_empty()) {
        for err in errors {
            eprintln!("warning: {err}");
        }
    }
    Ok(())
}

fn parse_kind(raw: &str) -> Result<WorkloadKind> {
    let kind = WorkloadKind::from_template_type(raw);
    if kind == WorkloadKind::Unknown {
        bail!("unknown --type '{raw}' (expected image, video, audio, 3d or llm)");
    }
    Ok(kind)
}

fn format_bundle(bundle: &TemplateBundle) -> String {
    let tier = match bundle.feasibility {
        Some(Feasibility::Comfortable) => "fits in VRAM",
        Some(Feasibility::CpuOffload) => "needs CPU offload for encoders/VAE",
        Some(Feasibility::WontRun) => "will not run",
        None => "unknown fit",
    };
    let roles: Vec<&str> = bundle.models_by_role().keys().copied().collect();
    format!(
        "{}  [{}]  {}  — {tier}  ({} files: {})",
        bundle.template.title,
        kind_label(bundle.template.kind),
        format_bytes(bundle.download_bytes),
        bundle.models.len(),
        roles.join(", ")
    )
}

fn kind_label(kind: WorkloadKind) -> &'static str {
    match kind {
        WorkloadKind::Image => "image",
        WorkloadKind::Video => "video",
        WorkloadKind::Audio => "audio",
        WorkloadKind::ThreeD => "3d",
        WorkloadKind::Llm => "llm",
        WorkloadKind::Unknown => "other",
    }
}

fn select_variant(files: &[FileVariantInfo]) -> Option<String> {
    if files.len() <= 1 {
        return files.first().map(|f| f.name.clone());
    }
    if std::io::stdout().is_terminal() {
        let items: Vec<String> = files.iter().map(format_variant).collect();
        match dialoguer::Select::new()
            .with_prompt("Multiple variants available. Select one to download")
            .items(&items)
            .default(0)
            .interact()
        {
            Ok(idx) => Some(files[idx].name.clone()),
            Err(_) => select_largest(files),
        }
    } else {
        select_largest(files)
    }
}

fn select_largest(files: &[FileVariantInfo]) -> Option<String> {
    files
        .iter()
        .max_by(|a, b| {
            a.size_kb
                .partial_cmp(&b.size_kb)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|f| f.name.clone())
}

fn format_variant(f: &FileVariantInfo) -> String {
    let size = format_bytes((f.size_kb * 1024.0) as u64);
    let mut parts = vec![f.name.clone(), format!("({size})")];
    if let Some(ref fmt) = f.format {
        parts.push(fmt.clone());
    }
    if let Some(ref s) = f.size {
        parts.push(s.clone());
    }
    if let Some(ref fp) = f.fp {
        parts.push(fp.clone());
    }
    if let Some(ref qt) = f.quant_type {
        parts.push(qt.clone());
    }
    parts.join(" | ")
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

    // The template picker is interactive and needs several round trips, so it
    // runs its own request flow instead of the single request/response path.
    if let Some(Command::Templates {
        query,
        kind,
        task,
        model,
        name,
        comfortable_only,
        include_api,
        include_unrunnable,
        refresh,
        json,
        yes,
    }) = cli.command
    {
        return run_templates(
            &mut client,
            query,
            kind,
            task,
            model,
            name,
            comfortable_only,
            include_api,
            include_unrunnable,
            refresh,
            json,
            yes,
        )
        .await;
    }

    let is_status = cli.command.is_none() || matches!(cli.command, Some(Command::Status));

    let req = match cli.command {
        None | Some(Command::Status) => Request::GetStatus,
        Some(Command::Add { url, model_type }) => {
            let preferred = match client
                .send(&Request::GetVersionInfo { url: url.clone() })
                .await
            {
                Ok(Response::Ok(data)) => serde_json::from_value::<VersionInfo>(data)
                    .ok()
                    .and_then(|v| select_variant(&v.files)),
                _ => None,
            };
            Request::AddDownload {
                url,
                model_type,
                preferred_file_name: preferred,
            }
        }
        Some(Command::List) => Request::ListModels,
        Some(Command::Delete { id }) => Request::DeleteModel {
            id: resolve_model_id(&mut client, &id).await?,
        },
        Some(Command::CheckUpdates) => Request::CheckUpdates,
        Some(Command::Cancel { id }) => Request::Cancel {
            id: resolve_active_id(&mut client, &id).await?,
        },
        Some(Command::Updates) => Request::ListUpdates,
        Some(Command::DownloadVersion {
            model_id,
            version_id,
        }) => Request::DownloadVersion {
            model_id,
            version_id,
        },
        Some(Command::RedownloadMissing { all }) => Request::RedownloadMissing { all },
        Some(Command::SetKey { .. }) | Some(Command::Templates { .. }) => unreachable!(),
    };

    let is_updates = matches!(req, Request::ListUpdates);

    let response = client.send(&req).await?;

    if is_status {
        print_status(&response)?;
    } else if is_updates {
        print_updates(&response)?;
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
    let queued_jobs = data["queued"].as_array();
    let queued = queued_jobs.map(|a| a.len() as u64).unwrap_or(0);
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

fn print_updates(response: &Response) -> Result<()> {
    let data = match response {
        Response::Ok(data) => data,
        Response::Err { message } => bail!("daemon error: {message}"),
    };

    let updates = data.as_array();
    match updates {
        Some(items) if items.is_empty() => {
            println!("All models are up to date.");
        }
        Some(items) => {
            println!("{} update(s) available:\n", items.len());
            for item in items {
                let model_id = item["model_id"].as_u64().unwrap_or(0);
                let current_version = item["version_id"].as_u64().unwrap_or(0);
                let new_version = item["available_version_id"].as_u64().unwrap_or(0);
                let version_name = item["available_version_name"].as_str().unwrap_or("unknown");
                let model_type = item["model_type"].as_str().unwrap_or("?");
                let dest_path = item["dest_path"].as_str();

                let filename = dest_path.and_then(|p| p.rsplit('/').next()).unwrap_or("?");

                println!("  {filename}  [{model_type}]");
                println!("    version {current_version} \u{2192} {new_version} ({version_name})");
                println!("    comfyui-dl download-version {model_id} {new_version}");
                println!();
            }
        }
        None => {
            println!("All models are up to date.");
        }
    }

    Ok(())
}

fn print_active_job(job: &serde_json::Value) {
    let id = job["id"].as_str().unwrap_or("");
    let name = job["model_name"].as_str().unwrap_or("Unknown model");
    let bytes_received = job["bytes_received"].as_u64().unwrap_or(0);
    let total_bytes = job["total_bytes"].as_u64();
    let dest_path = job["dest_path"].as_str();
    let model_type = job["model_type"].as_str();
    let download_reason = job["download_reason"].as_str();
    let started_at = job["started_at"]
        .as_str()
        .and_then(|s| s.parse::<DateTime<Utc>>().ok());

    print!("  {}  {name}", short_id(id));
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
    let id = job["id"].as_str().unwrap_or("");
    let url = job["url"].as_str().unwrap_or("?");
    let model_type = job["model_type"].as_str();
    let download_reason = job["download_reason"].as_str();

    print!("  \u{23f3} {}  {url}", short_id(id));
    if let Some(mt) = model_type {
        print!("  [{mt}]");
    }
    if download_reason == Some("update_available") {
        print!("  (upgrade)");
    }
    println!();
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Resolve a user-provided ID (full UUID or unique prefix) against the active and
/// queued jobs reported by `GetStatus`.
async fn resolve_active_id(client: &mut IpcClient, input: &str) -> Result<Uuid> {
    if let Ok(uuid) = Uuid::parse_str(input) {
        return Ok(uuid);
    }

    let data = ok_data(client.send(&Request::GetStatus).await?)?;
    let candidates = ["active", "queued"]
        .iter()
        .filter_map(|k| data[k].as_array())
        .flatten()
        .filter_map(|job| job["id"].as_str());
    match_prefix(input, candidates, "active or queued job")
}

/// Resolve a user-provided ID (full UUID or unique prefix) against the catalogued
/// models reported by `ListModels`.
async fn resolve_model_id(client: &mut IpcClient, input: &str) -> Result<Uuid> {
    if let Ok(uuid) = Uuid::parse_str(input) {
        return Ok(uuid);
    }

    let data = ok_data(client.send(&Request::ListModels).await?)?;
    let candidates = data
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str());
    match_prefix(input, candidates, "catalogued model")
}

fn ok_data(response: Response) -> Result<serde_json::Value> {
    match response {
        Response::Ok(data) => Ok(data),
        Response::Err { message } => bail!("daemon error: {message}"),
    }
}

fn match_prefix<'a>(
    input: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    kind: &str,
) -> Result<Uuid> {
    let matches: Vec<Uuid> = candidates
        .into_iter()
        .filter(|s| s.starts_with(input))
        .filter_map(|s| Uuid::parse_str(s).ok())
        .collect();
    match matches.len() {
        0 => bail!("no {kind} matches id '{input}'"),
        1 => Ok(matches[0]),
        n => bail!("id prefix '{input}' is ambiguous ({n} matches); use a longer prefix"),
    }
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

    #[test]
    fn test_select_variant_single_file() {
        let files = vec![FileVariantInfo {
            name: "model.safetensors".to_string(),
            size_kb: 1000.0,
            primary: Some(true),
            format: Some("SafeTensor".to_string()),
            size: None,
            fp: None,
            quant_type: None,
            component_type: None,
        }];
        assert_eq!(
            select_variant(&files),
            Some("model.safetensors".to_string())
        );
    }

    #[test]
    fn test_select_largest_picks_max_size() {
        let files = vec![
            FileVariantInfo {
                name: "small.safetensors".to_string(),
                size_kb: 1000.0,
                primary: Some(true),
                format: None,
                size: None,
                fp: None,
                quant_type: None,
                component_type: None,
            },
            FileVariantInfo {
                name: "large.safetensors".to_string(),
                size_kb: 5000.0,
                primary: Some(false),
                format: None,
                size: None,
                fp: None,
                quant_type: None,
                component_type: None,
            },
        ];
        assert_eq!(
            select_largest(&files),
            Some("large.safetensors".to_string())
        );
    }

    #[test]
    fn test_format_variant_shows_all_metadata() {
        let f = FileVariantInfo {
            name: "model.safetensors".to_string(),
            size_kb: 1024.0,
            primary: Some(true),
            format: Some("SafeTensor".to_string()),
            size: Some("pruned".to_string()),
            fp: Some("fp16".to_string()),
            quant_type: None,
            component_type: None,
        };
        let s = format_variant(&f);
        assert!(s.contains("model.safetensors"));
        assert!(s.contains("SafeTensor"));
        assert!(s.contains("pruned"));
        assert!(s.contains("fp16"));
    }
}
