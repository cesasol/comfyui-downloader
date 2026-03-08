use crate::catalog::DownloadJob;
use crate::civitai::CivitaiClient;
use crate::config::Config;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tracing::info;

/// Download the file for `job`, verify its checksum, and return the destination path.
pub async fn download(
    job: &DownloadJob,
    config: &Config,
    civitai: &CivitaiClient,
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

    while let Some(chunk) = stream.next().await {
        let chunk: Bytes = chunk.context("reading chunk")?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);

    let digest = hex::encode(hasher.finalize());
    info!("SHA-256: {digest}");

    if let Some(version_id) = job.version_id {
        let version = civitai
            .get_model_version(version_id)
            .await
            .context("fetching version metadata for checksum")?;
        let expected = version
            .files
            .iter()
            .find(|f| f.primary == Some(true))
            .or_else(|| version.files.first())
            .and_then(|f| f.hashes.sha256.as_deref());

        if expected.is_none() {
            tracing::warn!("No SHA-256 hash available for version {version_id}, skipping verification");
        } else if !checksums_match(&digest, expected) {
            fs::remove_file(&tmp).await?;
            bail!(
                "checksum mismatch: computed {digest}, expected {}",
                expected.unwrap_or("unknown")
            );
        } else {
            info!("Checksum verified");
        }
    }

    fs::rename(&tmp, &dest).await?;
    Ok(dest)
}

fn check_disk_space(dir: &PathBuf) -> Result<()> {
    // Require at least 1 GiB free.
    let stat = nix_statvfs(dir)?;
    if stat < 1024 * 1024 * 1024 {
        bail!("insufficient disk space (< 1 GiB free)");
    }
    Ok(())
}

fn nix_statvfs(path: &PathBuf) -> Result<u64> {
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
}
