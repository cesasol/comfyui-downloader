use crate::catalog::Catalog;
use crate::civitai::CivitaiClient;
use crate::config::Config;
use crate::daemon::downloader;
use hex;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{info, warn};

const MODEL_EXTENSIONS: &[&str] = &["safetensors", "gguf", "pt", "pth", "bin", "ckpt"];

/// How often a scan that is waiting for a key asks the keyring again. Matches
/// the refresh cooldown, so every poll actually reaches D-Bus.
const API_KEY_POLL: Duration = Duration::from_secs(60);

/// All subdirectories of the models root that may contain model files.
/// Must stay in sync with `ModelType::models_subdir` and `models_subdir_for_file`.
const KNOWN_SUBDIRS: &[&str] = &[
    "checkpoints",
    "diffusion_models",
    "embeddings",
    "loras",
    "controlnet",
    "vae",
    "upscale_models",
    "other",
];

pub async fn run(config: Arc<Config>, civitai: Arc<CivitaiClient>, catalog: Arc<Mutex<Catalog>>) {
    wait_for_api_key(&config).await;
    info!("Scanning models directory for files missing artifacts or not yet tracked in catalog");
    let models_dir = config.paths.models_dir.clone();
    let mut artifacts_updated = 0usize;
    let mut catalog_registered = 0usize;
    let mut stack: Vec<PathBuf> = KNOWN_SUBDIRS
        .iter()
        .map(|s| models_dir.join(s))
        .filter(|p| p.is_dir())
        .collect();
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
            let Ok(ft) = entry.file_type().await else {
                continue;
            };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() && is_model_file(&path) {
                let (arts, reg) = process_file(&path, &civitai, &catalog, &models_dir).await;
                if arts {
                    artifacts_updated += 1;
                }
                if reg {
                    catalog_registered += 1;
                }
            }
        }
    }
    info!(
        "Startup scan complete: artifacts updated for {artifacts_updated} file(s), \
         {catalog_registered} new catalog entries"
    );
}

/// Blocks the scan until a CivitAI API key is available, asking the keyring
/// again on a slow cadence.
///
/// The scan resolves artifacts by hash through the API, so a daemon that
/// started before the keyring was ready — or before the first `comfyui-dl
/// set-key` — must not skip it for the rest of its life. Waiting costs nothing
/// while the key is genuinely absent: the poll sleeps between attempts, and a
/// machine with no session bus fails each attempt immediately.
async fn wait_for_api_key(config: &Config) {
    if config.civitai_api_key().is_some() {
        return;
    }
    info!("No CivitAI API key yet; the startup scan runs once one is configured");
    loop {
        config.refresh_secrets().await;
        if config.civitai_api_key().is_some() {
            info!("CivitAI API key available; running the startup scan");
            return;
        }
        sleep(API_KEY_POLL).await;
    }
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
    let Some(stem) = path.file_stem() else {
        return true;
    };
    let Some(parent) = path.parent() else {
        return true;
    };
    let prefix = format!("{}.preview.", stem.to_string_lossy());
    std::fs::read_dir(parent)
        .map(|entries| {
            !entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        })
        .unwrap_or(true)
}

/// Returns `(artifacts_updated, catalog_registered)`.
async fn process_file(
    path: &PathBuf,
    civitai: &CivitaiClient,
    catalog: &Arc<Mutex<Catalog>>,
    models_dir: &Path,
) -> (bool, bool) {
    let missing_meta = needs_metadata(path);
    let missing_preview = needs_preview(path);

    if !missing_meta && !missing_preview {
        let registered = register_from_metadata(path, catalog, models_dir).await;
        return (false, registered);
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
            return (false, false);
        }
    };

    let version = match civitai.get_model_version_by_hash(&sha256).await {
        Ok(v) => v,
        Err(e) => {
            warn!("CivitAI lookup failed for {}: {e:#}", path.display());
            return (false, false);
        }
    };

    // Extract catalog fields before version is consumed by save_artifacts.
    let version_id = version.id;
    let model_id = version.model_id;
    let download_url = version
        .download_url
        .clone()
        .unwrap_or_else(|| format!("https://civitai.com/api/download/models/{version_id}"));

    downloader::save_artifacts(path, version, &sha256, missing_meta, missing_preview).await;

    let registered = register_in_catalog(
        path,
        &download_url,
        model_id,
        Some(version_id),
        Some(&sha256),
        catalog,
        models_dir,
    )
    .await;
    (true, registered)
}

/// Try to register a model that already has a `.metadata.json` sidecar.
/// Reads `civitai.id` and `civitai.modelId` from the JSON without re-hashing the file.
async fn register_from_metadata(
    path: &Path,
    catalog: &Arc<Mutex<Catalog>>,
    models_dir: &Path,
) -> bool {
    let meta_path = path.with_extension("metadata.json");
    let content = match tokio::fs::read_to_string(&meta_path).await {
        Ok(c) => c,
        Err(_) => return false,
    };
    let meta: serde_json::Value = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to parse {}: {e}", meta_path.display());
            return false;
        }
    };

    let version_id = meta["civitai"]["id"].as_u64();
    let model_id = meta["civitai"]["modelId"].as_u64();
    let sha256 = meta["sha256"].as_str();
    let download_url = meta["civitai"]["downloadUrl"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| version_id.map(|v| format!("https://civitai.com/api/download/models/{v}")));

    let Some(url) = download_url else {
        return false;
    };

    register_in_catalog(
        path, &url, model_id, version_id, sha256, catalog, models_dir,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn register_in_catalog(
    path: &Path,
    url: &str,
    model_id: Option<u64>,
    version_id: Option<u64>,
    sha256: Option<&str>,
    catalog: &Arc<Mutex<Catalog>>,
    models_dir: &Path,
) -> bool {
    let model_type = model_type_from_path(models_dir, path);
    let cat = catalog.lock().await;
    match cat.register_existing(
        url,
        model_id,
        version_id,
        model_type.as_deref(),
        path,
        crate::catalog::DownloadReason::StartupScan,
        None,
    ) {
        Ok(Some(job)) => {
            // Record the content hash so this pre-existing file participates in
            // content-addressed dedup for future downloads.
            if let Some(sha256) = sha256
                && let Err(e) = cat.set_sha256(job.id, sha256)
            {
                warn!("Failed to record sha256 for {}: {e}", path.display());
            }
            info!(
                "Registered {} in catalog (version_id={:?})",
                path.display(),
                job.version_id
            );
            true
        }
        Ok(None) => false,
        Err(e) => {
            warn!("Failed to register {} in catalog: {e}", path.display());
            false
        }
    }
}

/// Extract the first path component after `models_dir` as the model type subdirectory.
/// E.g. `/data/models/loras/Pony/lora.safetensors` → `Some("loras")`.
fn model_type_from_path(models_dir: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(models_dir)
        .ok()?
        .components()
        .next()?
        .as_os_str()
        .to_str()
        .map(|s| s.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A scan must not sit waiting when the key is already in hand: the poll
    /// would otherwise delay every startup by one interval.
    #[tokio::test]
    async fn test_wait_for_api_key_returns_at_once_when_a_key_is_present() {
        let config = Config::default();
        config
            .secrets
            .set(crate::secrets::Credential::CivitaiApiKey, "key");

        tokio::time::timeout(Duration::from_secs(1), wait_for_api_key(&config))
            .await
            .expect("a configured key must not make the scan wait");
    }

    #[test]
    fn test_model_type_from_path_loras() {
        let models_dir = Path::new("/models");
        let path = Path::new("/models/loras/Pony/my_lora.safetensors");
        assert_eq!(
            model_type_from_path(models_dir, path),
            Some("loras".to_string())
        );
    }

    #[test]
    fn test_model_type_from_path_diffusion_models() {
        let models_dir = Path::new("/data/models");
        let path = Path::new("/data/models/diffusion_models/Flux.1 D/model.gguf");
        assert_eq!(
            model_type_from_path(models_dir, path),
            Some("diffusion_models".to_string())
        );
    }

    #[test]
    fn test_model_type_from_path_not_under_models_dir() {
        let models_dir = Path::new("/models");
        let path = Path::new("/other/loras/model.safetensors");
        assert_eq!(model_type_from_path(models_dir, path), None);
    }

    #[test]
    fn test_model_type_from_path_direct_child() {
        let models_dir = Path::new("/models");
        let path = Path::new("/models/checkpoints/model.safetensors");
        assert_eq!(
            model_type_from_path(models_dir, path),
            Some("checkpoints".to_string())
        );
    }

    fn stub_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/stubs")
            .join(name)
    }

    fn load_metadata_value() -> serde_json::Value {
        let json = std::fs::read_to_string(stub_path("metadata.stub.json")).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn test_metadata_extraction_for_catalog_registration() {
        let meta = load_metadata_value();

        let version_id = meta["civitai"]["id"].as_u64();
        let model_id = meta["civitai"]["modelId"].as_u64();
        let download_url = meta["civitai"]["downloadUrl"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| version_id.map(|v| format!("https://civitai.com/api/download/models/{v}")));

        assert_eq!(version_id, Some(5550001));
        assert_eq!(model_id, Some(990001));
        assert_eq!(
            download_url.as_deref(),
            Some("https://example.com/api/download/models/5550001")
        );
    }

    #[test]
    fn test_stub_file_sha256_matches_metadata() {
        let data =
            std::fs::read(stub_path("model.stub.safetensors")).expect("failed to read stub binary");
        let digest = hex::encode(Sha256::digest(&data));

        let meta = load_metadata_value();
        let expected = meta["sha256"].as_str().expect("sha256 field missing");

        assert_eq!(digest, expected);
    }

    #[test]
    fn test_stub_file_sha256_matches_model_response() {
        use crate::civitai::types::ModelInfo;

        let data =
            std::fs::read(stub_path("model.stub.safetensors")).expect("failed to read stub binary");
        let digest = hex::encode(Sha256::digest(&data));

        let json = std::fs::read_to_string(stub_path("model_response.stub.json")).unwrap();
        let info: ModelInfo = serde_json::from_str(&json).unwrap();
        let file_hash = info.model_versions[1].files[0]
            .hashes
            .sha256
            .as_deref()
            .expect("SHA256 hash missing");

        assert_eq!(digest, file_hash.to_ascii_lowercase());
    }

    #[test]
    fn test_stub_binary_is_small() {
        let meta =
            std::fs::metadata(stub_path("model.stub.safetensors")).expect("stub binary missing");
        assert_eq!(meta.len(), 100);
    }
}
