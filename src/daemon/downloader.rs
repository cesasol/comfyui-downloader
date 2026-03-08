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

    let key = config.civitai.api_key.as_deref()
        .ok_or_else(|| anyhow::anyhow!("CivitAI API key is not configured (set civitai.api_key in config.toml)"))?;

    let http = reqwest::Client::new();

    // When we know the version_id, resolve the real download URL and expected hash from
    // the CivitAI API rather than using whatever URL the user provided (which may be a
    // model page URL that returns HTML instead of the model file).
    let (download_url, expected_hash) = if let Some(version_id) = job.version_id {
        let version = civitai
            .get_model_version(version_id)
            .await
            .context("fetching version metadata")?;
        let file = version
            .files
            .iter()
            .find(|f| f.primary == Some(true))
            .or_else(|| version.files.first())
            .context("no files in version metadata")?;
        let url = file
            .download_url
            .clone()
            .with_context(|| format!("no downloadUrl for file {} in version {version_id}", file.name))?;
        info!("Resolved download URL for version {version_id}: {url}");
        (url, file.hashes.sha256.clone())
    } else {
        tracing::warn!("No version_id for job {}; using stored URL without checksum verification", job.id);
        (job.url.clone(), None)
    };

    let resp = http
        .get(&download_url)
        .bearer_auth(key)
        .send()
        .await
        .context("starting download")?;
    if !resp.status().is_success() {
        bail!("download failed with status {}", resp.status());
    }

    let total_bytes = resp.content_length();
    {
        let mut prog = progress.lock().await;
        prog.insert(job.id, DownloadProgress { bytes_received: 0, total_bytes });
    }

    // Derive filename from Content-Disposition or the download URL.
    let filename = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_filename_from_cd)
        .unwrap_or_else(|| {
            download_url
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

    if let Some(expected) = expected_hash.as_deref() {
        if !checksums_match(&digest, Some(expected)) {
            fs::remove_file(&tmp).await?;
            bail!("checksum mismatch: computed {digest}, expected {expected}");
        }
        info!("Checksum verified");
    } else {
        tracing::warn!("No SHA-256 hash available, skipping verification");
    }

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

fn checksums_match(computed: &str, expected: Option<&str>) -> bool {
    match expected {
        None => true,
        Some(h) => h.eq_ignore_ascii_case(computed),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_parse_filename_from_cd_quoted() {
        let result = super::parse_filename_from_cd(
            r#"attachment; filename="my_model.safetensors""#,
        );
        assert_eq!(result, Some("my_model.safetensors".to_string()));
    }

    #[test]
    fn test_parse_filename_from_cd_unquoted() {
        let result = super::parse_filename_from_cd("attachment; filename=model.bin");
        assert_eq!(result, Some("model.bin".to_string()));
    }

    #[test]
    fn test_checksums_match() {
        assert!(super::checksums_match("abc123", Some("abc123")));
        assert!(super::checksums_match("ABC123", Some("abc123"))); // case-insensitive
        assert!(!super::checksums_match("abc123", Some("deadbeef")));
        assert!(super::checksums_match("abc123", None)); // no expected hash → pass
    }

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
