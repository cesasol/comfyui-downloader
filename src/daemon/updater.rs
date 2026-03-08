use crate::catalog::{Catalog, JobStatus};
use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::daemon::notifier;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tokio::time::{sleep, Duration};
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
    let mut by_model: HashMap<u64, &crate::catalog::DownloadJob> = HashMap::new();
    for job in jobs
        .iter()
        .filter(|j| j.status == JobStatus::Done && j.model_id.is_some() && j.version_id.is_some())
    {
        by_model.entry(job.model_id.unwrap()).or_insert(job);
    }

    for (model_id, job) in &by_model {
        let stored_version_id = job.version_id.unwrap();
        let model = match civitai.get_model(*model_id).await {
            Ok(m) => m,
            Err(e) => {
                warn!("Could not fetch model {model_id}: {e}");
                continue;
            }
        };

        let Some(latest) = model.model_versions.first() else {
            continue;
        };

        if is_newer(latest.id, stored_version_id) {
            info!(
                "Update available for model {model_id}: {} → {}",
                stored_version_id, latest.id
            );
            let cat = catalog.lock().await;
            cat.enqueue_version_update(*model_id, latest.id, job.model_type.as_deref())?;
            drop(cat);
            let _ = notifier::notify_update_available(&model.name, &latest.name);
        } else {
            info!("Model {model_id} is up to date (version {})", stored_version_id);
        }
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
        use tokio::time::{timeout, Duration};

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
}
