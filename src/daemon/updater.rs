use crate::catalog::{Catalog, DownloadJob, JobStatus};
use crate::civitai::CivitaiClient;
use crate::civitai::types::ModelInfo;
use crate::config::Config;
use crate::daemon::{downloader, notifier};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};

pub async fn run(
    config: Arc<Config>,
    catalog: Arc<Mutex<Catalog>>,
    civitai: Arc<CivitaiClient>,
    wake: Arc<Notify>,
) {
    let interval = Duration::from_secs(config.daemon.update_interval_hours * 3600);
    loop {
        info!("Running update check");
        if let Err(e) = check_updates(&config, &catalog, &civitai).await {
            error!("Update check failed: {e}");
        }
        tokio::select! {
            _ = sleep(interval) => {}
            _ = wake.notified() => {
                info!("Update check woken by CheckUpdates command");
            }
        }
    }
}

/// CivitAI assigns monotonically increasing version IDs.
pub(crate) fn is_newer(latest_id: u64, stored_id: u64) -> bool {
    latest_id > stored_id
}

async fn check_updates(
    config: &Arc<Config>,
    catalog: &Arc<Mutex<Catalog>>,
    civitai: &Arc<CivitaiClient>,
) -> anyhow::Result<()> {
    let jobs = {
        let cat = catalog.lock().await;
        cat.list_jobs()?
    };

    // One representative Done job per model_id.
    let mut by_model: HashMap<u64, &DownloadJob> = HashMap::new();
    for job in jobs
        .iter()
        .filter(|j| j.status == JobStatus::Done && j.model_id.is_some() && j.version_id.is_some())
    {
        by_model.entry(job.model_id.unwrap()).or_insert(job);
    }

    for (model_id, job) in &by_model {
        {
            let cat = catalog.lock().await;
            match cat.should_check_update(*model_id) {
                Ok(false) => {
                    info!("Skipping model {model_id}: checked within last 24h");
                    continue;
                }
                Ok(true) => {}
                Err(e) => {
                    warn!("Failed to check rate limit for model {model_id}: {e}");
                }
            }
        }

        let stored_version_id = job.version_id.unwrap();
        let model = match civitai.get_model(*model_id).await {
            Ok(m) => m,
            Err(e) => {
                warn!("Could not fetch model {model_id}: {e}");
                continue;
            }
        };

        {
            let cat = catalog.lock().await;
            if let Err(e) = cat.set_last_update_check(*model_id) {
                warn!("Failed to set last_update_check for model {model_id}: {e}");
            }
        }

        if let Some(latest) = model.model_versions.first() {
            if is_newer(latest.id, stored_version_id) {
                let stored_version = model
                    .model_versions
                    .iter()
                    .find(|v| v.id == stored_version_id);

                let should_flag = match (stored_version, &latest.base_model) {
                    (Some(stored), Some(latest_base)) => match &stored.base_model {
                        Some(stored_base) => stored_base == latest_base,
                        None => {
                            warn!(
                                "Stored version {} has no base_model, skipping update to avoid type mismatch",
                                stored_version_id
                            );
                            false
                        }
                    },
                    (Some(_), None) => {
                        warn!(
                            "Latest version {} has no base_model, skipping update to avoid type mismatch",
                            latest.id
                        );
                        false
                    }
                    (None, _) => {
                        warn!(
                            "Could not find stored version {} in model {}, proceeding with flag",
                            stored_version_id, model_id
                        );
                        true
                    }
                };

                if should_flag {
                    info!(
                        "Update available for model {model_id}: {} → {} ({})",
                        stored_version_id, latest.id, latest.name
                    );
                    let cat = catalog.lock().await;
                    if let Err(e) = cat.flag_update_available(*model_id, latest.id, &latest.name) {
                        warn!("Failed to flag update for model {model_id}: {e}");
                    }
                    drop(cat);
                    let _ = notifier::notify_update_available(&model.name, &latest.name);
                } else if let (Some(stored), Some(latest_base)) =
                    (stored_version, &latest.base_model)
                    && let Some(stored_base) = &stored.base_model
                {
                    info!(
                        "Skipping update for model {model_id}: base model mismatch ('{}' → '{}')",
                        stored_base, latest_base
                    );
                }
            } else {
                info!(
                    "Model {model_id} is up to date (version {})",
                    stored_version_id
                );
            }
        }

        for done_job in jobs.iter().filter(|j| {
            j.status == JobStatus::Done
                && j.model_id == Some(*model_id)
                && j.version_id.is_some()
                && j.dest_path.is_some()
        }) {
            relocate_if_needed(done_job, &model, config, catalog).await;
        }
    }

    Ok(())
}

async fn relocate_if_needed(
    job: &DownloadJob,
    model: &ModelInfo,
    config: &Config,
    catalog: &Arc<Mutex<Catalog>>,
) {
    let current_path = PathBuf::from(job.dest_path.as_ref().unwrap());
    if !current_path.exists() {
        return;
    }

    let version_id = job.version_id.unwrap();
    let Some(version) = model.model_versions.iter().find(|v| v.id == version_id) else {
        return;
    };
    let mut expected_subdir = model.r#type.models_subdir().to_string();
    if expected_subdir == "checkpoints" {
        let ext = current_path.extension().and_then(|e| e.to_str());
        match ext {
            Some("gguf") => {
                expected_subdir = "diffusion_models".to_string();
            }
            Some("safetensors") => {
                match crate::safetensor::inspect_components(&current_path).await {
                    Ok(c) if !c.has_vae && !c.has_clip => {
                        expected_subdir = "diffusion_models".to_string();
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(
                            "Failed to inspect safetensors header for {}: {e:#}",
                            current_path.display()
                        );
                    }
                }
            }
            _ => {}
        }
    }
    let mut expected_dir = config.paths.models_dir.join(&expected_subdir);
    if let Some(ref base_model) = version.base_model {
        expected_dir = expected_dir.join(downloader::sanitize_dir_name(base_model));
    }

    // Prefer the on-disk filename over file.name — the downloader derives
    // it from the Content-Disposition header which may differ from the API.
    let Some(current_filename) = current_path.file_name() else {
        return;
    };
    let expected_path = expected_dir.join(current_filename);

    if current_path == expected_path {
        return;
    }

    if expected_path.exists() {
        warn!(
            "Cannot relocate {}: target already exists at {}",
            current_path.display(),
            expected_path.display()
        );
        return;
    }

    info!(
        "Relocating {} → {}",
        current_path.display(),
        expected_path.display()
    );

    if let Err(e) = tokio::fs::create_dir_all(&expected_dir).await {
        warn!("Failed to create directory {}: {e}", expected_dir.display());
        return;
    }

    if let Err(e) = tokio::fs::rename(&current_path, &expected_path).await {
        warn!(
            "Failed to move {} → {}: {e}",
            current_path.display(),
            expected_path.display()
        );
        return;
    }

    move_sidecar(&current_path, &expected_path, "metadata.json").await;
    move_preview_sidecars(&current_path, &expected_path).await;

    update_metadata_file_path(&expected_path).await;

    {
        let cat = catalog.lock().await;
        if let Err(e) = cat.set_dest_path(job.id, &expected_path) {
            warn!("Failed to update catalog dest_path: {e}");
        }
    }

    if let Some(old_dir) = current_path.parent() {
        let _ = tokio::fs::remove_dir(old_dir).await;
    }

    let filename = current_filename.to_string_lossy();
    let _ = notifier::notify_file_moved(
        &filename,
        &current_path.display().to_string(),
        &expected_path.display().to_string(),
    );
}

async fn move_sidecar(old_model: &Path, new_model: &Path, extension: &str) {
    let old_sidecar = old_model.with_extension(extension);
    if old_sidecar.exists() {
        let new_sidecar = new_model.with_extension(extension);
        if let Err(e) = tokio::fs::rename(&old_sidecar, &new_sidecar).await {
            warn!(
                "Failed to move sidecar {} → {}: {e}",
                old_sidecar.display(),
                new_sidecar.display()
            );
        }
    }
}

async fn move_preview_sidecars(old_model: &Path, new_model: &Path) {
    let Some(parent) = old_model.parent() else {
        return;
    };
    let Some(stem) = old_model.file_stem() else {
        return;
    };
    let prefix = format!("{}.preview.", stem.to_string_lossy());
    let Ok(mut entries) = tokio::fs::read_dir(parent).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Some(ext) = name_str.strip_prefix(&prefix) {
            let new_preview = new_model.with_extension(format!("preview.{ext}"));
            if let Err(e) = tokio::fs::rename(entry.path(), &new_preview).await {
                warn!("Failed to move preview sidecar: {e}");
            }
        }
    }
}

async fn update_metadata_file_path(new_model_path: &Path) {
    let meta_path = new_model_path.with_extension("metadata.json");
    let Ok(content) = tokio::fs::read_to_string(&meta_path).await else {
        return;
    };
    let Ok(mut meta) = serde_json::from_str::<serde_json::Value>(&content) else {
        return;
    };
    let Some(obj) = meta.as_object_mut() else {
        return;
    };

    obj.insert(
        "file_path".to_string(),
        serde_json::Value::String(new_model_path.to_string_lossy().into_owned()),
    );

    if let Some(stem) = new_model_path.file_stem()
        && let Some(parent) = new_model_path.parent()
    {
        let prefix = format!("{}.preview.", stem.to_string_lossy());
        if let Ok(mut entries) = tokio::fs::read_dir(parent).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    obj.insert(
                        "preview_url".to_string(),
                        serde_json::Value::String(entry.path().to_string_lossy().into_owned()),
                    );
                    break;
                }
            }
        }
    }

    if let Ok(json) = serde_json::to_string_pretty(&meta)
        && let Err(e) = tokio::fs::write(&meta_path, json).await
    {
        warn!(
            "Failed to update metadata file {}: {e}",
            meta_path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_is_newer_version() {
        assert!(super::is_newer(200, 100));
        assert!(!super::is_newer(100, 100));
        assert!(!super::is_newer(50, 100));
    }

    #[tokio::test]
    async fn test_notify_wakes_select() {
        use std::sync::Arc;
        use tokio::sync::Notify;
        use tokio::time::{Duration, timeout};

        let notify = Arc::new(Notify::new());
        let n = notify.clone();

        // Race: notified() vs a 1-hour sleep. After cancel, notified should win.
        let result = timeout(Duration::from_millis(200), async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(3600)) => "sleep",
                _ = n.notified() => "notified",
            }
        });

        notify.notify_one();
        assert_eq!(result.await.unwrap(), "notified");
    }

    fn load_model_info() -> crate::civitai::types::ModelInfo {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/stubs/model_response.stub.json");
        let json = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn test_update_available_from_stub_versions() {
        let info = load_model_info();

        let stored_version_id = 5550001u64;
        let latest = info.model_versions.first().unwrap();
        assert!(latest.id > stored_version_id);
    }

    #[test]
    fn test_no_update_when_already_latest() {
        let info = load_model_info();

        let stored_version_id = 5550003u64;
        let latest = info.model_versions.first().unwrap();
        assert!(latest.id <= stored_version_id);
    }
}
