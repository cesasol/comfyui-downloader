use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::daemon::downloader;
use hex;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

const MODEL_EXTENSIONS: &[&str] = &["safetensors", "gguf", "pt", "pth", "bin", "ckpt"];

pub async fn run(config: Arc<Config>, civitai: Arc<CivitaiClient>) {
    if config.civitai.api_key.is_none() {
        warn!("Skipping startup scan: no CivitAI API key configured");
        return;
    }
    info!("Scanning models directory for files with missing metadata or preview images");
    let models_dir = config.paths.models_dir.clone();
    let mut count = 0usize;
    let mut stack = vec![models_dir];
    while let Some(dir) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) => {
                warn!("Cannot read directory {}: {e}", dir.display());
                continue;
            }
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Ok(ft) = entry.file_type().await else { continue };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() && is_model_file(&path) {
                if process_file(&path, &civitai).await {
                    count += 1;
                }
            }
        }
    }
    info!("Startup scan complete: updated artifacts for {count} file(s)");
}

fn is_model_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| MODEL_EXTENSIONS.contains(&e))
        .unwrap_or(false)
}

fn needs_metadata(path: &Path) -> bool {
    !path.with_extension("metadata.json").exists()
}

fn needs_preview(path: &Path) -> bool {
    let Some(stem) = path.file_stem() else { return true };
    let Some(parent) = path.parent() else { return true };
    let prefix = format!("{}.preview.", stem.to_string_lossy());
    std::fs::read_dir(parent)
        .map(|entries| !entries.flatten().any(|e| e.file_name().to_string_lossy().starts_with(&prefix)))
        .unwrap_or(true)
}

async fn process_file(path: &PathBuf, civitai: &CivitaiClient) -> bool {
    let missing_meta = needs_metadata(path);
    let missing_preview = needs_preview(path);
    if !missing_meta && !missing_preview {
        return false;
    }
    info!(
        "Found {} (missing:{}{})",
        path.display(),
        if missing_meta { " metadata" } else { "" },
        if missing_preview { " preview" } else { "" },
    );
    let sha256 = match compute_sha256(path.clone()).await {
        Ok(h) => h,
        Err(e) => {
            warn!("Failed to hash {}: {e}", path.display());
            return false;
        }
    };
    let version = match civitai.get_model_version_by_hash(&sha256).await {
        Ok(v) => v,
        Err(e) => {
            warn!("CivitAI lookup failed for {}: {e:#}", path.display());
            return false;
        }
    };
    downloader::save_artifacts(path, version, &sha256, missing_meta, missing_preview).await;
    true
}

async fn compute_sha256(path: PathBuf) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 128 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await?
}
