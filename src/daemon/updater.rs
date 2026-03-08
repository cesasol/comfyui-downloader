use crate::catalog::Catalog;
use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::daemon::notifier;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

pub async fn run(
    config: Arc<Config>,
    catalog: Arc<Mutex<Catalog>>,
    civitai: Arc<CivitaiClient>,
) {
    let interval = Duration::from_secs(config.daemon.update_interval_hours * 3600);
    loop {
        info!("Running update check");
        if let Err(e) = check_updates(&catalog, &civitai).await {
            error!("Update check failed: {e}");
        }
        sleep(interval).await;
    }
}

async fn check_updates(
    catalog: &Arc<Mutex<Catalog>>,
    _civitai: &Arc<CivitaiClient>,
) -> anyhow::Result<()> {
    // TODO: iterate done jobs that have a version_id, query CivitAI for the
    // latest version of each model_id, compare, and notify if newer.
    let jobs = {
        let cat = catalog.lock().await;
        cat.list_jobs()?
    };

    for job in jobs.iter().filter(|j| j.version_id.is_some()) {
        let _model_id = match job.model_id {
            Some(id) => id,
            None => continue,
        };
        // Placeholder: real implementation queries civitai.get_model(model_id)
        // and compares model.model_versions[0].id with job.version_id.
        info!("Checked job {} — no update logic yet", job.id);
    }
    Ok(())
}
