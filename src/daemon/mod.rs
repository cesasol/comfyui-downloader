pub mod downloader;
pub mod notifier;
pub mod queue;
pub mod updater;

use crate::catalog::Catalog;
use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::daemon::queue::{ActiveTasks, ProgressMap};
use crate::ipc::{IpcServer, Request, Response};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

pub async fn run() -> Result<()> {
    let config = Arc::new(Config::load()?);
    info!("Loaded config");

    let catalog = Arc::new(Mutex::new(Catalog::open(
        &dirs::data_local_dir()
            .unwrap_or_default()
            .join("comfyui-downloader")
            .join("catalog.db"),
    )?));

    let civitai = Arc::new(CivitaiClient::new(config.civitai.api_key.clone())?);
    let active: ActiveTasks = Arc::new(Mutex::new(HashMap::new()));
    let progress: ProgressMap = Arc::new(Mutex::new(HashMap::new()));

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
        tokio::spawn(async move {
            updater::run(cfg, cat, civ).await;
        })
    };

    let server = IpcServer::bind(&config.daemon.socket_path)?;
    info!("Daemon ready");

    let cat = catalog.clone();
    let act = active.clone();
    let prog = progress.clone();
    server
        .serve(move |req| {
            let cat = cat.clone();
            let act = act.clone();
            let prog = prog.clone();
            async move { handle_request(req, cat, act, prog).await }
        })
        .await?;

    queue_handle.abort();
    updater_handle.abort();
    Ok(())
}

async fn handle_request(
    req: Request,
    catalog: Arc<Mutex<Catalog>>,
    active: ActiveTasks,
    progress: ProgressMap,
) -> Response {
    match req {
        Request::AddDownload { url, model_type } => {
            let cat = catalog.lock().await;
            match cat.enqueue(&url, model_type.as_deref()) {
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
        Request::GetStatus => {
            let queued = {
                let cat = catalog.lock().await;
                cat.count_by_status(crate::catalog::JobStatus::Queued).unwrap_or(0)
            };
            let active_jobs: Vec<serde_json::Value> = {
                let prog = progress.lock().await;
                prog.iter()
                    .map(|(id, p)| serde_json::json!({
                        "id": id,
                        "bytes_received": p.bytes_received,
                        "total_bytes": p.total_bytes,
                    }))
                    .collect()
            };
            let free_bytes = crate::config::Config::load()
                .ok()
                .map(|c| crate::daemon::downloader::free_disk_bytes(&c.paths.models_dir).unwrap_or(0))
                .unwrap_or(0);
            Response::ok(serde_json::json!({
                "queued": queued,
                "active": active_jobs,
                "free_bytes": free_bytes,
            }))
        }
        Request::CheckUpdates => {
            Response::ok(serde_json::json!({ "message": "update check triggered" }))
        }
        Request::Cancel { id } => {
            // Signal the in-flight task if active; otherwise set DB flag directly.
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
