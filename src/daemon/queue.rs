use crate::catalog::{Catalog, JobStatus};
use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::daemon::downloader;
use crate::daemon::notifier;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

pub async fn run(
    config: Arc<Config>,
    catalog: Arc<Mutex<Catalog>>,
    civitai: Arc<CivitaiClient>,
) {
    loop {
        let job = {
            let cat = catalog.lock().await;
            cat.next_queued().unwrap_or(None)
        };

        if let Some(job) = job {
            info!("Starting download job {}", job.id);
            {
                let cat = catalog.lock().await;
                let _ = cat.set_status(job.id, JobStatus::Downloading, None);
            }

            match downloader::download(&job, &config, &civitai).await {
                Ok(dest) => {
                    info!("Job {} complete: {}", job.id, dest.display());
                    let cat = catalog.lock().await;
                    let _ = cat.set_status(job.id, JobStatus::Done, None);
                    let _ = notifier::notify_success(&dest.display().to_string());
                }
                Err(e) => {
                    error!("Job {} failed: {e}", job.id);
                    let cat = catalog.lock().await;
                    let _ = cat.set_status(job.id, JobStatus::Failed, Some(&e.to_string()));
                    let _ = notifier::notify_error(&e.to_string());
                }
            }
        } else {
            sleep(Duration::from_secs(5)).await;
        }
    }
}
