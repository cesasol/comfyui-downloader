pub mod downloader;
pub mod notifier;
pub mod queue;
pub mod scanner;
pub mod store;
#[cfg(feature = "tray-icon")]
pub mod systray;
pub mod updater;

use crate::catalog::{Catalog, DownloadJob};
use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::daemon::queue::{ActiveTasks, ProgressMap};
use crate::ipc::protocol::EnrichedModel;
use crate::ipc::protocol::{ActiveJob, QueuedJob, Snapshot};
use crate::ipc::{IpcServer, Request, Response};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tracing::{info, warn};

pub async fn run() -> Result<()> {
    let mut config = Config::load()?;
    // Credentials live in the Secret Service; this also migrates any plaintext
    // ones left in config.toml by an older version.
    config.resolve_credentials().await?;
    if config.initialise_model_families(crate::placement::DEFAULT_FAMILY_ALIASES) {
        info!("Initialised the model-family alias snapshot");
    }
    config.save()?; // Persist any new fields added since the config was last written.
    info!("Loaded config from {}", Config::config_path().display());
    let config = Arc::new(config);

    // Initialize system tray state (no-op if feature not enabled)
    #[cfg(feature = "tray-icon")]
    let tray_state: Arc<crate::daemon::systray::TrayState> = {
        use crate::daemon::systray::TrayState;
        Arc::new(TrayState::new())
    };

    let catalog = Arc::new(Mutex::new(Catalog::open(
        &crate::config::xdg_data_home()
            .join("comfyui-downloader")
            .join("catalog.db"),
    )?));

    // Collapse any duplicate `done` rows accumulated by older daemon versions
    // before they get a chance to trigger redundant downloads (e.g. via
    // `redownload-missing`).
    {
        let cat = catalog.lock().await;
        match cat.dedupe_done_jobs() {
            Ok(0) => {}
            Ok(n) => info!("Removed {n} duplicate done row(s) from catalog at startup"),
            Err(e) => warn!("Startup dedupe of done rows failed: {e:#}"),
        }
        match cat.cancel_redundant_pending_jobs() {
            Ok(0) => {}
            Ok(n) => info!("Cancelled {n} pending job(s) whose version is already completed"),
            Err(e) => warn!("Startup cancellation of redundant pending jobs failed: {e:#}"),
        }
        // Recover jobs stranded in `downloading`/`verifying` by a previous
        // daemon that exited mid-download; otherwise they sit at 0% forever
        // because the worker only ever selects `queued` rows.
        match cat.requeue_interrupted() {
            Ok(0) => {}
            Ok(n) => info!("Re-queued {n} interrupted download(s) from a previous run"),
            Err(e) => warn!("Startup re-queue of interrupted downloads failed: {e:#}"),
        }
    }

    let civitai = Arc::new(CivitaiClient::new(
        config.civitai_api_key().map(str::to_owned),
    )?);
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

    // Spawn system tray icon (if feature and config enabled)
    #[cfg(feature = "tray-icon")]
    let tray_handle = if config.daemon.enable_tray_icon {
        use crate::daemon::systray::spawn_tray_icon;
        let cfg = config.clone();
        let cat = catalog.clone();
        let state = tray_state.clone();
        match spawn_tray_icon(cfg, cat, state) {
            Ok(handle) => {
                info!("System tray icon started");
                Some(handle)
            }
            Err(e) => {
                warn!("Failed to start system tray icon: {e:#}");
                None
            }
        }
    } else {
        info!("System tray icon disabled in config");
        None
    };

    // Spawn tray state watcher to update counts
    #[cfg(feature = "tray-icon")]
    let tray_watcher = {
        use crate::daemon::systray::watch_catalog;
        let cat = catalog.clone();
        let state = tray_state.clone();
        tokio::spawn(async move {
            watch_catalog(cat, state).await;
        })
    };

    let server = IpcServer::bind(&config.daemon.socket_path)?;
    info!("Daemon ready");

    let cat_h = catalog.clone();
    let act_h = active.clone();
    let prog_h = progress.clone();
    let wake_h = update_wake.clone();
    let models_dir_h = config.paths.models_dir.clone();
    let civ_h = civitai.clone();
    let cfg_h = config.clone();

    server
        .serve(move |req| {
            let cat = cat_h.clone();
            let act = act_h.clone();
            let prog = prog_h.clone();
            let wake = wake_h.clone();
            let models_dir = models_dir_h.clone();
            let civ = civ_h.clone();
            let cfg = cfg_h.clone();
            async move { handle_request(req, cat, act, prog, wake, &models_dir, civ, cfg).await }
        })
        .await?;

    scanner_handle.abort();
    queue_handle.abort();
    updater_handle.abort();

    // Cleanup system tray
    #[cfg(feature = "tray-icon")]
    {
        use std::sync::atomic::Ordering;
        if let Some(handle) = tray_handle {
            // Signal the tray icon thread to exit
            tray_state.should_exit.store(true, Ordering::Relaxed);
            // Wait for the thread to finish
            let _ = handle.join();
        }
        tray_watcher.abort();
    }

    Ok(())
}

/// Model files present on disk that no catalog row names.
fn untracked_models(
    catalog: &Catalog,
    models_dir: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    const MODEL_EXTENSIONS: [&str; 7] = ["safetensors", "gguf", "ckpt", "pt", "pth", "bin", "onnx"];
    let tracked: std::collections::HashSet<String> = catalog
        .list_jobs()?
        .into_iter()
        .filter_map(|job| job.dest_path)
        .collect();
    let mut untracked = Vec::new();
    let mut stack = vec![models_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() => {
                    let is_model = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| MODEL_EXTENSIONS.contains(&e));
                    if is_model && !tracked.contains(&path.to_string_lossy().to_string()) {
                        untracked.push(path.to_string_lossy().into_owned());
                    }
                }
                _ => {}
            }
        }
    }
    untracked.sort();
    Ok(untracked)
}

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    req: Request,
    catalog: Arc<Mutex<Catalog>>,
    active: ActiveTasks,
    progress: ProgressMap,
    update_wake: Arc<Notify>,
    models_dir: &std::path::Path,
    civitai: Arc<crate::civitai::CivitaiClient>,
    config: Arc<crate::config::Config>,
) -> Response {
    match req {
        Request::AddDownload {
            url,
            model_type,
            preferred_file_name,
            family,
        } => {
            let cat = catalog.lock().await;
            let overrides = crate::catalog::PlacementOverrides {
                user_role: model_type.clone(),
                user_family: family,
                ..Default::default()
            };
            match cat.enqueue_with_placement(
                &url,
                model_type.as_deref(),
                crate::catalog::DownloadReason::CliAdd,
                preferred_file_name.as_deref(),
                &overrides,
            ) {
                Ok(job) => Response::ok(job),
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::AddDownloads { items } => {
            let cat = catalog.lock().await;
            let mut jobs = Vec::new();
            let mut errors = Vec::new();
            for item in &items {
                let overrides = crate::catalog::PlacementOverrides {
                    template_role: item.model_type.clone(),
                    user_family: item.family.clone(),
                    template_families: item.template_families.clone(),
                    ..Default::default()
                };
                match cat.enqueue_with_placement(
                    &item.url,
                    item.model_type.as_deref(),
                    crate::catalog::DownloadReason::CliAdd,
                    None,
                    &overrides,
                ) {
                    Ok(job) => jobs.push(job),
                    Err(e) => errors.push(format!("{}: {e}", item.url)),
                }
            }
            if jobs.is_empty() && !errors.is_empty() {
                Response::err(errors.join("; "))
            } else {
                Response::ok(serde_json::json!({ "queued": jobs, "errors": errors }))
            }
        }
        Request::ListTemplates {
            filter,
            refresh,
            include_unrunnable,
        } => match list_templates(&config, filter, refresh, include_unrunnable).await {
            Ok(listing) => Response::ok(listing),
            Err(e) => Response::err(format!("{e:#}")),
        },
        Request::GetVersionInfo { url } => {
            let (model_id, version_id) = crate::catalog::parse_civitai_url(&url);
            let result = async {
                let version = if let Some(vid) = version_id {
                    civitai.get_model_version(vid).await?
                } else if let Some(mid) = model_id {
                    let model_info = civitai.get_model(mid).await?;
                    let latest = model_info
                        .model_versions
                        .iter()
                        .find(|v| {
                            !config.daemon.skip_early_access
                                || v.availability.as_deref() != Some("EarlyAccess")
                        })
                        .context("no publicly available version")?;
                    civitai.get_model_version(latest.id).await?
                } else {
                    anyhow::bail!("URL does not contain a model or version ID")
                };

                let files: Vec<crate::ipc::protocol::FileVariantInfo> = version
                    .files
                    .iter()
                    .map(|f| crate::ipc::protocol::FileVariantInfo {
                        name: f.name.clone(),
                        size_kb: f.size_kb,
                        primary: f.primary,
                        format: f.metadata.as_ref().and_then(|m| m.format.clone()),
                        size: f.metadata.as_ref().and_then(|m| m.size.clone()),
                        fp: f.metadata.as_ref().and_then(|m| m.fp.clone()),
                        quant_type: f.metadata.as_ref().and_then(|m| m.quant_type.clone()),
                        component_type: f.metadata.as_ref().and_then(|m| m.component_type.clone()),
                    })
                    .collect();

                Ok(crate::ipc::protocol::VersionInfo {
                    version_id: version.id,
                    version_name: version.name.clone(),
                    base_model: version.base_model.clone(),
                    files,
                })
            }
            .await;

            match result {
                Ok(info) => Response::ok(info),
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
                    let mut failures = Vec::new();
                    for path in deleted_paths {
                        match tokio::fs::remove_file(&path).await {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => {
                                warn!("Failed to delete file {}: {}", path.display(), e);
                                failures.push(format!("{}: {e}", path.display()));
                            }
                        }
                    }
                    if failures.is_empty() {
                        let cat = catalog.lock().await;
                        if let Err(e) = cat.forget_model(id) {
                            return Response::err(format!("removing catalog row: {e}"));
                        }
                        Response::ok(serde_json::json!({ "deleted": id }))
                    } else {
                        Response::err(format!(
                            "kept the catalog entry because files could not be removed: {}",
                            failures.join("; ")
                        ))
                    }
                }
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::Diagnose { repair } => {
            let cat = catalog.lock().await;
            let report = match cat.diagnose() {
                Ok(report) => report,
                Err(e) => return Response::err(format!("{e:#}")),
            };
            let repaired = if repair {
                match cat.repair() {
                    Ok(outcome) => Some(outcome),
                    Err(e) => return Response::err(format!("{e:#}")),
                }
            } else {
                None
            };
            let untracked = untracked_models(&cat, models_dir).unwrap_or_default();
            Response::ok(serde_json::json!({
                "dangling": report.dangling,
                "duplicate_paths": report.duplicate_paths,
                "untracked": untracked,
                "repaired": repaired,
            }))
        }
        Request::GetStatus => {
            let snap = build_snapshot(&catalog, &progress, models_dir).await;
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
                Response::ok(serde_json::json!({ "cancelled": id }))
            } else {
                let cat = catalog.lock().await;
                match cat.set_status(id, crate::catalog::JobStatus::Cancelled, None) {
                    Ok(()) => Response::ok(serde_json::json!({ "cancelled": id })),
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
                Ok(jobs) => Response::ok(serde_json::json!({
                    "requeued": jobs.len(),
                    "jobs": jobs,
                })),
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::DownloadVersion {
            model_id,
            version_id,
        } => {
            let cat = catalog.lock().await;
            let url = format!("https://civitai.com/models/{model_id}?modelVersionId={version_id}");
            match cat.enqueue(&url, None, crate::catalog::DownloadReason::CliAdd, None) {
                Ok(job) => {
                    let _ = cat.clear_update_flag(model_id);
                    Response::ok(job)
                }
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::RedownloadModel { id } => {
            let cat = catalog.lock().await;
            match cat.requeue_one(id) {
                Ok(job) => Response::ok(job),
                Err(e) => Response::err(e.to_string()),
            }
        }
    }
}

/// Fetch the ComfyUI template catalog, resolve the model bundles of everything
/// matching `filter`, and judge each bundle against the local GPU.
async fn list_templates(
    config: &crate::config::Config,
    filter: crate::templates::TemplateFilter,
    refresh: bool,
    include_unrunnable: bool,
) -> anyhow::Result<crate::ipc::protocol::TemplateListing> {
    let gpu = crate::gpu::primary_gpu();
    let vram_bytes = config
        .gpu
        .vram_bytes
        .or_else(|| gpu.as_ref().map(|g| g.vram_bytes));

    let cache_dir = crate::config::xdg_cache_home()
        .join("comfyui-downloader")
        .join("templates");
    let catalog = crate::templates::TemplateCatalog::new(cache_dir)?;
    let entries: Vec<crate::templates::TemplateEntry> = catalog
        .index(refresh)
        .await?
        .into_iter()
        .filter(|entry| filter.matches(entry))
        .collect();

    let hf = crate::huggingface::HfClient::new(config.huggingface_token().map(str::to_owned))?;
    let resolved = catalog.bundles(entries, &hf, vram_bytes, refresh).await?;
    let (bundles, hidden_unrunnable) =
        crate::templates::prune_bundles(resolved, include_unrunnable);

    Ok(crate::ipc::protocol::TemplateListing {
        gpu,
        vram_bytes,
        hidden_unrunnable,
        bundles,
    })
}

async fn build_snapshot(
    catalog: &Arc<Mutex<Catalog>>,
    progress: &ProgressMap,
    models_dir: &std::path::Path,
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
    }
}

async fn enrich_models(models: Vec<DownloadJob>) -> Vec<EnrichedModel> {
    let mut enriched = Vec::with_capacity(models.len());
    for job in models {
        let metadata = match job.dest_path.as_deref() {
            Some(dest) => read_sidecar_metadata(Path::new(dest)).await,
            None => None,
        };
        let (
            model_name,
            version_name,
            base_model,
            preview_path,
            preview_nsfw_level,
            file_size,
            sha256,
        ) = match metadata {
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
        let catalog = Arc::new(Mutex::new(Catalog::open(&db_path).expect("open catalog")));
        let queued_job = {
            let cat = catalog.lock().await;
            cat.enqueue(
                "https://civitai.com/models/42?modelVersionId=99",
                Some("loras"),
                DownloadReason::CliAdd,
                None,
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

        let snap = build_snapshot(&catalog, &progress, tmp.path()).await;

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
