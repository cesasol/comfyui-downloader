pub mod downloader;
pub mod notifier;
pub mod queue;
pub mod scanner;
pub mod updater;

use crate::catalog::{Catalog, DownloadJob};
use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::daemon::queue::{ActiveTasks, ProgressMap};
use crate::ipc::protocol::EnrichedModel;
use crate::ipc::{IpcServer, Request, Response};
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tracing::{info, warn};

pub async fn run() -> Result<()> {
    let config = Config::load()?;
    config.save()?; // Persist any new fields added since the config was last written.
    info!("Loaded config from {}", Config::config_path().display());
    let config = Arc::new(config);

    let catalog = Arc::new(Mutex::new(Catalog::open(
        &crate::config::xdg_data_home()
            .join("comfyui-downloader")
            .join("catalog.db"),
    )?));

    let civitai = Arc::new(CivitaiClient::new(config.civitai.api_key.clone())?);
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let progress: ProgressMap = Arc::new(Mutex::new(HashMap::new()));
    let update_wake: Arc<Notify> = Arc::new(Notify::new());

    let scanner_handle = {
        let cfg = config.clone();
        let civ = civitai.clone();
        let cat = catalog.clone();
        tokio::spawn(async move {
            scanner::run(cfg, civ, cat).await;
        })
    };

    let queue_handle = {
        let cfg = config.clone();
        let cat = catalog.clone();
        let civ = civitai.clone();
        let act = active.clone();
        let prog = progress.clone();
        tokio::spawn(async move {
            queue::run(cfg, cat, civ, act, prog).await;
        })
    };

    let updater_handle = {
        let cfg = config.clone();
        let cat = catalog.clone();
        let civ = civitai.clone();
        let wake = update_wake.clone();
        tokio::spawn(async move {
            updater::run(cfg, cat, civ, wake).await;
        })
    };

    let server = IpcServer::bind(&config.daemon.socket_path)?;
    info!("Daemon ready");

    let cat = catalog.clone();
    let act = active.clone();
    let prog = progress.clone();
    let wake = update_wake.clone();
    server
        .serve(move |req| {
            let cat = cat.clone();
            let act = act.clone();
            let prog = prog.clone();
            let wake = wake.clone();
            async move { handle_request(req, cat, act, prog, wake).await }
        })
        .await?;

    scanner_handle.abort();
    queue_handle.abort();
    updater_handle.abort();
    Ok(())
}

async fn handle_request(
    req: Request,
    catalog: Arc<Mutex<Catalog>>,
    active: ActiveTasks,
    progress: ProgressMap,
    update_wake: Arc<Notify>,
) -> Response {
    match req {
        Request::AddDownload { url, model_type } => {
            let cat = catalog.lock().await;
            match cat.enqueue(
                &url,
                model_type.as_deref(),
                crate::catalog::DownloadReason::CliAdd,
            ) {
                Ok(job) => Response::ok(job),
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::ListQueue => {
            let cat = catalog.lock().await;
            match cat.list_jobs() {
                Ok(jobs) => Response::ok(jobs),
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::ListModels => {
            let cat = catalog.lock().await;
            match cat.list_done_models() {
                Ok(models) => Response::ok(models),
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::ListModelsEnriched => {
            let cat = catalog.lock().await;
            match cat.list_done_models() {
                Ok(models) => {
                    drop(cat);
                    let enriched = enrich_models(models).await;
                    Response::ok(enriched)
                }
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::DeleteModel { id } => {
            let cat = catalog.lock().await;
            match cat.delete_model(id) {
                Ok(deleted_paths) => {
                    drop(cat);
                    for path in deleted_paths {
                        if let Err(e) = tokio::fs::remove_file(&path).await {
                            warn!("Failed to delete file {}: {}", path.display(), e);
                        }
                    }
                    Response::ok(serde_json::json!({ "deleted": id }))
                }
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::GetStatus => {
            let queued_jobs = {
                let cat = catalog.lock().await;
                cat.list_queued().unwrap_or_default()
            };
            let active_jobs: Vec<serde_json::Value> = {
                let prog = progress.lock().await;
                prog.iter()
                    .map(|(id, p)| {
                        serde_json::json!({
                            "id": id,
                            "bytes_received": p.bytes_received,
                            "total_bytes": p.total_bytes,
                            "model_name": p.model_name,
                            "dest_path": p.dest_path,
                            "model_type": p.model_type,
                            "download_reason": p.download_reason,
                            "started_at": p.started_at,
                        })
                    })
                    .collect()
            };
            let queued_info: Vec<serde_json::Value> = queued_jobs
                .iter()
                .map(|j| {
                    serde_json::json!({
                        "id": j.id,
                        "url": j.url,
                        "model_type": j.model_type,
                        "download_reason": j.download_reason.to_string(),
                    })
                })
                .collect();
            let free_bytes = crate::config::Config::load()
                .ok()
                .map(|c| {
                    crate::daemon::downloader::free_disk_bytes(&c.paths.models_dir).unwrap_or(0)
                })
                .unwrap_or(0);
            Response::ok(serde_json::json!({
                "queued": queued_info.len(),
                "queued_jobs": queued_info,
                "active": active_jobs,
                "free_bytes": free_bytes,
            }))
        }
        Request::CheckUpdates => {
            update_wake.notify_one();
            Response::ok(serde_json::json!({ "message": "update check triggered" }))
        }
        Request::Cancel { id } => {
            let cancelled = {
                let tasks = active.lock().await;
                if let Some(token) = tasks.get(&id) {
                    token.cancel();
                    true
                } else {
                    false
                }
            };
            if cancelled {
                Response::ok(serde_json::json!({ "cancelled": id }))
            } else {
                let cat = catalog.lock().await;
                match cat.set_status(id, crate::catalog::JobStatus::Cancelled, None) {
                    Ok(()) => Response::ok(serde_json::json!({ "cancelled": id })),
                    Err(e) => Response::err(e.to_string()),
                }
            }
        }
    }
}

async fn enrich_models(models: Vec<DownloadJob>) -> Vec<EnrichedModel> {
    let mut enriched = Vec::with_capacity(models.len());
    for job in models {
        let metadata = match job.dest_path.as_deref() {
            Some(dest) => read_sidecar_metadata(Path::new(dest)).await,
            None => None,
        };
        let (model_name, base_model, preview_path, preview_nsfw_level, file_size, sha256) =
            match metadata {
                Some(meta) => (
                    meta.get("model_name")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    meta.get("base_model")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    meta.get("preview_url")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    meta.get("preview_nsfw_level")
                        .and_then(|v| v.as_u64())
                        .map(|n| n as u32),
                    meta.get("size").and_then(|v| v.as_u64()),
                    meta.get("sha256")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                ),
                None => (None, None, None, None, None, None),
            };
        enriched.push(EnrichedModel {
            id: job.id,
            url: job.url,
            model_id: job.model_id,
            version_id: job.version_id,
            model_type: job.model_type,
            dest_path: job.dest_path,
            created_at: job.created_at,
            updated_at: job.updated_at,
            model_name,
            base_model,
            preview_path,
            preview_nsfw_level,
            file_size,
            sha256,
        });
    }
    enriched
}

async fn read_sidecar_metadata(model_path: &Path) -> Option<serde_json::Value> {
    let meta_path = model_path.with_extension("metadata.json");
    let bytes = tokio::fs::read(&meta_path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}
