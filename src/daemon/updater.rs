use crate::catalog::{Catalog, DownloadJob, JobStatus};
use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::daemon::notifier;
use std::collections::HashMap;
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
        if let Err(e) = check_updates(&catalog, &civitai).await {
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

        // Established placements are preserved, including files the old
        // routing rules misplaced. A routine update check must never move a
        // file because the placement rules or the aliases have changed since.
    }

    Ok(())
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
