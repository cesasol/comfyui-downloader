use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tracing::debug;

const MAX_HEADER_SIZE: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelComponents {
    pub has_vae: bool,
    pub has_clip: bool,
}

/// Read the safetensors header JSON and detect whether the file bundles
/// VAE and/or CLIP weights alongside the diffusion model.
///
/// # Format
///
/// A safetensors file starts with:
/// - 8 bytes: little-endian u64 — size of the JSON header in bytes
/// - N bytes: UTF-8 JSON object whose keys are tensor names
///
/// # Detection heuristics
///
/// - **VAE**: any key starting with `first_stage_model.`
/// - **CLIP**: any key starting with `cond_stage_model.` (SD 1.x / 2.x)
///   or `conditioner.embedders.` (SDXL)
pub async fn inspect_components(path: &Path) -> Result<ModelComponents> {
    let mut file = File::open(path)
        .await
        .with_context(|| format!("opening safetensors file {}", path.display()))?;

    let mut len_buf = [0u8; 8];
    file.read_exact(&mut len_buf)
        .await
        .context("reading safetensors header length")?;
    let header_len = u64::from_le_bytes(len_buf);

    if header_len > MAX_HEADER_SIZE {
        bail!(
            "safetensors header too large ({header_len} bytes, max {MAX_HEADER_SIZE}); \
             file may be corrupt"
        );
    }

    let mut header_buf = vec![0u8; header_len as usize];
    file.read_exact(&mut header_buf)
        .await
        .context("reading safetensors header JSON")?;

    let header: HashMap<String, serde_json::Value> =
        serde_json::from_slice(&header_buf).context("parsing safetensors header JSON")?;

    let has_vae = header.keys().any(|k| k.starts_with("first_stage_model."));
    let has_clip = header
        .keys()
        .any(|k| k.starts_with("cond_stage_model.") || k.starts_with("conditioner.embedders."));

    debug!(
        "safetensors inspection: has_vae={has_vae}, has_clip={has_clip} ({})",
        path.display()
    );

    Ok(ModelComponents { has_vae, has_clip })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_safetensors_bytes(keys: &[&str]) -> Vec<u8> {
        let mut header = serde_json::Map::new();
        for (i, key) in keys.iter().enumerate() {
            let offset_start = i * 4;
            let offset_end = offset_start + 4;
            header.insert(
                key.to_string(),
                serde_json::json!({
                    "dtype": "F32",
                    "shape": [1],
                    "data_offsets": [offset_start, offset_end]
                }),
            );
        }
        let json = serde_json::to_string(&serde_json::Value::Object(header)).unwrap();
        let json_bytes = json.as_bytes();
        let len = (json_bytes.len() as u64).to_le_bytes();

        let tensor_data = vec![0u8; keys.len() * 4];

        let mut buf = Vec::new();
        buf.extend_from_slice(&len);
        buf.extend_from_slice(json_bytes);
        buf.extend_from_slice(&tensor_data);
        buf
    }

    async fn write_and_inspect(keys: &[&str]) -> ModelComponents {
        let data = build_safetensors_bytes(keys);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.safetensors");
        tokio::fs::write(&path, &data).await.unwrap();
        inspect_components(&path).await.unwrap()
    }

    #[tokio::test]
    async fn test_full_checkpoint_has_vae_and_clip() {
        let components = write_and_inspect(&[
            "model.diffusion_model.input_blocks.0.0.weight",
            "first_stage_model.encoder.down.0.block.0.conv1.weight",
            "cond_stage_model.transformer.text_model.encoder.layers.0.self_attn.q_proj.weight",
        ])
        .await;

        assert!(components.has_vae);
        assert!(components.has_clip);
    }

    #[tokio::test]
    async fn test_sdxl_checkpoint_has_vae_and_clip() {
        let components = write_and_inspect(&[
            "model.diffusion_model.input_blocks.0.0.weight",
            "first_stage_model.decoder.up.0.block.0.conv1.weight",
            "conditioner.embedders.0.transformer.text_model.encoder.layers.0.self_attn.q_proj.weight",
        ])
        .await;

        assert!(components.has_vae);
        assert!(components.has_clip);
    }

    #[tokio::test]
    async fn test_diffusion_only_no_vae_no_clip() {
        let components = write_and_inspect(&[
            "model.diffusion_model.input_blocks.0.0.weight",
            "model.diffusion_model.output_blocks.0.0.weight",
        ])
        .await;

        assert!(!components.has_vae);
        assert!(!components.has_clip);
    }

    #[tokio::test]
    async fn test_flux_model_no_vae_no_clip() {
        let components = write_and_inspect(&[
            "double_blocks.0.img_attn.qkv.weight",
            "single_blocks.0.linear1.weight",
            "img_in.weight",
            "txt_in.weight",
            "final_layer.linear.weight",
        ])
        .await;

        assert!(!components.has_vae);
        assert!(!components.has_clip);
    }

    #[tokio::test]
    async fn test_has_vae_only() {
        let components = write_and_inspect(&[
            "model.diffusion_model.input_blocks.0.0.weight",
            "first_stage_model.encoder.down.0.block.0.conv1.weight",
        ])
        .await;

        assert!(components.has_vae);
        assert!(!components.has_clip);
    }

    #[tokio::test]
    async fn test_has_clip_only() {
        let components = write_and_inspect(&[
            "model.diffusion_model.input_blocks.0.0.weight",
            "cond_stage_model.transformer.text_model.encoder.layers.0.self_attn.q_proj.weight",
        ])
        .await;

        assert!(!components.has_vae);
        assert!(components.has_clip);
    }

    #[tokio::test]
    async fn test_empty_header_no_components() {
        let components = write_and_inspect(&[]).await;

        assert!(!components.has_vae);
        assert!(!components.has_clip);
    }

    #[tokio::test]
    async fn test_metadata_key_ignored() {
        // __metadata__ is a reserved safetensors key — must not trigger component detection.
        let mut data = build_safetensors_bytes(&[]);
        let header = serde_json::json!({
            "__metadata__": {"format": "pt"},
            "model.diffusion_model.weight": {
                "dtype": "F32", "shape": [1], "data_offsets": [0, 4]
            }
        });
        let json = serde_json::to_string(&header).unwrap();
        let json_bytes = json.as_bytes();
        let len = (json_bytes.len() as u64).to_le_bytes();
        data.clear();
        data.extend_from_slice(&len);
        data.extend_from_slice(json_bytes);
        data.extend_from_slice(&[0u8; 4]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.safetensors");
        tokio::fs::write(&path, &data).await.unwrap();
        let components = inspect_components(&path).await.unwrap();

        assert!(!components.has_vae);
        assert!(!components.has_clip);
    }
}
