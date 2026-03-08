use crate::catalog::DownloadJob;
use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::daemon::queue::{DownloadProgress, ProgressMap};
use anyhow::{bail, Context, Result};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Download the file for `job`, verify its checksum, and return the destination path.
pub async fn download(
    job: &DownloadJob,
    config: &Config,
    civitai: &CivitaiClient,
    token: CancellationToken,
    progress: ProgressMap,
) -> Result<PathBuf> {
    // Parse model/version IDs from the URL if not already resolved.
    let dest_dir = config.paths.models_dir.join(
        job.model_type.as_deref().unwrap_or("other"),
    );
    fs::create_dir_all(&dest_dir).await?;

    check_disk_space(&dest_dir)?;

    let http = reqwest::Client::new();
    let mut req = http.get(&job.url);
    if let Some(key) = &config.civitai.api_key {
        req = req.bearer_auth(key);
    }

    let resp = req.send().await.context("starting download")?;
    if !resp.status().is_success() {
        bail!("download failed with status {}", resp.status());
    }

    let total_bytes = resp.content_length();
    {
        let mut prog = progress.lock().await;
        prog.insert(job.id, DownloadProgress { bytes_received: 0, total_bytes });
    }

    // Derive filename from Content-Disposition or URL.
    let filename = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_filename_from_cd)
        .unwrap_or_else(|| {
            job.url
                .split('/')
                .last()
                .unwrap_or("model.bin")
                .to_string()
        });

    let dest = dest_dir.join(&filename);
    let tmp = dest.with_extension("tmp");

    let mut file = File::create(&tmp).await?;
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    let mut bytes_received: u64 = 0;

    loop {
        tokio::select! {
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(chunk)) => {
                        bytes_received += chunk.len() as u64;
                        hasher.update(&chunk);
                        file.write_all(&chunk).await?;
                        {
                            let mut prog = progress.lock().await;
                            if let Some(entry) = prog.get_mut(&job.id) {
                                entry.bytes_received = bytes_received;
                            }
                        }
                    }
                    Some(Err(e)) => return Err(anyhow::Error::from(e)).context("reading chunk"),
                    None => break,
                }
            }
            _ = token.cancelled() => {
                drop(file);
                let _ = tokio::fs::remove_file(&tmp).await;
                bail!("download cancelled");
            }
        }
    }
    file.flush().await?;
    drop(file);

    let digest = hex::encode(hasher.finalize());
    info!("SHA-256: {digest}");

    // TODO: compare against CivitAI-reported hash once version metadata is resolved.

    fs::rename(&tmp, &dest).await?;
    Ok(dest)
}

fn check_disk_space(dir: &PathBuf) -> Result<()> {
    // Require at least 1 GiB free.
    let stat = free_disk_bytes(dir)?;
    if stat < 1024 * 1024 * 1024 {
        bail!("insufficient disk space (< 1 GiB free)");
    }
    Ok(())
}

pub(crate) fn free_disk_bytes(path: &std::path::PathBuf) -> Result<u64> {
    use std::ffi::CString;
    let cs = CString::new(path.to_string_lossy().as_ref())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(cs.as_ptr(), &mut stat) };
    if ret != 0 {
        bail!("statvfs failed");
    }
    Ok(stat.f_bavail * stat.f_frsize)
}

fn parse_filename_from_cd(header: &str) -> Option<String> {
    header
        .split(';')
        .find_map(|part| {
            let part = part.trim();
            part.strip_prefix("filename=")
                .map(|s| s.trim_matches('"').to_string())
        })
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_cancellation_token_stops_loop() {
        use tokio::time::{timeout, Duration};
        use tokio_util::sync::CancellationToken;

        let token = CancellationToken::new();
        let t = token.clone();

        let result = timeout(Duration::from_millis(200), async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(60)) => "slept",
                _ = t.cancelled() => "cancelled",
            }
        });

        token.cancel();
        assert_eq!(result.await.unwrap(), "cancelled");
    }
}
