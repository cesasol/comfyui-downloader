use crate::catalog::DownloadJob;
use crate::civitai::CivitaiClient;
use crate::civitai::types::ModelVersion;
use crate::config::Config;
use crate::daemon::notifier;
use crate::daemon::queue::{DownloadProgress, ProgressMap};
use anyhow::{Context, Result, bail};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

struct VersionResolution {
    download_url: String,
    expected_hash: Option<String>,
    /// ComfyUI models subdirectory derived from the CivitAI model type (e.g. "checkpoints").
    model_type_subdir: Option<String>,
    /// Base model name used as a subdirectory level (e.g. "SDXL 1.0", "Pony").
    base_model: Option<String>,
    /// Filename from the API (file.name). Used to check for an existing file before downloading.
    filename: Option<String>,
    model_name: Option<String>,
    preview_image_url: Option<String>,
    preview_nsfw_level: Option<u32>,
    /// Full version API response, stored for metadata serialization.
    model_version: Option<ModelVersion>,
}

#[derive(serde::Serialize)]
struct ModelMetadata {
    file_name: String,
    model_name: Option<String>,
    file_path: String,
    size: u64,
    modified: f64,
    sha256: String,
    base_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview_nsfw_level: Option<u32>,
    notes: String,
    from_civitai: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    civitai: Option<serde_json::Value>,
}

/// Resolve the authoritative download URL, expected SHA-256, model type, and base model
/// from the CivitAI API. Falls back to the stored job URL only when no IDs are available.
async fn resolve_version(
    job: &DownloadJob,
    civitai: &CivitaiClient,
    config: &Config,
) -> Result<VersionResolution> {
    match (job.version_id, job.model_id) {
        (Some(version_id), Some(model_id)) => {
            // Both IDs known: use get_model for reliable type and base_model,
            // get_model_version for the authoritative file list and download URL.
            let (model_info, version) = tokio::try_join!(
                civitai.get_model(model_id),
                civitai.get_model_version(version_id),
            )?;
            let base_model = model_info
                .model_versions
                .iter()
                .find(|v| v.id == version_id)
                .and_then(|v| v.base_model.clone())
                .or_else(|| version.base_model.clone());
            let file = version
                .files
                .iter()
                .find(|f| f.primary == Some(true))
                .or_else(|| version.files.first())
                .context("no files in version metadata")?;
            let model_type_subdir = Some(
                model_info
                    .r#type
                    .models_subdir_for_file(file, base_model.as_deref())
                    .to_string(),
            );
            let download_url = file.download_url.clone().with_context(|| {
                format!(
                    "no downloadUrl for file '{}' in version {version_id}",
                    file.name
                )
            })?;
            let expected_hash = file.hashes.sha256.clone();
            let filename = file.name.clone();
            let preview_image_url = version.images.first().map(|img| img.url.clone());
            let preview_nsfw_level = version.images.first().and_then(|img| img.nsfw_level);
            let model_name = Some(model_info.name);
            info!(
                "Resolved: type={:?} base_model={:?} file={}",
                model_type_subdir, base_model, filename
            );
            Ok(VersionResolution {
                download_url,
                expected_hash,
                model_type_subdir,
                base_model,
                filename: Some(filename),
                model_name,
                preview_image_url,
                preview_nsfw_level,
                model_version: Some(version),
            })
        }

        (Some(version_id), None) => {
            // Version ID only: single call, use whatever the endpoint returns.
            let version = civitai
                .get_model_version(version_id)
                .await
                .context("fetching version metadata")?;
            let base_model = version.base_model.clone();
            let file = version
                .files
                .iter()
                .find(|f| f.primary == Some(true))
                .or_else(|| version.files.first())
                .context("no files in version metadata")?;
            let model_type_subdir = version.model.as_ref().map(|m| {
                m.r#type
                    .models_subdir_for_file(file, base_model.as_deref())
                    .to_string()
            });
            let download_url = file.download_url.clone().with_context(|| {
                format!(
                    "no downloadUrl for file '{}' in version {version_id}",
                    file.name
                )
            })?;
            let expected_hash = file.hashes.sha256.clone();
            let filename = file.name.clone();
            let preview_image_url = version.images.first().map(|img| img.url.clone());
            let preview_nsfw_level = version.images.first().and_then(|img| img.nsfw_level);
            let model_name = version.model.as_ref().map(|m| m.name.clone());
            info!(
                "Resolved: type={:?} base_model={:?} file={}",
                model_type_subdir, base_model, filename
            );
            Ok(VersionResolution {
                download_url,
                expected_hash,
                model_type_subdir,
                base_model,
                filename: Some(filename),
                model_name,
                preview_image_url,
                preview_nsfw_level,
                model_version: Some(version),
            })
        }

        (None, Some(model_id)) => {
            // Model ID only: pick the latest non-early-access version.
            let model_info = civitai
                .get_model(model_id)
                .await
                .context("fetching model metadata")?;
            let latest = model_info
                .model_versions
                .iter()
                .find(|v| {
                    !config.daemon.skip_early_access
                        || v.availability.as_deref() != Some("EarlyAccess")
                })
                .context("no publicly available version (all versions are EarlyAccess)")?;
            let base_model = latest.base_model.clone();
            let version_id = latest.id;
            let version = civitai
                .get_model_version(version_id)
                .await
                .context("fetching latest version metadata")?;
            let file = version
                .files
                .iter()
                .find(|f| f.primary == Some(true))
                .or_else(|| version.files.first())
                .context("no files in latest version")?;
            let model_type_subdir = Some(
                model_info
                    .r#type
                    .models_subdir_for_file(file, base_model.as_deref())
                    .to_string(),
            );
            let download_url = file.download_url.clone().with_context(|| {
                format!(
                    "no downloadUrl for file '{}' in version {version_id}",
                    file.name
                )
            })?;
            let expected_hash = file.hashes.sha256.clone();
            let filename = file.name.clone();
            let preview_image_url = version.images.first().map(|img| img.url.clone());
            let preview_nsfw_level = version.images.first().and_then(|img| img.nsfw_level);
            let model_name = Some(model_info.name);
            info!(
                "Resolved: type={:?} base_model={:?} file={}",
                model_type_subdir, base_model, filename
            );
            Ok(VersionResolution {
                download_url,
                expected_hash,
                model_type_subdir,
                base_model,
                filename: Some(filename),
                model_name,
                preview_image_url,
                preview_nsfw_level,
                model_version: Some(version),
            })
        }

        (None, None) => {
            warn!(
                "Job {} has no model/version ID; using stored URL without checksum verification",
                job.id
            );
            Ok(VersionResolution {
                download_url: job.url.clone(),
                expected_hash: None,
                model_type_subdir: None,
                base_model: None,
                filename: None,
                model_name: None,
                preview_image_url: None,
                preview_nsfw_level: None,
                model_version: None,
            })
        }
    }
}

/// Download the file for `job`, verify its checksum, and return `(dest_path, resolved_model_type)`.
/// `resolved_model_type` is the CivitAI-reported subdir (e.g. "checkpoints") if available.
pub async fn download(
    job: &DownloadJob,
    config: &Config,
    civitai: &CivitaiClient,
    token: CancellationToken,
    progress: ProgressMap,
) -> Result<(PathBuf, Option<String>)> {
    let key = config.civitai.api_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("CivitAI API key is not configured (set civitai.api_key in config.toml)")
    })?;

    let resolution = resolve_version(job, civitai, config).await?;

    // Prefer the API-reported model type over whatever the user provided at enqueue time.
    let model_type_str = resolution
        .model_type_subdir
        .as_deref()
        .or(job.model_type.as_deref())
        .unwrap_or("other");
    let mut dest_dir = config.paths.models_dir.join(model_type_str);
    if let Some(ref base_model) = resolution.base_model {
        dest_dir = dest_dir.join(sanitize_dir_name(base_model));
    }
    // Check if the target file already exists before downloading.
    if let Some(ref name) = resolution.filename {
        let existing = dest_dir.join(name);
        if existing.exists() {
            info!(
                "File already exists, skipping download: {}",
                existing.display()
            );
            return Ok((existing, resolution.model_type_subdir));
        }
    }

    fs::create_dir_all(&dest_dir).await?;

    check_disk_space(&dest_dir)?;

    let http = reqwest::Client::new();
    let resp = http
        .get(&resolution.download_url)
        .bearer_auth(key)
        .send()
        .await
        .context("starting download")?;
    match resp.status() {
        s if s.is_success() => {}
        s @ (reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN) => {
            return Err(crate::civitai::CivitaiAccessError { status: s.as_u16() }.into());
        }
        s => bail!("download failed with status {s}"),
    }

    let total_bytes = resp.content_length();
    let filename = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_filename_from_cd)
        .unwrap_or_else(|| {
            resolution
                .download_url
                .split('/')
                .next_back()
                .unwrap_or("model.bin")
                .to_string()
        });

    info!(
        "Downloading '{}' → {}/{}",
        filename, model_type_str, filename
    );
    {
        let mut prog = progress.lock().await;
        prog.insert(
            job.id,
            DownloadProgress {
                bytes_received: 0,
                total_bytes,
            },
        );
    }

    let notif_id = notifier::notify_download_start(&filename);
    let mut last_notif_pct: u64 = 0;

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
                        if let (Some(nid), Some(total)) = (notif_id, total_bytes)
                            && total > 0
                        {
                            let pct = bytes_received * 100 / total;
                            if pct >= last_notif_pct + 10 {
                                last_notif_pct = pct;
                                notifier::update_download_progress(nid, &filename, bytes_received, total_bytes);
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
                if let Some(nid) = notif_id {
                    notifier::close_download_notification(nid);
                }
                bail!("download cancelled");
            }
        }
    }
    file.flush().await?;
    drop(file);

    if let Some(nid) = notif_id {
        notifier::close_download_notification(nid);
    }

    let digest = hex::encode(hasher.finalize());
    info!("SHA-256: {digest}");

    if let Some(expected) = resolution.expected_hash.as_deref() {
        if !expected.eq_ignore_ascii_case(&digest) {
            fs::remove_file(&tmp).await?;
            bail!("checksum mismatch: computed {digest}, expected {expected}");
        }
        info!("Checksum verified");
    } else {
        warn!("No SHA-256 hash available for this file, skipping verification");
    }

    fs::rename(&tmp, &dest).await?;

    let preview_path = resolution.preview_image_url.as_deref().map(|url| {
        let ext = url
            .split('?')
            .next()
            .unwrap_or(url)
            .rsplit('.')
            .next()
            .unwrap_or("jpg");
        dest.with_extension(format!("preview.{ext}"))
    });
    write_metadata(&dest, &resolution, &digest, preview_path.as_ref()).await;
    if let (Some(url), Some(path)) = (
        resolution.preview_image_url.as_deref(),
        preview_path.as_ref(),
    ) {
        download_preview(url, path).await;
    }
    Ok((dest, resolution.model_type_subdir))
}

async fn write_metadata(
    dest: &PathBuf,
    resolution: &VersionResolution,
    sha256: &str,
    preview_path: Option<&PathBuf>,
) {
    let meta_path = dest.with_extension("metadata.json");

    let fs_meta = tokio::fs::metadata(dest).await.ok();
    let size = fs_meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let modified = fs_meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let file_name = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file_path = dest.to_string_lossy().into_owned();
    let preview_url = preview_path.map(|p| p.to_string_lossy().into_owned());
    let civitai = resolution
        .model_version
        .as_ref()
        .and_then(|v| serde_json::to_value(v).ok());

    let meta = ModelMetadata {
        file_name,
        model_name: resolution.model_name.clone(),
        file_path,
        size,
        modified,
        sha256: sha256.to_string(),
        base_model: resolution.base_model.clone(),
        preview_url,
        preview_nsfw_level: resolution.preview_nsfw_level,
        notes: String::new(),
        from_civitai: resolution.model_version.is_some(),
        civitai,
    };
    match serde_json::to_string_pretty(&meta) {
        Ok(json) => {
            if let Err(e) = tokio::fs::write(&meta_path, json).await {
                warn!("Failed to write metadata file {}: {e}", meta_path.display());
            }
        }
        Err(e) => warn!("Failed to serialise metadata: {e}"),
    }
}

async fn download_preview(url: &str, path: &PathBuf) {
    if path.exists() {
        return;
    }
    let http = reqwest::Client::new();
    match http.get(url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.bytes().await {
            Ok(bytes) => {
                if let Err(e) = tokio::fs::write(path, &bytes).await {
                    warn!("Failed to write preview image {}: {e}", path.display());
                } else {
                    info!("Preview saved: {}", path.display());
                }
            }
            Err(e) => warn!("Failed to read preview image bytes: {e}"),
        },
        Ok(resp) => warn!("Preview image request failed with status {}", resp.status()),
        Err(e) => warn!("Failed to fetch preview image: {e}"),
    }
}

/// Write metadata and/or download a preview image for a model file that was not
/// downloaded by this daemon (e.g. discovered by the startup scanner).
/// Pass `write_meta = false` to skip metadata if it already exists.
/// Pass `write_preview = false` to skip preview if it already exists.
pub(crate) async fn save_artifacts(
    dest: &PathBuf,
    version: ModelVersion,
    sha256: &str,
    write_meta: bool,
    write_preview: bool,
) {
    let model_name = version.model.as_ref().map(|m| m.name.clone());
    let base_model = version.base_model.clone();
    let preview_image_url = version.images.first().map(|img| img.url.clone());
    let preview_nsfw_level = version.images.first().and_then(|img| img.nsfw_level);
    let preview_path = preview_image_url.as_deref().map(|url| {
        let ext = url
            .split('?')
            .next()
            .unwrap_or(url)
            .rsplit('.')
            .next()
            .unwrap_or("jpg");
        dest.with_extension(format!("preview.{ext}"))
    });
    let resolution = VersionResolution {
        download_url: String::new(),
        expected_hash: None,
        model_type_subdir: None,
        base_model,
        filename: None,
        model_name,
        preview_image_url: preview_image_url.clone(),
        preview_nsfw_level,
        model_version: Some(version),
    };
    if write_meta {
        write_metadata(dest, &resolution, sha256, preview_path.as_ref()).await;
    }
    if write_preview
        && let (Some(url), Some(path)) = (preview_image_url.as_deref(), preview_path.as_ref())
    {
        download_preview(url, path).await;
    }
}

fn check_disk_space(dir: &Path) -> Result<()> {
    let stat = free_disk_bytes(dir)?;
    if stat < 1024 * 1024 * 1024 {
        bail!("insufficient disk space (< 1 GiB free)");
    }
    Ok(())
}

pub(crate) fn free_disk_bytes(path: &std::path::Path) -> Result<u64> {
    use std::ffi::CString;
    let cs = CString::new(path.to_string_lossy().as_ref())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(cs.as_ptr(), &mut stat) };
    if ret != 0 {
        bail!("statvfs failed");
    }
    Ok(stat.f_bavail * stat.f_frsize)
}

/// Sanitize a string for use as a directory name component.
/// Strips characters that are unsafe on common filesystems (slashes, null bytes, etc.).
pub(crate) fn sanitize_dir_name(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !matches!(
                c,
                '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
        })
        .collect()
}

fn parse_filename_from_cd(header: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("filename=")
            .map(|s| s.trim_matches('"').to_string())
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_parse_filename_from_cd_quoted() {
        let result =
            super::parse_filename_from_cd(r#"attachment; filename="my_model.safetensors""#);
        assert_eq!(result, Some("my_model.safetensors".to_string()));
    }

    #[test]
    fn test_parse_filename_from_cd_unquoted() {
        let result = super::parse_filename_from_cd("attachment; filename=model.bin");
        assert_eq!(result, Some("model.bin".to_string()));
    }

    #[tokio::test]
    async fn test_cancellation_token_stops_loop() {
        use tokio::time::{Duration, timeout};
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

    fn load_model_info() -> crate::civitai::types::ModelInfo {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/stubs/model_response.stub.json");
        let json = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn test_early_access_filtering() {
        let info = load_model_info();

        let first_public = info
            .model_versions
            .iter()
            .find(|v| v.availability.as_deref() != Some("EarlyAccess"));

        let v = first_public.expect("should find a public version");
        assert_eq!(v.id, 5550002);
        assert_eq!(v.name, "Flux Dev V2");
    }

    #[test]
    fn test_early_access_not_filtered_when_disabled() {
        let info = load_model_info();

        let latest = info.model_versions.first().expect("should have versions");
        assert_eq!(latest.id, 5550003);
        assert_eq!(latest.availability.as_deref(), Some("EarlyAccess"));
    }
}
