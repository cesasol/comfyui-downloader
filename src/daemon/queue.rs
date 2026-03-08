use crate::catalog::{Catalog, JobStatus};
use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::daemon::downloader;
use crate::daemon::notifier;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use uuid::Uuid;

/// Map of active download job IDs to their cancellation tokens.
pub type ActiveTasks = Arc<Mutex<HashMap<Uuid, CancellationToken>>>;

pub async fn run(
    config: Arc<Config>,
    catalog: Arc<Mutex<Catalog>>,
    civitai: Arc<CivitaiClient>,
    active: ActiveTasks,
) {
    let max = config.daemon.max_concurrent_downloads.max(1) as usize;
    let sem = Arc::new(Semaphore::new(max));

    loop {
        // Wait for a slot before looking for work.
        let permit = match Arc::clone(&sem).acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // semaphore closed
        };

        let job = {
            let cat = catalog.lock().await;
            cat.next_queued().unwrap_or(None)
        };

        let Some(job) = job else {
            // No work yet; release permit and wait.
            drop(permit);
            sleep(Duration::from_secs(5)).await;
            continue;
        };

        info!("Starting download job {}", job.id);
        {
            let cat = catalog.lock().await;
            let _ = cat.set_status(job.id, JobStatus::Downloading, None);
        }

        let token = CancellationToken::new();
        {
            let mut tasks = active.lock().await;
            tasks.insert(job.id, token.clone());
        }

        let cat = catalog.clone();
        let cfg = config.clone();
        let civ = civitai.clone();
        let active_ref = active.clone();
        let job_id = job.id;

        tokio::spawn(async move {
            let _permit = permit; // released when task finishes

            match downloader::download(&job, &cfg, &civ, token).await {
                Ok(dest) => {
                    info!("Job {job_id} complete: {}", dest.display());
                    let cat = cat.lock().await;
                    let _ = cat.set_dest_path(job_id, &dest);
                    let _ = cat.set_status(job_id, JobStatus::Done, None);
                    let _ = notifier::notify_success(&dest.display().to_string());
                }
                Err(e) if e.to_string().contains("cancelled") => {
                    info!("Job {job_id} cancelled");
                    let cat = cat.lock().await;
                    let _ = cat.set_status(job_id, JobStatus::Cancelled, None);
                }
                Err(e) => {
                    error!("Job {job_id} failed: {e}");
                    let cat = cat.lock().await;
                    let _ = cat.set_status(job_id, JobStatus::Failed, Some(&e.to_string()));
                    let _ = notifier::notify_error(&e.to_string());
                }
            }

            let mut tasks = active_ref.lock().await;
            tasks.remove(&job_id);
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    #[tokio::test]
    async fn test_semaphore_limits_concurrency() {
        let sem = Arc::new(Semaphore::new(2));
        let mut permits = vec![];

        permits.push(sem.clone().acquire_owned().await.unwrap());
        permits.push(sem.clone().acquire_owned().await.unwrap());

        assert!(sem.try_acquire().is_err());

        drop(permits.pop());
        assert!(sem.try_acquire().is_ok());
    }
}
