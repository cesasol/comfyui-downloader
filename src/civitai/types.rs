use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: u64,
    pub name: String,
    pub r#type: ModelType,
    pub model_versions: Vec<ModelVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelVersionModel {
    pub name: String,
    pub r#type: ModelType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelVersion {
    pub id: u64,
    pub name: String,
    pub model_id: Option<u64>,
    pub created_at: String,
    pub base_model: Option<String>,
    pub availability: Option<String>,
    pub download_url: Option<String>,
    pub files: Vec<ModelFile>,
    /// Nested model info (present in /model-versions/{id} responses).
    pub model: Option<ModelVersionModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    pub size: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelFile {
    pub name: String,
    #[serde(rename = "sizeKB")]
    pub size_kb: f64,
    pub hashes: FileHashes,
    pub primary: Option<bool>,
    pub download_url: Option<String>,
    pub metadata: Option<FileMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub struct FileHashes {
    pub sha256: Option<String>,
    pub blake3: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ModelType {
    Checkpoint,
    #[serde(rename = "TextualInversion")]
    Embedding,
    Hypernetwork,
    #[serde(rename = "AestheticGradient")]
    AestheticGradient,
    LORA,
    Controlnet,
    Poses,
    #[serde(rename = "VAE")]
    Vae,
    #[serde(rename = "LoCon")]
    LoCon,
    #[serde(rename = "Upscaler")]
    Upscaler,
    #[serde(other)]
    Other,
}

impl ModelType {
    /// Map to the ComfyUI models subdirectory name.
    /// For checkpoints, use `models_subdir_for_file` instead to account for pruned vs full.
    pub fn models_subdir(&self) -> &'static str {
        match self {
            Self::Checkpoint => "checkpoints",
            Self::Embedding => "embeddings",
            Self::LORA | Self::LoCon => "loras",
            Self::Controlnet => "controlnet",
            Self::Vae => "vae",
            Self::Upscaler => "upscale_models",
            _ => "other",
        }
    }

    /// Like `models_subdir` but routes checkpoints based on the file's `metadata.size`:
    /// - "pruned" → `diffusion_models` (quantised / pruned weights)
    /// - "full" or unknown → `checkpoints`
    pub fn models_subdir_for_file(&self, file: &ModelFile) -> &'static str {
        if matches!(self, Self::Checkpoint) {
            match file.metadata.as_ref().and_then(|m| m.size.as_deref()) {
                Some("pruned") => "diffusion_models",
                _ => "checkpoints",
            }
        } else {
            self.models_subdir()
        }
    }
}
