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
    #[serde(default)]
    pub images: Vec<ModelImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelImage {
    pub url: String,
    #[serde(default)]
    pub nsfw_level: Option<u32>,
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
    #[serde(rename = "LORA")]
    Lora,
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
    pub fn models_subdir(&self) -> &'static str {
        match self {
            Self::Checkpoint => "checkpoints",
            Self::Embedding => "embeddings",
            Self::Lora | Self::LoCon => "loras",
            Self::Controlnet => "controlnet",
            Self::Vae => "vae",
            Self::Upscaler => "upscale_models",
            _ => "other",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/stubs")
            .join(name)
    }

    fn load_model_info() -> ModelInfo {
        let json = std::fs::read_to_string(stub_path("model_response.stub.json")).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn load_metadata_value() -> serde_json::Value {
        let json = std::fs::read_to_string(stub_path("metadata.stub.json")).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn test_deserialize_model_response_stub() {
        let info = load_model_info();

        assert_eq!(info.id, 990001);
        assert_eq!(info.name, "Dream Weaver");
        assert_eq!(info.r#type, ModelType::Checkpoint);
        assert_eq!(info.model_versions.len(), 3);
    }

    #[test]
    fn test_model_response_version_fields() {
        let info = load_model_info();

        let ea = &info.model_versions[0];
        assert_eq!(ea.id, 5550003);
        assert_eq!(ea.name, "ZImg V1");
        assert_eq!(ea.base_model.as_deref(), Some("ZImageBase"));
        assert_eq!(ea.availability.as_deref(), Some("EarlyAccess"));

        let flux = &info.model_versions[1];
        assert_eq!(flux.id, 5550002);
        assert_eq!(flux.name, "Flux Dev V2");
        assert_eq!(flux.base_model.as_deref(), Some("Flux.1 D"));
        assert_eq!(flux.availability.as_deref(), Some("Public"));

        let sdxl = &info.model_versions[2];
        assert_eq!(sdxl.id, 5550001);
        assert_eq!(sdxl.base_model.as_deref(), Some("SDXL 1.0"));
    }

    #[test]
    fn test_model_response_file_hashes() {
        let info = load_model_info();

        let flux_file = &info.model_versions[1].files[0];
        assert_eq!(
            flux_file.hashes.sha256.as_deref(),
            Some("16EE9E0BFAE1B44EF42E8805155E3A08264B8B48E3C913D9F75234661EF4E88A")
        );
        assert!(flux_file.primary == Some(true));
        assert_eq!(flux_file.name, "dreamWeaver_fluxDevV2.safetensors");
    }

    #[test]
    fn test_deserialize_metadata_civitai_as_model_version() {
        let meta = load_metadata_value();
        let version: ModelVersion = serde_json::from_value(meta["civitai"].clone())
            .expect("civitai field must deserialize as ModelVersion");

        assert_eq!(version.id, 5550001);
        assert_eq!(version.model_id, Some(990001));
        assert_eq!(version.name, "v2");
        assert_eq!(version.base_model.as_deref(), Some("Flux.1 D"));
        assert_eq!(
            version.download_url.as_deref(),
            Some("https://example.com/api/download/models/5550001")
        );
        assert_eq!(version.files.len(), 1);
        assert_eq!(version.images.len(), 1);
    }

    #[test]
    fn test_metadata_civitai_model_type() {
        let meta = load_metadata_value();
        let version: ModelVersion = serde_json::from_value(meta["civitai"].clone()).unwrap();

        let model = version.model.expect("nested model field must be present");
        assert_eq!(model.name, "Synthetic Test Model");
        assert_eq!(model.r#type, ModelType::Checkpoint);
    }

    #[test]
    fn test_checkpoint_always_routes_to_checkpoints() {
        assert_eq!(ModelType::Checkpoint.models_subdir(), "checkpoints");
    }

    #[test]
    fn test_lora_routes_to_loras() {
        assert_eq!(ModelType::Lora.models_subdir(), "loras");
        assert_eq!(ModelType::LoCon.models_subdir(), "loras");
    }

    #[test]
    fn test_vae_routes_to_vae() {
        assert_eq!(ModelType::Vae.models_subdir(), "vae");
    }
}
