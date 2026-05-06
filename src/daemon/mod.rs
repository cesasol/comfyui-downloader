pub mod downloader;
pub mod events;
pub mod notifier;
pub mod queue;
pub mod scanner;
pub mod updater;

use crate::catalog::{Catalog, DownloadJob};
use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::daemon::queue::{ActiveTasks, ProgressMap};
use crate::ipc::protocol::EnrichedModel;
use crate::ipc::protocol::{ActiveJob, QueuedJob, Snapshot};
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
    let event_bus: crate::daemon::events::EventBus = crate::daemon::events::new_bus();

    let scanner_handle = {
        let cfg = config.clone();
        let civ = civitai.clone();
        let cat = catalog.clone();
        let bus = event_bus.clone();
        tokio::spawn(async move {
            scanner::run(cfg, civ, cat, bus).await;
        })
    };

    let queue_handle = {
        let cfg = config.clone();
        let cat = catalog.clone();
        let civ = civitai.clone();
        let act = active.clone();
        let prog = progress.clone();
        let bus = event_bus.clone();
        tokio::spawn(async move {
            queue::run(cfg, cat, civ, act, prog, bus).await;
        })
    };

    let tick_handle = {
        let bus = event_bus.clone();
        let prog = progress.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if !prog.lock().await.is_empty() {
                    let _ = bus.send(crate::daemon::events::Event::ProgressTick);
                }
            }
        })
    };

    let updater_handle = {
        let cfg = config.clone();
        let cat = catalog.clone();
        let civ = civitai.clone();
        let wake = update_wake.clone();
        let bus = event_bus.clone();
        tokio::spawn(async move {
            updater::run(cfg, cat, civ, wake, bus).await;
        })
    };

    let server = IpcServer::bind(&config.daemon.socket_path)?;
    info!("Daemon ready");

    let cat_h = catalog.clone();
    let act_h = active.clone();
    let prog_h = progress.clone();
    let wake_h = update_wake.clone();
    let bus_h = event_bus.clone();
    let models_dir_h = config.paths.models_dir.clone();

    let cat_s = catalog.clone();
    let prog_s = progress.clone();
    let bus_s = event_bus.clone();

    server
        .serve(
            move |req| {
                let cat = cat_h.clone();
                let act = act_h.clone();
                let prog = prog_h.clone();
                let wake = wake_h.clone();
                let bus = bus_h.clone();
                let models_dir = models_dir_h.clone();
                async move { handle_request(req, cat, act, prog, wake, bus, &models_dir).await }
            },
            move |writer| {
                let cat = cat_s.clone();
                let prog = prog_s.clone();
                let bus = bus_s.clone();
                async move { run_subscribe(writer, cat, prog, bus).await }
            },
        )
        .await?;

    scanner_handle.abort();
    queue_handle.abort();
    tick_handle.abort();
    updater_handle.abort();
    Ok(())
}

async fn handle_request(
    req: Request,
    catalog: Arc<Mutex<Catalog>>,
    active: ActiveTasks,
    progress: ProgressMap,
    update_wake: Arc<Notify>,
    bus: crate::daemon::events::EventBus,
    models_dir: &std::path::Path,
) -> Response {
    match req {
        Request::AddDownload { url, model_type } => {
            let cat = catalog.lock().await;
            match cat.enqueue(
                &url,
                model_type.as_deref(),
                crate::catalog::DownloadReason::CliAdd,
            ) {
                Ok(job) => {
                    let _ = bus.send(crate::daemon::events::Event::CatalogChanged);
                    let _ = bus.send(crate::daemon::events::Event::QueueChanged);
                    Response::ok(job)
                }
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
                    let _ = bus.send(crate::daemon::events::Event::CatalogChanged);
                    Response::ok(serde_json::json!({ "deleted": id }))
                }
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::GetStatus => {
            let snap = build_snapshot(&catalog, &progress, models_dir, false, false, 0).await;
            Response::ok(snap)
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
                let _ = bus.send(crate::daemon::events::Event::QueueChanged);
                Response::ok(serde_json::json!({ "cancelled": id }))
            } else {
                let cat = catalog.lock().await;
                match cat.set_status(id, crate::catalog::JobStatus::Cancelled, None) {
                    Ok(()) => {
                        let _ = bus.send(crate::daemon::events::Event::QueueChanged);
                        Response::ok(serde_json::json!({ "cancelled": id }))
                    }
                    Err(e) => Response::err(e.to_string()),
                }
            }
        }
        Request::ListUpdates => {
            let cat = catalog.lock().await;
            match cat.list_updates_available() {
                Ok(updates) => Response::ok(updates),
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::RedownloadMissing { all } => {
            let cat = catalog.lock().await;
            match cat.requeue_done(!all) {
                Ok(jobs) => {
                    let _ = bus.send(crate::daemon::events::Event::CatalogChanged);
                    let _ = bus.send(crate::daemon::events::Event::QueueChanged);
                    Response::ok(serde_json::json!({
                        "requeued": jobs.len(),
                        "jobs": jobs,
                    }))
                }
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::DownloadVersion {
            model_id,
            version_id,
        } => {
            let cat = catalog.lock().await;
            let url = format!("https://civitai.com/models/{model_id}?modelVersionId={version_id}");
            match cat.enqueue(&url, None, crate::catalog::DownloadReason::CliAdd) {
                Ok(job) => {
                    let _ = cat.clear_update_flag(model_id);
                    let _ = bus.send(crate::daemon::events::Event::CatalogChanged);
                    let _ = bus.send(crate::daemon::events::Event::QueueChanged);
                    Response::ok(job)
                }
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::RedownloadModel { id } => {
            let cat = catalog.lock().await;
            match cat.requeue_one(id) {
                Ok(job) => {
                    let _ = bus.send(crate::daemon::events::Event::CatalogChanged);
                    let _ = bus.send(crate::daemon::events::Event::QueueChanged);
                    Response::ok(job)
                }
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::Subscribe => {
            Response::err("subscribe is a streaming variant; not yet implemented in this build")
        }
    }
}

pub(crate) async fn run_subscribe(
    mut writer: crate::ipc::server::SubscribeWriter,
    _catalog: Arc<Mutex<Catalog>>,
    _progress: ProgressMap,
    _bus: crate::daemon::events::EventBus,
) {
    use crate::ipc::protocol::Frame;
    let _ = writer.send(&Frame::Subscribed).await;
    // Real implementation lands in Task 6.
}

async fn build_snapshot(
    catalog: &Arc<Mutex<Catalog>>,
    progress: &ProgressMap,
    models_dir: &std::path::Path,
    catalog_dirty: bool,
    updates_dirty: bool,
    seq: u64,
) -> Snapshot {
    let queued_jobs = {
        let cat = catalog.lock().await;
        cat.list_queued().unwrap_or_default()
    };
    let active = {
        let prog = progress.lock().await;
        prog.iter()
            .map(|(id, p)| ActiveJob {
                id: *id,
                model_name: p.model_name.clone(),
                version_name: p.version_name.clone(),
                model_type: p.model_type.clone(),
                bytes_received: p.bytes_received,
                total_bytes: p.total_bytes,
                dest_path: p.dest_path.clone(),
                started_at: p.started_at,
                download_reason: p.download_reason.clone(),
            })
            .collect()
    };
    let queued = queued_jobs
        .into_iter()
        .map(|j| QueuedJob {
            id: j.id,
            url: j.url,
            model_name: None,
            version_name: None,
            model_type: j.model_type,
            download_reason: Some(j.download_reason.to_string()),
        })
        .collect();
    let free_bytes = crate::daemon::downloader::free_disk_bytes(models_dir).unwrap_or(0);

    Snapshot {
        active,
        queued,
        free_bytes,
        catalog_dirty,
        updates_dirty,
        seq,
    }
}

async fn enrich_models(models: Vec<DownloadJob>) -> Vec<EnrichedModel> {
    let mut enriched = Vec::with_capacity(models.len());
    for job in models {
        let metadata = match job.dest_path.as_deref() {
            Some(dest) => read_sidecar_metadata(Path::new(dest)).await,
            None => None,
        };
        let (model_name, version_name, base_model, preview_path, preview_nsfw_level, file_size, sha256) =
            match metadata {
                Some(meta) => (
                    meta.get("model_name")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    meta.get("version_name")
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
                None => (None, None, None, None, None, None, None),
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
            version_name,
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

#[cfg(test)]
mod tests {
    use super::{build_snapshot, read_sidecar_metadata};
    use crate::catalog::{Catalog, DownloadReason};
    use crate::daemon::queue::{DownloadProgress, ProgressMap};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    /// Verify that `read_sidecar_metadata` correctly reads and returns the
    /// `version_name` key from a sidecar JSON file written next to the model
    /// file.  This test locks down the key-name contract between the writer
    /// (`downloader::ModelMetadata`) and the reader (`enrich_models`).
    #[tokio::test]
    async fn test_sidecar_version_name_round_trip() {
        let dir = tempfile::tempdir().expect("create temp dir");
        // The function derives the sidecar path by replacing the model file's
        // extension with "metadata.json", so we point it at a fake model file.
        let model_path: PathBuf = dir.path().join("mymodel.safetensors");
        let sidecar_path = model_path.with_extension("metadata.json");

        let sidecar = serde_json::json!({
            "file_name": "mymodel",
            "model_name": null,
            "version_name": "better_hands",
            "file_path": model_path.to_str().unwrap(),
            "size": 0,
            "modified": 0.0,
            "sha256": "abc123",
            "base_model": null,
            "notes": "",
            "from_civitai": false
        });
        tokio::fs::write(&sidecar_path, serde_json::to_vec_pretty(&sidecar).unwrap())
            .await
            .expect("write sidecar");

        let meta = read_sidecar_metadata(&model_path)
            .await
            .expect("sidecar should be readable");

        let version_name = meta
            .get("version_name")
            .and_then(|v| v.as_str())
            .map(String::from);
        assert_eq!(version_name, Some("better_hands".to_string()));
    }

    /// When `version_name` is serialized as `null` (no `skip_serializing_if`),
    /// the reader must still return `None` gracefully — not panic or return a
    /// stray `"null"` string.
    #[tokio::test]
    async fn test_sidecar_version_name_null_returns_none() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let model_path: PathBuf = dir.path().join("mymodel.safetensors");
        let sidecar_path = model_path.with_extension("metadata.json");

        let sidecar = serde_json::json!({
            "file_name": "mymodel",
            "model_name": null,
            "version_name": null,
            "file_path": model_path.to_str().unwrap(),
            "size": 0,
            "modified": 0.0,
            "sha256": "abc123",
            "base_model": null,
            "notes": "",
            "from_civitai": false
        });
        tokio::fs::write(&sidecar_path, serde_json::to_vec_pretty(&sidecar).unwrap())
            .await
            .expect("write sidecar");

        let meta = read_sidecar_metadata(&model_path)
            .await
            .expect("sidecar should be readable");

        let version_name = meta
            .get("version_name")
            .and_then(|v| v.as_str())
            .map(String::from);
        assert_eq!(version_name, None);
    }

    /// `build_snapshot` must correctly populate `active` from the ProgressMap
    /// and `queued` from the catalog, without re-reading config from disk.
    #[tokio::test]
    async fn test_build_snapshot_active_and_queued() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let db_path = tmp.path().join("catalog.db");

        // Open a fresh catalog and enqueue one queued job.
        let catalog = Arc::new(Mutex::new(
            Catalog::open(&db_path).expect("open catalog"),
        ));
        let queued_job = {
            let cat = catalog.lock().await;
            cat.enqueue(
                "https://civitai.com/models/42?modelVersionId=99",
                Some("loras"),
                DownloadReason::CliAdd,
            )
            .expect("enqueue")
        };

        // Build a ProgressMap with one synthetic active job.
        let active_id = Uuid::new_v4();
        let progress: ProgressMap = Arc::new(Mutex::new({
            let mut m = HashMap::new();
            m.insert(
                active_id,
                DownloadProgress {
                    bytes_received: 1024,
                    total_bytes: Some(4096),
                    model_name: Some("TestModel".into()),
                    version_name: Some("v1".into()),
                    dest_path: Some("/tmp/test.safetensors".into()),
                    model_type: Some("checkpoints".into()),
                    download_reason: Some("cli_add".into()),
                    started_at: None,
                },
            );
            m
        }));

        let snap = build_snapshot(&catalog, &progress, tmp.path(), false, false, 7).await;

        // Sequence number must be propagated.
        assert_eq!(snap.seq, 7);
        assert!(!snap.catalog_dirty);
        assert!(!snap.updates_dirty);

        // Active jobs: exactly the one we put in the ProgressMap.
        assert_eq!(snap.active.len(), 1);
        let active_entry = &snap.active[0];
        assert_eq!(active_entry.id, active_id);
        assert_eq!(active_entry.model_name.as_deref(), Some("TestModel"));
        assert_eq!(active_entry.version_name.as_deref(), Some("v1"));
        assert_eq!(active_entry.model_type.as_deref(), Some("checkpoints"));

        // Queued jobs: exactly the one we enqueued.
        assert_eq!(snap.queued.len(), 1);
        let queued_entry = &snap.queued[0];
        assert_eq!(queued_entry.id, queued_job.id);
        // Queued jobs are not enriched yet — names stay None.
        assert!(queued_entry.model_name.is_none());
        assert!(queued_entry.version_name.is_none());
        assert_eq!(queued_entry.model_type.as_deref(), Some("loras"));
        assert_eq!(
            queued_entry.download_reason.as_deref(),
            Some("cli_add"),
            "download_reason must be DownloadReason::CliAdd.to_string()"
        );
    }
}
