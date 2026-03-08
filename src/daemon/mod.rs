pub mod downloader;
pub mod notifier;
pub mod queue;
pub mod updater;

use crate::catalog::Catalog;
use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::ipc::{IpcServer, Request, Response};
use anyhow::Result;
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
    let cfg = config.clone();
    let cat = catalog.clone();
    let civ = civitai.clone();

    // Spawn the download queue worker.
    let queue_handle = {
        let cfg = config.clone();
        let cat = catalog.clone();
        let civ = civitai.clone();
        tokio::spawn(async move {
            queue::run(cfg, cat, civ).await;
        })
    };

    // Spawn the periodic update checker.
    let updater_handle = {
        let cfg = config.clone();
        let cat = catalog.clone();
        let civ = civitai.clone();
        tokio::spawn(async move {
            updater::run(cfg, cat, civ).await;
        })
    };

    // Bind IPC socket and serve.
    let server = IpcServer::bind(&config.daemon.socket_path)?;
    info!("Daemon ready");

    server
        .serve(move |req| {
            let cat = cat.clone();
            async move { handle_request(req, cat).await }
        })
        .await?;

    queue_handle.abort();
    updater_handle.abort();
    Ok(())
}

async fn handle_request(
    req: Request,
    catalog: Arc<Mutex<Catalog>>,
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
        Request::GetStatus => Response::ok(serde_json::json!({ "status": "running" })),
        Request::CheckUpdates => {
            // Signal handled by the updater task; for now acknowledge.
            Response::ok(serde_json::json!({ "message": "update check triggered" }))
        }
        Request::Cancel { id } => {
            let cat = catalog.lock().await;
            match cat.set_status(id, crate::catalog::JobStatus::Cancelled, None) {
                Ok(()) => Response::ok(serde_json::json!({ "cancelled": id })),
                Err(e) => Response::err(e.to_string()),
            }
        }
    }
}
