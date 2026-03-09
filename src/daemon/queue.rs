use crate::catalog::{Catalog, DownloadReason, JobStatus};
use crate::civitai::{CivitaiAccessError, CivitaiClient};
use crate::config::Config;
use crate::daemon::downloader;
use crate::daemon::notifier;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

/// Map of active download job IDs to their cancellation tokens.
pub type ActiveTasks = Arc<Mutex<HashMap<Uuid, CancellationToken>>>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct DownloadProgress {
    pub bytes_received: u64,
    pub total_bytes: Option<u64>,
}

pub type ProgressMap = Arc<Mutex<HashMap<Uuid, DownloadProgress>>>;

pub async fn run(
    config: Arc<Config>,
    catalog: Arc<Mutex<Catalog>>,
    civitai: Arc<CivitaiClient>,
    active: ActiveTasks,
    progress: ProgressMap,
) {
    let max = config.daemon.max_concurrent_downloads.max(1);
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

        // Skip update if the user already deleted every original file for this model.
        if job.download_reason == DownloadReason::UpdateAvailable
            && let Some(model_id) = job.model_id
        {
            let done_jobs = {
                let cat = catalog.lock().await;
                cat.done_jobs_for_model(model_id).unwrap_or_default()
            };
            let tracked: Vec<_> = done_jobs.iter().filter(|j| j.dest_path.is_some()).collect();
            let all_missing = !tracked.is_empty()
                && tracked
                    .iter()
                    .all(|j| !Path::new(j.dest_path.as_ref().unwrap()).exists());
            if all_missing {
                info!(
                    "Skipping update job {} for model {model_id}: all original files deleted",
                    job.id
                );
                {
                    let cat = catalog.lock().await;
                    let _ = cat.delete_job(job.id);
                    for prev in &done_jobs {
                        let _ = cat.delete_job(prev.id);
                    }
                }
                drop(permit);
                let _ = notifier::notify_update_skipped_deleted(
                    job.version_id.unwrap_or(0),
                    job.model_type.as_deref(),
                );
                continue;
            }
        }

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
        let prog = progress.clone();

        tokio::spawn(async move {
            let _permit = permit; // released when task finishes

            match downloader::download(&job, &cfg, &civ, token, prog.clone()).await {
                Ok((dest, resolved_type)) => {
                    info!("Job {job_id} complete: {}", dest.display());
                    let cat = cat.lock().await;
                    let _ = cat.set_dest_path(job_id, &dest);
                    if let Some(model_type) = resolved_type {
                        let _ = cat.set_model_type(job_id, &model_type);
                    }
                    let _ = cat.set_status(job_id, JobStatus::Done, None);
                    let _ = notifier::notify_success(&dest.display().to_string());
                }
                Err(e) if e.to_string().contains("cancelled") => {
                    info!("Job {job_id} cancelled");
                    let cat = cat.lock().await;
                    let _ = cat.set_status(job_id, JobStatus::Cancelled, None);
                }
                Err(ref e) if e.downcast_ref::<CivitaiAccessError>().is_some() => {
                    let status = e.downcast_ref::<CivitaiAccessError>().unwrap().status;
                    warn!("Job {job_id}: HTTP {status} access denied");
                    {
                        let cat = cat.lock().await;
                        let msg = format!("access denied (HTTP {status})");
                        let _ = cat.set_status(job_id, JobStatus::Failed, Some(&msg));
                    }
                    try_enqueue_fallback_version(&job, status, &cat, &civ).await;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    error!("Job {job_id} failed: {msg}");
                    let cat = cat.lock().await;
                    let _ = cat.set_status(job_id, JobStatus::Failed, Some(&msg));
                    let _ = notifier::notify_error(&msg);
                }
            }

            active_ref.lock().await.remove(&job_id);
            prog.lock().await.remove(&job_id);
        });
    }
}

async fn try_enqueue_fallback_version(
    job: &crate::catalog::DownloadJob,
    status: u16,
    catalog: &Arc<Mutex<Catalog>>,
    civitai: &Arc<CivitaiClient>,
) {
    let Some(model_id) = job.model_id else {
        let msg = format!("access denied (HTTP {status}), no model ID for fallback");
        let _ = notifier::notify_error(&msg);
        return;
    };

    let model_info = match civitai.get_model(model_id).await {
        Ok(m) => m,
        Err(_) => {
            let msg = format!("access denied (HTTP {status}), could not fetch model {model_id}");
            let _ = notifier::notify_error(&msg);
            return;
        }
    };

    let denied_version = job.version_id.unwrap_or(0);
    let start = model_info
        .model_versions
        .iter()
        .position(|v| v.id == denied_version)
        .map(|p| p + 1)
        .unwrap_or(0);

    let mut enqueued: Option<u64> = None;
    for candidate in model_info.model_versions.iter().skip(start) {
        let already_in_catalog = {
            let cat = catalog.lock().await;
            cat.get_job_by_version_id(candidate.id)
                .ok()
                .flatten()
                .is_some()
        };
        if already_in_catalog {
            continue;
        }
        let url = format!(
            "https://civitai.com/models/{model_id}?modelVersionId={}",
            candidate.id
        );
        let cat = catalog.lock().await;
        if cat
            .enqueue(
                &url,
                job.model_type.as_deref(),
                DownloadReason::AccessDeniedFallback,
            )
            .is_ok()
        {
            enqueued = Some(candidate.id);
        }
        break;
    }

    match enqueued {
        Some(fallback_id) => {
            let _ = notifier::notify_version_access_denied(
                &model_info.name,
                denied_version,
                fallback_id,
                status,
            );
        }
        None => {
            let _ = notifier::notify_access_denied_no_fallback(
                &model_info.name,
                denied_version,
                status,
            );
        }
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
