use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tracing::debug;

const MAX_HEADER_SIZE: u64 = 100 * 1024 * 1024;

/// Tensor-name prefixes that mean the file carries diffusion weights.
const DIFFUSION_PREFIXES: [&str; 5] = [
    "model.diffusion_model",
    "double_blocks",
    "single_blocks",
    "joint_blocks",
    "input_blocks",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelComponents {
    pub has_vae: bool,
    pub has_clip: bool,
    /// The file is an autoencoder on its own, carrying no diffusion weights.
    /// Such a file belongs in `vae/` whatever model type the source reported,
    /// because a source describes the model it ships, not each of its files.
    pub is_standalone_vae: bool,
}

/// Confirm a file really is a safetensors model.
///
/// Worth doing whenever nothing else verified the bytes: a source that answers
/// a download with an HTML error page, or a transfer that stops early, both
/// leave a file that looks like a model to every later step and fails only when
/// a user tries to load it.
pub async fn verify_parseable(path: &Path) -> Result<()> {
    read_header(path)
        .await
        .with_context(|| format!("{} is not a valid safetensors file", path.display()))?;
    Ok(())
}

/// Read and parse the safetensors header JSON.
async fn read_header(path: &Path) -> Result<HashMap<String, serde_json::Value>> {
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

    serde_json::from_slice(&header_buf).context("parsing safetensors header JSON")
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
    let header = read_header(path).await?;

    let has_vae = header
        .keys()
        .any(|k| k.starts_with("first_stage_model.") || k.starts_with("vae."));
    let has_clip = header
        .keys()
        .any(|k| k.starts_with("cond_stage_model.") || k.starts_with("conditioner.embedders."));

    // A decoder stack with no diffusion weights is an autoencoder by itself.
    // Requiring the decoder is what separates it from a text encoder, which
    // carries `encoder.*` keys and no decoder at all.
    let has_diffusion = header
        .keys()
        .any(|k| DIFFUSION_PREFIXES.iter().any(|p| k.starts_with(p)));
    let is_standalone_vae = !has_diffusion && header.keys().any(|k| k.starts_with("decoder."));

    debug!(
        "safetensors inspection: has_vae={has_vae}, has_clip={has_clip}, \
         is_standalone_vae={is_standalone_vae} ({})",
        path.display()
    );

    Ok(ModelComponents {
        has_vae,
        has_clip,
        is_standalone_vae,
    })
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

    /// A HuggingFace error page saved under a `.safetensors` name. Two such
    /// files were found in a live models tree, listed by ComfyUI as usable
    /// models, because the source published no hash so nothing was verified.
    #[tokio::test]
    async fn test_an_html_error_page_is_not_a_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qwen_image_vae.safetensors");
        tokio::fs::write(&path, b"<!doctype html>\n<html class=\"\">\n\t<head>\n")
            .await
            .unwrap();

        let error = verify_parseable(&path)
            .await
            .expect_err("an HTML page must not pass as a model file");

        assert!(
            error.to_string().contains("not a valid safetensors file"),
            "unhelpful error: {error:#}"
        );
    }

    #[tokio::test]
    async fn test_a_real_model_file_is_parseable() {
        let data = build_safetensors_bytes(&["model.diffusion_model.weight"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.safetensors");
        tokio::fs::write(&path, &data).await.unwrap();

        verify_parseable(&path).await.expect("a real model parses");
    }

    #[tokio::test]
    async fn test_a_truncated_model_file_is_rejected() {
        let mut data = build_safetensors_bytes(&["model.diffusion_model.weight"]);
        data.truncate(12);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut.safetensors");
        tokio::fs::write(&path, &data).await.unwrap();

        verify_parseable(&path)
            .await
            .expect_err("a truncated file must not pass as a model");
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

    /// Key names taken from a real `ltx-video-2b-v0.9.safetensors` (908
    /// tensors) found in a live models tree: a checkpoint that bundles its VAE
    /// under `vae.` rather than `first_stage_model.`.
    #[tokio::test]
    async fn test_a_checkpoint_bundling_vae_under_the_vae_prefix_is_detected() {
        let components = write_and_inspect(&[
            "model.diffusion_model.transformer_blocks.0.attn1.to_q.weight",
            "vae.encoder.conv_in.weight",
            "vae.decoder.conv_out.weight",
            "vae.per_channel_statistics.mean-of-means",
        ])
        .await;

        assert!(
            components.has_vae,
            "a bundled VAE under vae.* must keep this file in checkpoints"
        );
    }

    /// Key layout of a real `wan2.2_vae.safetensors` (196 tensors).
    #[tokio::test]
    async fn test_a_wan_vae_is_recognised_as_a_standalone_vae() {
        let components = write_and_inspect(&[
            "conv1.weight",
            "conv2.weight",
            "encoder.conv1.weight",
            "decoder.conv1.weight",
            "decoder.head.0.weight",
            "decoder.upsamples.0.residual.0.gamma",
        ])
        .await;

        assert!(components.is_standalone_vae);
    }

    /// Key layout of a real `flux2-vae.safetensors` (251 tensors).
    #[tokio::test]
    async fn test_a_diffusers_style_vae_is_recognised_as_a_standalone_vae() {
        let components = write_and_inspect(&[
            "bn.running_mean",
            "encoder.conv_in.weight",
            "decoder.conv_in.weight",
            "decoder.up_blocks.0.resnets.0.conv1.weight",
            "decoder.conv_out.weight",
        ])
        .await;

        assert!(components.is_standalone_vae);
    }

    /// Key layout of a real `t5xxl_fp16.safetensors` (220 tensors). A text
    /// encoder has `encoder.*` keys and no decoder, so it must never be taken
    /// for a VAE: the first version of this analysis made exactly that mistake.
    #[tokio::test]
    async fn test_a_text_encoder_is_not_a_standalone_vae() {
        let components = write_and_inspect(&[
            "shared.weight",
            "encoder.embed_tokens.weight",
            "encoder.block.0.layer.0.SelfAttention.q.weight",
            "encoder.final_layer_norm.weight",
        ])
        .await;

        assert!(!components.is_standalone_vae);
    }

    #[tokio::test]
    async fn test_a_checkpoint_that_bundles_a_vae_is_not_a_standalone_vae() {
        let components = write_and_inspect(&[
            "model.diffusion_model.transformer_blocks.0.attn1.to_q.weight",
            "vae.encoder.conv_in.weight",
            "vae.decoder.conv_out.weight",
        ])
        .await;

        assert!(components.has_vae);
        assert!(
            !components.is_standalone_vae,
            "a model that also carries diffusion weights is not just a VAE"
        );
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
