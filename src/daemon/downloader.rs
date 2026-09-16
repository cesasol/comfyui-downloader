use crate::catalog::{Catalog, DownloadJob};
use crate::civitai::CivitaiClient;
use crate::civitai::types::{ModelFile, ModelImage, ModelInfo, ModelVersion};
use crate::config::Config;
use crate::daemon::notifier;
use crate::daemon::queue::{DownloadProgress, ProgressMap};
use anyhow::{Context, Result, bail};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Result of a completed (or deduplicated) download.
pub struct DownloadOutcome {
    /// Final on-disk location of the model file.
    pub dest: PathBuf,
    /// Resolved ComfyUI model-type subdir, when the source reported one.
    pub model_type: Option<String>,
    /// SHA-256 of the file content, when known. From the source metadata on a
    /// deduplicated hit, or the computed digest after a real download.
    pub sha256: Option<String>,
    /// True when the bytes were reused from an existing identical file instead
    /// of being downloaded again.
    pub deduplicated: bool,
}

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
    /// Model info from CivitAI API, contains tags and description.
    model_info: Option<ModelInfo>,
}

#[derive(serde::Serialize)]
struct ModelMetadata {
    file_name: String,
    model_name: String,
    version_name: Option<String>,
    file_path: String,
    size: u64,
    modified: f64,
    sha256: String,
    base_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview_url: Option<String>,
    #[serde(default = "default_preview_nsfw_level")]
    preview_nsfw_level: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "modelDescription")]
    model_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    from_civitai: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    civitai: Option<serde_json::Value>,
    #[serde(default = "default_tags")]
    tags: Vec<String>,
    #[serde(default)]
    civitai_deleted: bool,
    #[serde(default)]
    favorite: bool,
    #[serde(default)]
    exclude: bool,
    #[serde(default)]
    db_checked: bool,
    #[serde(default)]
    skip_metadata_refresh: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata_source: Option<String>,
    #[serde(default)]
    last_checked_at: f64,
    #[serde(default = "default_hash_status")]
    hash_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    autov3: Option<String>,
}

#[allow(dead_code)]
fn default_preview_nsfw_level() -> u32 {
    0
}

#[allow(dead_code)]
fn default_tags() -> Vec<String> {
    Vec::new()
}

#[allow(dead_code)]
fn default_hash_status() -> String {
    "completed".to_string()
}

/// Resolve the authoritative download URL, expected SHA-256, model type, and base model
/// from the CivitAI API. Falls back to the stored job URL only when no IDs are available.
fn select_file<'a>(version: &'a ModelVersion, preferred: Option<&str>) -> Option<&'a ModelFile> {
    if let Some(name) = preferred {
        let found = version.files.iter().find(|f| f.name == name);
        if found.is_none() {
            warn!(
                "Preferred file '{name}' not found in version {}, falling back",
                version.id
            );
        }
        found
    } else {
        None
    }
    .or_else(|| version.files.iter().find(|f| f.primary == Some(true)))
    .or_else(|| version.files.first())
}

async fn resolve_version(
    job: &DownloadJob,
    civitai: &CivitaiClient,
    config: &Config,
) -> Result<VersionResolution> {
    if let Some(reference) = crate::huggingface::parse_hf_url(&job.url) {
        return resolve_huggingface(job, &reference, config).await;
    }
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
            let file = select_file(&version, job.preferred_file_name.as_deref())
                .context("no files in version metadata")?;
            let model_type_subdir = Some(model_info.r#type.models_subdir().to_string());
            let download_url = file.download_url.clone().with_context(|| {
                format!(
                    "no downloadUrl for file '{}' in version {version_id}",
                    file.name
                )
            })?;
            let expected_hash = file.hashes.sha256.clone();
            let filename = file.name.clone();
            let preview_image = select_preview_image(&version.images);
            let preview_image_url = preview_image.map(|img| img.url.clone());
            let preview_nsfw_level = preview_image.and_then(|img| img.nsfw_level);
            let model_name = Some(model_info.name.clone());
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
                model_info: Some(model_info),
            })
        }

        (Some(version_id), None) => {
            let version = civitai
                .get_model_version(version_id)
                .await
                .context("fetching version metadata")?;
            let base_model = version.base_model.clone();
            let file = select_file(&version, job.preferred_file_name.as_deref())
                .context("no files in version metadata")?;
            let model_type_subdir = version
                .model
                .as_ref()
                .map(|m| m.r#type.models_subdir().to_string());
            let download_url = file.download_url.clone().with_context(|| {
                format!(
                    "no downloadUrl for file '{}' in version {version_id}",
                    file.name
                )
            })?;
            let expected_hash = file.hashes.sha256.clone();
            let filename = file.name.clone();
            let preview_image = select_preview_image(&version.images);
            let preview_image_url = preview_image.map(|img| img.url.clone());
            let preview_nsfw_level = preview_image.and_then(|img| img.nsfw_level);
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
                model_info: None,
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
            let file = select_file(&version, job.preferred_file_name.as_deref())
                .context("no files in latest version")?;
            let model_type_subdir = Some(model_info.r#type.models_subdir().to_string());
            let download_url = file.download_url.clone().with_context(|| {
                format!(
                    "no downloadUrl for file '{}' in version {version_id}",
                    file.name
                )
            })?;
            let expected_hash = file.hashes.sha256.clone();
            let filename = file.name.clone();
            let preview_image = select_preview_image(&version.images);
            let preview_image_url = preview_image.map(|img| img.url.clone());
            let preview_nsfw_level = preview_image.and_then(|img| img.nsfw_level);
            let model_name = Some(model_info.name.clone());
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
                model_info: Some(model_info),
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
                model_info: None,
            })
        }
    }
}

/// Resolve a HuggingFace file: the download URL is deterministic, and the tree
/// API supplies the LFS SHA-256 so the usual checksum verification still runs.
/// The target subdirectory comes from the job (set by the template picker) or
/// from the file's directory inside the repo (`split_files/vae/...` → `vae`).
async fn resolve_huggingface(
    job: &DownloadJob,
    reference: &crate::huggingface::HfFileRef,
    config: &Config,
) -> Result<VersionResolution> {
    let client = crate::huggingface::HfClient::new(config.huggingface_token().map(str::to_owned))?;
    let meta = match client.file_meta(reference).await {
        Ok(meta) => Some(meta),
        Err(e) => {
            // Metadata is an optimisation: without it we simply download
            // without checksum verification.
            warn!(
                "HuggingFace metadata lookup failed for {}: {e:#}",
                reference.path
            );
            None
        }
    };
    let model_type_subdir = job.model_type.clone().or_else(|| {
        reference
            .dir()
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .and_then(crate::templates::normalize_role)
    });
    info!(
        "Resolved HuggingFace file {}/{} → {:?}",
        reference.repo, reference.path, model_type_subdir
    );
    Ok(VersionResolution {
        download_url: reference.resolve_url(),
        expected_hash: meta.and_then(|m| m.sha256),
        model_type_subdir,
        base_model: None,
        filename: Some(reference.file_name().to_string()),
        model_name: Some(format!("{} ({})", reference.file_name(), reference.repo)),
        preview_image_url: None,
        preview_nsfw_level: None,
        model_version: None,
        model_info: None,
    })
}

/// Attach the bearer token for the download host, when one is configured.
fn with_auth(req: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(token) => req.bearer_auth(token),
        None => req,
    }
}

/// Download the file for `job`, verify its checksum, and return a
/// [`DownloadOutcome`] describing the final path, model type, and hash.
///
/// `catalog` is used for content-addressed deduplication: if another completed
/// job already holds a byte-identical file (matched by the source-declared
/// SHA-256, even from a different repo or platform), the transfer is skipped
/// and that file's path is reused.
pub async fn download(
    job: &DownloadJob,
    config: &Config,
    civitai: &CivitaiClient,
    catalog: &Arc<Mutex<Catalog>>,
    token: CancellationToken,
    progress: ProgressMap,
) -> Result<DownloadOutcome> {
    let from_huggingface = crate::huggingface::is_huggingface_url(&job.url);
    let auth_token: Option<&str> = if from_huggingface {
        // Public HuggingFace files need no credentials; gated repos need a token.
        config.huggingface_token()
    } else {
        Some(config.civitai_api_key().ok_or_else(|| {
            anyhow::anyhow!(
                "CivitAI API key is not configured (run `comfyui-dl set-key <KEY>` to store it in the keyring)"
            )
        })?)
    };

    let resolution = resolve_version(job, civitai, config).await?;

    // Content-addressed dedup: if the source declared a SHA-256 and another
    // completed job already holds a file with that exact hash on disk, reuse it
    // instead of downloading the same bytes again. This consolidates copies of
    // the same file across repos and across HuggingFace and CivitAI.
    if let Some(expected) = resolution.expected_hash.as_deref() {
        let existing = {
            let cat = catalog.lock().await;
            cat.find_done_job_by_sha256(expected, job.id).ok().flatten()
        };
        if let Some(existing) = existing
            && let Some(dest_path) = existing.dest_path.as_deref()
        {
            let dest = PathBuf::from(dest_path);
            info!(
                "Identical file already present (sha256 {expected}), reusing {}",
                dest.display()
            );
            return Ok(DownloadOutcome {
                dest,
                model_type: resolution.model_type_subdir.clone(),
                sha256: Some(expected.to_ascii_lowercase()),
                deduplicated: true,
            });
        }
    }

    let mut model_type_str = resolution
        .model_type_subdir
        .clone()
        .or_else(|| job.model_type.clone())
        .unwrap_or_else(|| "other".to_string());
    let mut dest_dir = config.paths.models_dir.join(&model_type_str);
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
            return Ok(DownloadOutcome {
                dest: existing,
                model_type: resolution.model_type_subdir.clone(),
                sha256: resolution
                    .expected_hash
                    .clone()
                    .map(|h| h.to_ascii_lowercase()),
                deduplicated: true,
            });
        }
    }

    fs::create_dir_all(&dest_dir)
        .await
        .with_context(|| format!("creating model directory {}", dest_dir.display()))?;

    check_disk_space(&dest_dir)?;

    let provisional_filename = resolution
        .download_url
        .split('/')
        .next_back()
        .unwrap_or("model.bin");
    let provisional_dest = dest_dir.join(provisional_filename);
    let provisional_tmp = provisional_dest.with_extension("tmp");

    let resume_from: u64 = if provisional_tmp.exists() {
        match tokio::fs::metadata(&provisional_tmp).await {
            Ok(metadata) => {
                let size = metadata.len();
                info!("Resuming download from byte {}", size);
                size
            }
            Err(_) => 0,
        }
    } else {
        0
    };

    let http = reqwest::Client::new();
    let mut req = with_auth(http.get(&resolution.download_url), auth_token);

    if resume_from > 0 {
        req = req.header("Range", format!("bytes={}-", resume_from));
    }

    let mut resp = req.send().await.context("starting download")?;

    let mut resume_from = resume_from;

    if resume_from > 0 && resp.status() == reqwest::StatusCode::OK {
        warn!("Server doesn't support range requests, restarting download");
        drop(resp);
        let _ = tokio::fs::remove_file(&provisional_tmp).await;
        resume_from = 0;

        resp = with_auth(http.get(&resolution.download_url), auth_token)
            .send()
            .await
            .context("restarting download")?;
    }

    match resp.status() {
        s if s.is_success() => {}
        reqwest::StatusCode::PARTIAL_CONTENT => {}
        s @ (reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN) => {
            if from_huggingface {
                bail!(
                    "HuggingFace returned HTTP {s}: the repo may be gated — accept its licence and run `comfyui-dl set-key --service huggingface <TOKEN>`"
                );
            }
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

    let mut dest = dest_dir.join(&filename);
    let tmp = dest.with_extension("tmp");
    let notification_preview_path = resolution
        .preview_image_url
        .as_deref()
        .map(|url| preview_path_for_url(&dest, url));

    if tmp != provisional_tmp
        && provisional_tmp.exists()
        && resume_from > 0
        && let Err(e) = tokio::fs::rename(&provisional_tmp, &tmp).await
    {
        warn!(
            "Failed to rename temp file: {}, continuing without resume",
            e
        );
        resume_from = 0;
    }

    info!(
        "Downloading '{}' → {}/{}",
        filename, model_type_str, filename
    );

    if let (Some(url), Some(path)) = (
        resolution.preview_image_url.as_deref(),
        notification_preview_path.as_ref(),
    ) {
        download_preview(url, path).await;
    }

    let calculated_total_bytes = if resume_from > 0 {
        Some(resume_from + resp.content_length().unwrap_or(0))
    } else {
        total_bytes
    };

    {
        let mut prog = progress.lock().await;
        prog.insert(
            job.id,
            DownloadProgress {
                bytes_received: resume_from,
                total_bytes: calculated_total_bytes,
                model_name: resolution.model_name.clone(),
                version_name: resolution.model_version.as_ref().map(|v| v.name.clone()),
                dest_path: Some(dest.to_string_lossy().into_owned()),
                model_type: resolution
                    .model_type_subdir
                    .clone()
                    .or_else(|| job.model_type.clone()),
                download_reason: Some(job.download_reason.to_string()),
                started_at: Some(chrono::Utc::now()),
            },
        );
    }

    let notif_id = notifier::notify_download_start(&filename, notification_preview_path.as_deref());
    let mut last_notif_pct: u64 = 0;

    let mut file = if resume_from > 0 {
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&tmp)
            .await
            .with_context(|| format!("opening {} to resume download", tmp.display()))?
    } else {
        File::create(&tmp)
            .await
            .with_context(|| format!("creating temporary file {}", tmp.display()))?
    };
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    let mut bytes_received: u64 = resume_from;

    loop {
        tokio::select! {
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(chunk)) => {
                        bytes_received += chunk.len() as u64;
                        hasher.update(&chunk);
                        file.write_all(&chunk)
                            .await
                            .with_context(|| format!("writing to {}", tmp.display()))?;
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
                                notifier::update_download_progress(
                                    nid,
                                    &filename,
                                    bytes_received,
                                    total_bytes,
                                    notification_preview_path.as_deref(),
                                );
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
    file.flush()
        .await
        .with_context(|| format!("flushing {}", tmp.display()))?;
    drop(file);

    if let Some(nid) = notif_id {
        notifier::close_download_notification(nid);
    }

    let digest = if resume_from > 0 {
        info!("Re-hashing entire file for resumed download");
        let mut file = tokio::fs::File::open(&tmp).await?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 8192];

        loop {
            let bytes_read = file.read(&mut buffer).await?;
            if bytes_read == 0 {
                break;
            }
            hasher.update(&buffer[..bytes_read]);
        }

        hex::encode(hasher.finalize())
    } else {
        hex::encode(hasher.finalize())
    };

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

    if model_type_str == "checkpoints" && !is_video_checkpoint(resolution.base_model.as_deref()) {
        let ext = dest.extension().and_then(|e| e.to_str());
        let reroute = match ext {
            Some("gguf") => true,
            Some("safetensors") => match crate::safetensor::inspect_components(&tmp).await {
                Ok(c) if !c.has_vae && !c.has_clip => true,
                Ok(_) => {
                    info!("VAE/CLIP found in safetensors header, keeping in checkpoints");
                    false
                }
                Err(e) => {
                    warn!("Failed to inspect safetensors header: {e:#}; defaulting to checkpoints");
                    false
                }
            },
            _ => false,
        };
        if reroute {
            info!("Routing checkpoint to diffusion_models");
            model_type_str = "diffusion_models".to_string();
            dest_dir = config.paths.models_dir.join(&model_type_str);
            if let Some(ref base_model) = resolution.base_model {
                dest_dir = dest_dir.join(sanitize_dir_name(base_model));
            }
            fs::create_dir_all(&dest_dir)
                .await
                .with_context(|| format!("creating model directory {}", dest_dir.display()))?;
            dest = dest_dir.join(&filename);
        }
    }

    fs::rename(&tmp, &dest)
        .await
        .with_context(|| format!("moving {} to {}", tmp.display(), dest.display()))?;

    let preview_path = resolution
        .preview_image_url
        .as_deref()
        .map(|url| preview_path_for_url(&dest, url));
    if let (Some(notification_path), Some(final_path)) =
        (notification_preview_path.as_ref(), preview_path.as_ref())
        && notification_path != final_path
        && notification_path.exists()
        && let Err(e) = tokio::fs::rename(notification_path, final_path).await
    {
        warn!("Failed to move preview sidecar: {e}");
    }
    write_metadata(&dest, &resolution, &digest, preview_path.as_ref()).await;
    if let (Some(url), Some(path)) = (
        resolution.preview_image_url.as_deref(),
        preview_path.as_ref(),
    ) {
        download_preview(url, path).await;
    }
    Ok(DownloadOutcome {
        dest,
        model_type: Some(model_type_str),
        sha256: Some(digest),
        deduplicated: false,
    })
}

fn select_preview_image(images: &[ModelImage]) -> Option<&ModelImage> {
    images.iter().find(|image| {
        image
            .r#type
            .as_deref()
            .is_none_or(|kind| kind.eq_ignore_ascii_case("image"))
            && static_image_extension(&image.url).is_some()
    })
}

fn static_image_extension(url: &str) -> Option<&str> {
    let ext = url
        .split('?')
        .next()
        .unwrap_or(url)
        .rsplit('.')
        .next()
        .unwrap_or("");
    if matches!(
        ext.to_ascii_lowercase().as_str(),
        "apng" | "avif" | "bmp" | "gif" | "jpeg" | "jpg" | "png" | "svg" | "webp"
    ) {
        Some(ext)
    } else {
        None
    }
}

fn preview_path_for_url(dest: &Path, url: &str) -> PathBuf {
    let ext = static_image_extension(url).unwrap_or("jpg");
    dest.with_extension(format!("preview.{ext}"))
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
    let version_name = resolution.model_version.as_ref().map(|v| v.name.clone());
    let civitai = resolution
        .model_version
        .as_ref()
        .and_then(|v| serde_json::to_value(v).ok());

    // Extract tags and description from CivitAI metadata
    // Try model_info first (has full model details), then fall back to version.model
    let (tags, model_description) = if let Some(ref model_info) = resolution.model_info {
        let tags: Vec<String> = model_info
            .tags
            .clone()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let description = model_info.description.clone().filter(|d| !d.is_empty());
        (tags, description)
    } else if let Some(ref version) = resolution.model_version {
        let tags: Vec<String> = version
            .model
            .as_ref()
            .map(|m| m.tags.clone())
            .map(|t| t.into_iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        let description = version
            .model
            .as_ref()
            .and_then(|m| m.description.clone())
            .filter(|d| !d.is_empty());
        (tags, description)
    } else {
        (Vec::new(), None)
    };

    // Extract AutoV3 hash from CivitAI file hashes
    let autov3 = resolution
        .model_version
        .as_ref()
        .and_then(|v| {
            v.files
                .iter()
                .find(|f| {
                    f.hashes
                        .sha256
                        .as_deref()
                        .map(|h| h.eq_ignore_ascii_case(sha256))
                        .unwrap_or(false)
                })
                .and_then(|f| f.hashes.auto_v3.clone())
        })
        .filter(|h| !h.is_empty());

    // Determine base_model - fall back to Unknown if not provided
    let base_model = resolution
        .base_model
        .clone()
        .unwrap_or_else(|| "Unknown".to_string());

    // Determine model_name - fall back to file_name if not provided
    let model_name = resolution
        .model_name
        .clone()
        .unwrap_or_else(|| file_name.clone());

    let meta = ModelMetadata {
        file_name,
        model_name,
        version_name,
        file_path,
        size,
        modified,
        sha256: sha256.to_string(),
        base_model,
        preview_url,
        preview_nsfw_level: resolution.preview_nsfw_level.unwrap_or(0),
        model_description,
        notes: None,
        from_civitai: resolution.model_version.is_some(),
        civitai,
        tags,
        civitai_deleted: false,
        favorite: false,
        exclude: false,
        db_checked: false,
        skip_metadata_refresh: false,
        metadata_source: None,
        last_checked_at: 0.0,
        hash_status: "completed".to_string(),
        autov3,
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
        Ok(resp) if resp.status().is_success() && is_static_image_response(&resp) => {
            match resp.bytes().await {
                Ok(bytes) => {
                    if let Err(e) = tokio::fs::write(path, &bytes).await {
                        warn!("Failed to write preview image {}: {e}", path.display());
                    } else {
                        info!("Preview saved: {}", path.display());
                    }
                }
                Err(e) => warn!("Failed to read preview image bytes: {e}"),
            }
        }
        Ok(resp) if resp.status().is_success() => warn!(
            "Preview image request returned incompatible content type: {:?}",
            resp.headers().get(reqwest::header::CONTENT_TYPE)
        ),
        Ok(resp) => warn!("Preview image request failed with status {}", resp.status()),
        Err(e) => warn!("Failed to fetch preview image: {e}"),
    }
}

fn is_static_image_response(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|content_type| {
            let mime = content_type
                .split(';')
                .next()
                .unwrap_or(content_type)
                .trim();
            matches!(
                mime.to_ascii_lowercase().as_str(),
                "image/apng"
                    | "image/avif"
                    | "image/bmp"
                    | "image/gif"
                    | "image/jpeg"
                    | "image/png"
                    | "image/svg+xml"
                    | "image/webp"
            )
        })
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
    let preview_image = select_preview_image(&version.images);
    let preview_image_url = preview_image.map(|img| img.url.clone());
    let preview_nsfw_level = preview_image.and_then(|img| img.nsfw_level);
    let preview_path = preview_image_url
        .as_deref()
        .map(|url| preview_path_for_url(dest, url));
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
        model_info: None,
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

/// Check whether a checkpoint model is a video-generation model (LTXV, CogVideo,
/// WAN, Mochi, HunyuanVideo, etc.) These are full checkpoints that do not bundle
/// VAE/CLIP weights, so the standard "no VAE/CLIP → diffusion_models" reroute
/// should not apply to them.
pub(crate) fn is_video_checkpoint(base_model: Option<&str>) -> bool {
    let Some(bm) = base_model else {
        return false;
    };
    let bm_lower = bm.to_ascii_lowercase();
    // Known video model base_model prefixes on CivitAI.
    bm_lower.starts_with("ltxv")
        || bm_lower.starts_with("cogvideo")
        || bm_lower.starts_with("wan")
        || bm_lower.starts_with("mochi")
        || bm_lower.starts_with("hunyuanvideo")
        || bm_lower.starts_with("hunyuan")
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
    use crate::civitai::types::ModelImage;

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

    #[test]
    fn test_select_preview_image_skips_video() {
        let images = vec![
            ModelImage {
                url: "https://example.com/preview.mp4".to_string(),
                r#type: Some("video".to_string()),
                nsfw_level: Some(1),
            },
            ModelImage {
                url: "https://example.com/preview.webp".to_string(),
                r#type: Some("image".to_string()),
                nsfw_level: Some(2),
            },
        ];

        let selected = super::select_preview_image(&images).unwrap();
        assert_eq!(selected.url, "https://example.com/preview.webp");
        assert_eq!(selected.nsfw_level, Some(2));
    }

    #[test]
    fn test_select_preview_image_rejects_unknown_extension() {
        let images = vec![ModelImage {
            url: "https://example.com/preview.mp4".to_string(),
            r#type: None,
            nsfw_level: Some(1),
        }];

        assert!(super::select_preview_image(&images).is_none());
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

    #[test]
    fn test_is_video_checkpoint_ltxv() {
        assert!(super::is_video_checkpoint(Some("LTXV 2.3")));
        assert!(super::is_video_checkpoint(Some("ltxv 2")));
        assert!(super::is_video_checkpoint(Some("LTXV")));
    }

    #[test]
    fn test_is_video_checkpoint_other_video_models() {
        assert!(super::is_video_checkpoint(Some("CogVideoX")));
        assert!(super::is_video_checkpoint(Some("WAN 2.1")));
        assert!(super::is_video_checkpoint(Some("Wan2.1")));
        assert!(super::is_video_checkpoint(Some("Mochi 1")));
        assert!(super::is_video_checkpoint(Some("HunyuanVideo")));
        assert!(super::is_video_checkpoint(Some("Hunyuan")));
    }

    #[test]
    fn test_is_video_checkpoint_non_video() {
        assert!(!super::is_video_checkpoint(Some("Flux.1 D")));
        assert!(!super::is_video_checkpoint(Some("SDXL 1.0")));
        assert!(!super::is_video_checkpoint(Some("SD1.5")));
        assert!(!super::is_video_checkpoint(Some("Pony")));
        assert!(!super::is_video_checkpoint(Some("Illustrious")));
    }

    #[test]
    fn test_is_video_checkpoint_none() {
        assert!(!super::is_video_checkpoint(None));
    }
}
