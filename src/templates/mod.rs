//! ComfyUI default workflow-template catalog: fetch, parse, filter, feasibility.
//!
//! The upstream catalog lives in the `Comfy-Org/workflow_templates` repository.
//! `templates/index.json` groups every default template by category (image,
//! video, audio, 3d, llm) and carries the searchable metadata (title, tags,
//! model family names, total download size).  The per-template workflow JSON
//! names the individual weight files: newer templates carry them in
//! `nodes[].properties.models` (`{name, url, directory}`), older ones only list
//! them in a `MarkdownNote` "Model links" section grouped by directory heading.
//! Both sources are parsed and merged here, then sizes and checksums are filled
//! in from the HuggingFace tree API.

use crate::huggingface::{HfClient, HfFileRef, parse_hf_url};
use crate::vram::{Feasibility, VramRequirement, WorkloadKind, classify};
use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{debug, warn};

const INDEX_URL: &str =
    "https://raw.githubusercontent.com/Comfy-Org/workflow_templates/main/templates/index.json";
const TEMPLATE_BASE_URL: &str =
    "https://raw.githubusercontent.com/Comfy-Org/workflow_templates/main/templates";
const INDEX_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const WORKFLOW_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// ComfyUI `models/` subdirectories that hold weights the sampler needs on the
/// GPU while generating.
const COMPUTE_ROLES: &[&str] = &[
    "checkpoints",
    "diffusion_models",
    "unet",
    "loras",
    "controlnet",
    "model_patches",
    "style_models",
    "ipadapter",
    "gligen",
    "photomaker",
];

/// Subdirectories whose weights ComfyUI can execute on the CPU instead.
const OFFLOADABLE_ROLES: &[&str] = &[
    "text_encoders",
    "clip",
    "clip_vision",
    "vae",
    "vae_approx",
    "audio_encoders",
    "upscale_models",
    "latent_upscale_models",
    "embeddings",
    "detection",
    "geometry_estimation",
    "background_removal",
    "sams",
    "ultralytics",
];

/// A single weight file required by a template, with its ComfyUI role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateModel {
    pub file_name: String,
    pub url: String,
    /// ComfyUI `models/` subdirectory, e.g. `diffusion_models`, `vae`.
    pub role: String,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
}

impl TemplateModel {
    /// True when these weights must be resident on the GPU.
    pub fn is_compute(&self) -> bool {
        COMPUTE_ROLES.contains(&self.role.as_str())
    }

    /// True when these weights can be executed on the CPU (text encoders, VAE).
    pub fn is_offloadable(&self) -> bool {
        OFFLOADABLE_ROLES.contains(&self.role.as_str())
    }
}

/// One entry of `templates/index.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateEntry {
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    /// Model family names as published by the catalog, e.g. `Z-Image-Turbo`.
    pub model_families: Vec<String>,
    /// Category title, e.g. `Image`, `Video Tools`.
    pub category: String,
    pub kind: WorkloadKind,
    /// Total download size reported by the catalog, when it ships local models.
    pub total_size_bytes: Option<u64>,
    pub tutorial_url: Option<String>,
    /// Cloud-API template: runs on a hosted service, downloads no weights.
    pub is_api: bool,
}

/// A template plus its resolved model files and VRAM verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateBundle {
    pub template: TemplateEntry,
    pub models: Vec<TemplateModel>,
    pub requirement: VramRequirement,
    /// `None` when no GPU VRAM figure is available, so nothing can be judged.
    pub feasibility: Option<Feasibility>,
    /// Sum of the known file sizes — what downloading this bundle costs.
    pub download_bytes: u64,
}

impl TemplateBundle {
    pub fn build(
        template: TemplateEntry,
        models: Vec<TemplateModel>,
        vram_bytes: Option<u64>,
    ) -> Self {
        let requirement = requirement_for(&models);
        let feasibility = vram_bytes.map(|v| classify(requirement, v, template.kind));
        let download_bytes = models.iter().filter_map(|m| m.size_bytes).sum();
        Self {
            template,
            models,
            requirement,
            feasibility,
            download_bytes,
        }
    }

    /// Model files grouped by role, for dependency-aware display.
    pub fn models_by_role(&self) -> BTreeMap<&str, Vec<&TemplateModel>> {
        let mut out: BTreeMap<&str, Vec<&TemplateModel>> = BTreeMap::new();
        for m in &self.models {
            out.entry(m.role.as_str()).or_default().push(m);
        }
        out
    }
}

/// Split the weight bytes of `models` into GPU-resident and offloadable halves.
pub fn requirement_for(models: &[TemplateModel]) -> VramRequirement {
    let mut req = VramRequirement::default();
    for m in models {
        let Some(size) = m.size_bytes else { continue };
        if m.is_compute() {
            req.compute_bytes = req.compute_bytes.saturating_add(size);
        } else {
            // Unknown roles are treated as offloadable: they are auxiliary
            // helper models (detectors, upscalers) rather than the sampler.
            req.offloadable_bytes = req.offloadable_bytes.saturating_add(size);
        }
    }
    req
}

/// Fold a string for case- and separator-insensitive matching:
/// `"Z-Image-Turbo"` and `"image_z_image_turbo"` both contain `zimageturbo`.
fn fold(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Normalise a role heading or directory name to a ComfyUI `models/`
/// subdirectory.  Returns `None` for headings that do not name model weights
/// (input assets, prose notes).
pub fn normalize_role(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches(|c: char| c == ':' || c == '*');
    if trimmed.is_empty() {
        return None;
    }
    let folded = fold(trimmed);
    let mapped = match folded.as_str() {
        "diffusionmodel" | "diffusionmodels" | "unet" | "unetmodels" => Some("diffusion_models"),
        "textencoder" | "textencoders" | "clipmodels" => Some("text_encoders"),
        "vae" | "vaemodel" | "vaemodels" => Some("vae"),
        "lora" | "loras" => Some("loras"),
        "checkpoint" | "checkpoints" => Some("checkpoints"),
        "clip" => Some("clip"),
        "clipvision" => Some("clip_vision"),
        "controlnet" | "controlnets" => Some("controlnet"),
        "upscalemodel" | "upscalemodels" => Some("upscale_models"),
        "latentupscalemodel" | "latentupscalemodels" => Some("latent_upscale_models"),
        "embedding" | "embeddings" => Some("embeddings"),
        "audioencoder" | "audioencoders" => Some("audio_encoders"),
        "modelpatch" | "modelpatches" => Some("model_patches"),
        "stylemodel" | "stylemodels" => Some("style_models"),
        _ => None,
    };
    if let Some(role) = mapped {
        return Some(role.to_string());
    }

    // Already a directory name (single lowercase snake_case token)?
    let looks_like_dir = !trimmed.contains(' ')
        && trimmed
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if looks_like_dir
        && (COMPUTE_ROLES.contains(&trimmed)
            || OFFLOADABLE_ROLES.contains(&trimmed)
            || trimmed.ends_with("_models")
            || trimmed.ends_with("_encoders"))
    {
        return Some(trimmed.to_string());
    }
    if looks_like_dir && trimmed.len() <= 24 && !trimmed.contains("input") {
        // Rarer helper directories (detection, geometry_estimation, …) come
        // straight from the workflow and are valid ComfyUI subdirectories.
        return Some(trimmed.to_string());
    }
    None
}

/// Drop bundles with nothing to download and, unless `include_unrunnable`,
/// bundles that cannot run on this GPU.  Returns the retained bundles and how
/// many were hidden *because they cannot run* — a template that simply ships no
/// weights is not a feasibility problem and must not be reported as one.
pub fn prune_bundles(
    bundles: Vec<TemplateBundle>,
    include_unrunnable: bool,
) -> (Vec<TemplateBundle>, usize) {
    let mut bundles = bundles;
    bundles.retain(|b| !b.models.is_empty());
    let mut hidden = 0;
    if !include_unrunnable {
        let before = bundles.len();
        bundles.retain(|b| b.feasibility != Some(Feasibility::WontRun));
        hidden = before - bundles.len();
    }
    (bundles, hidden)
}

#[derive(Debug, Deserialize)]
struct RawCategory {
    #[serde(default)]
    title: String,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    templates: Vec<RawTemplate>,
}

#[derive(Debug, Deserialize)]
struct RawTemplate {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default, rename = "tutorialUrl")]
    tutorial_url: Option<String>,
}

/// Parse `templates/index.json` into flat template entries.
pub fn parse_index(body: &str) -> Result<Vec<TemplateEntry>> {
    let categories: Vec<RawCategory> =
        serde_json::from_str(body).context("parsing ComfyUI template index")?;
    let mut out = Vec::new();
    for cat in categories {
        let kind = WorkloadKind::from_template_type(&cat.kind);
        for t in cat.templates {
            let is_api = t.name.starts_with("api_")
                || t.tags.iter().any(|tag| tag.eq_ignore_ascii_case("api"));
            out.push(TemplateEntry {
                title: t.title.clone().unwrap_or_else(|| t.name.clone()),
                name: t.name,
                description: t.description,
                tags: t.tags,
                model_families: t.models.into_iter().filter(|m| m != "None").collect(),
                category: cat.title.clone(),
                kind,
                total_size_bytes: t.size,
                tutorial_url: t.tutorial_url,
                is_api,
            });
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct RawWorkflow {
    #[serde(default)]
    nodes: Vec<RawNode>,
}

#[derive(Debug, Deserialize)]
struct RawNode {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    properties: RawNodeProperties,
    #[serde(default)]
    widgets_values: serde_json::Value,
}

#[derive(Debug, Default, Deserialize)]
struct RawNodeProperties {
    #[serde(default)]
    models: Vec<RawNodeModel>,
}

#[derive(Debug, Deserialize)]
struct RawNodeModel {
    #[serde(default)]
    name: Option<String>,
    url: String,
    #[serde(default)]
    directory: Option<String>,
}

/// Extract the model files a template needs from its workflow JSON.
///
/// `nodes[].properties.models` is authoritative (it carries the target
/// directory); the `MarkdownNote` "Model links" section is used for templates
/// that predate that field.  Only HuggingFace URLs are returned — the notes
/// also link input assets on GitHub and documentation pages.
pub fn parse_workflow_models(body: &str) -> Result<Vec<TemplateModel>> {
    let workflow: RawWorkflow = serde_json::from_str(body).context("parsing workflow template")?;
    let mut models: Vec<TemplateModel> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for node in &workflow.nodes {
        for m in &node.properties.models {
            let Some(reference) = parse_hf_url(&m.url) else {
                continue;
            };
            let role = m
                .directory
                .as_deref()
                .and_then(normalize_role)
                .or_else(|| role_from_path(&reference))
                .unwrap_or_else(|| "other".to_string());
            let file_name = m
                .name
                .clone()
                .unwrap_or_else(|| reference.file_name().to_string());
            push_unique(&mut models, &mut seen, file_name, reference, role);
        }
    }

    for note in workflow.nodes.iter().filter(|n| n.r#type == "MarkdownNote") {
        let Some(text) = note.widgets_values.get(0).and_then(|v| v.as_str()) else {
            continue;
        };
        for (role, file_name, url) in parse_model_links(text) {
            let Some(reference) = parse_hf_url(&url) else {
                continue;
            };
            let role = role
                .or_else(|| role_from_path(&reference))
                .unwrap_or_else(|| "other".to_string());
            push_unique(&mut models, &mut seen, file_name, reference, role);
        }
    }

    Ok(models)
}

fn push_unique(
    models: &mut Vec<TemplateModel>,
    seen: &mut HashSet<String>,
    file_name: String,
    reference: HfFileRef,
    role: String,
) {
    let url = reference.resolve_url();
    if !seen.insert(url.clone()) {
        return;
    }
    models.push(TemplateModel {
        file_name,
        url,
        role,
        size_bytes: None,
        sha256: None,
    });
}

/// Derive the role from the file's directory inside the HuggingFace repo, e.g.
/// `split_files/text_encoders/qwen.safetensors` → `text_encoders`.
fn role_from_path(reference: &HfFileRef) -> Option<String> {
    reference
        .dir()
        .rsplit('/')
        .find(|seg| !seg.is_empty())
        .and_then(normalize_role)
}

/// Parse the `**role**` / `- [file](url)` structure of a "Model links" note.
fn parse_model_links(text: &str) -> Vec<(Option<String>, String, String)> {
    let mut out = Vec::new();
    let mut role: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed
            .strip_prefix("**")
            .and_then(|rest| rest.strip_suffix("**"))
        {
            role = normalize_role(heading);
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix("### ").or(trimmed.strip_prefix("## ")) {
            role = normalize_role(heading);
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("- ").or(trimmed.strip_prefix("* ")) else {
            continue;
        };
        let Some((label, tail)) = rest.strip_prefix('[').and_then(|r| r.split_once("](")) else {
            continue;
        };
        let Some(url) = tail.split(')').next() else {
            continue;
        };
        out.push((role.clone(), label.to_string(), url.trim().to_string()));
    }
    out
}

/// Selection filter over the catalog.  Empty fields match everything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateFilter {
    /// Free text matched against title, description, tags, families and name.
    pub text: Option<String>,
    /// Generation types, e.g. "all video models".
    pub kinds: Vec<WorkloadKind>,
    /// Task tags, e.g. "Image Edit".
    pub tasks: Vec<String>,
    /// Model families, e.g. "z-image-turbo".
    pub families: Vec<String>,
    /// Exact template names.
    pub names: Vec<String>,
    /// Include cloud-API templates (they download no weights).
    pub include_api: bool,
}

impl TemplateFilter {
    pub fn matches(&self, entry: &TemplateEntry) -> bool {
        if entry.is_api && !self.include_api {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&entry.kind) {
            return false;
        }
        if !self.names.is_empty() && !self.names.iter().any(|n| n == &entry.name) {
            return false;
        }
        if !self.tasks.is_empty() {
            let wanted: Vec<String> = self.tasks.iter().map(|t| fold(t)).collect();
            let hit = entry
                .tags
                .iter()
                .any(|tag| wanted.iter().any(|w| fold(tag).contains(w.as_str())));
            if !hit {
                return false;
            }
        }
        if !self.families.is_empty() {
            let wanted: Vec<String> = self.families.iter().map(|f| fold(f)).collect();
            let hit = wanted.iter().any(|w| {
                entry
                    .model_families
                    .iter()
                    .any(|fam| fold(fam).contains(w.as_str()))
                    || fold(&entry.name).contains(w.as_str())
            });
            if !hit {
                return false;
            }
        }
        if let Some(ref text) = self.text {
            let needle = fold(text);
            if !needle.is_empty() {
                let haystack = fold(&format!(
                    "{} {} {} {} {}",
                    entry.name,
                    entry.title,
                    entry.description.clone().unwrap_or_default(),
                    entry.tags.join(" "),
                    entry.model_families.join(" ")
                ));
                if !haystack.contains(&needle) {
                    return false;
                }
            }
        }
        true
    }
}

/// Fetches and caches the upstream catalog.
pub struct TemplateCatalog {
    http: reqwest::Client,
    cache_dir: PathBuf,
}

impl TemplateCatalog {
    pub fn new(cache_dir: PathBuf) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building template HTTP client")?;
        Ok(Self { http, cache_dir })
    }

    /// The template index, served from the on-disk cache while it is fresh.
    pub async fn index(&self, refresh: bool) -> Result<Vec<TemplateEntry>> {
        let path = self.cache_dir.join("index.json");
        let body = self
            .cached_fetch(&path, INDEX_URL, INDEX_TTL, refresh)
            .await?;
        parse_index(&body)
    }

    /// The model files of one template, served from the on-disk cache.
    pub async fn workflow_models(&self, name: &str, refresh: bool) -> Result<Vec<TemplateModel>> {
        let path = self
            .cache_dir
            .join("workflows")
            .join(format!("{name}.json"));
        let url = format!("{TEMPLATE_BASE_URL}/{name}.json");
        let body = self
            .cached_fetch(&path, &url, WORKFLOW_TTL, refresh)
            .await
            .with_context(|| format!("fetching workflow template '{name}'"))?;
        parse_workflow_models(&body)
    }

    async fn cached_fetch(
        &self,
        path: &Path,
        url: &str,
        ttl: Duration,
        refresh: bool,
    ) -> Result<String> {
        if !refresh && let Some(body) = read_fresh(path, ttl).await {
            return Ok(body);
        }
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;
        if !resp.status().is_success() {
            // A stale cache entry beats failing the whole listing.
            if let Ok(body) = tokio::fs::read_to_string(path).await {
                warn!("{url} returned {}; using stale cache", resp.status());
                return Ok(body);
            }
            anyhow::bail!("{url} returned HTTP {}", resp.status());
        }
        let body = resp.text().await.context("reading response body")?;
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if let Err(e) = tokio::fs::write(path, &body).await {
            debug!("could not cache {}: {e}", path.display());
        }
        Ok(body)
    }

    /// Resolve full bundles (model files with sizes and checksums, plus the
    /// VRAM verdict) for `entries`.
    pub async fn bundles(
        &self,
        entries: Vec<TemplateEntry>,
        hf: &HfClient,
        vram_bytes: Option<u64>,
        refresh: bool,
    ) -> Result<Vec<TemplateBundle>> {
        let mut collected: Vec<(TemplateEntry, Vec<TemplateModel>)> = stream::iter(entries)
            .map(|entry| async move {
                match self.workflow_models(&entry.name, refresh).await {
                    Ok(models) => Some((entry, models)),
                    Err(e) => {
                        warn!("skipping template '{}': {e:#}", entry.name);
                        None
                    }
                }
            })
            .buffer_unordered(8)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect();

        let sizes = self.resolve_sizes(&collected, hf).await;
        for (_, models) in collected.iter_mut() {
            for m in models.iter_mut() {
                if let Some(meta) = sizes.get(&m.url) {
                    m.size_bytes = Some(meta.0);
                    m.sha256 = meta.1.clone();
                }
            }
        }

        let mut bundles: Vec<TemplateBundle> = collected
            .into_iter()
            .map(|(entry, models)| TemplateBundle::build(entry, models, vram_bytes))
            .collect();
        bundles.sort_by(|a, b| a.template.name.cmp(&b.template.name));
        Ok(bundles)
    }

    /// One HuggingFace tree request per (repo, revision, directory) covers every
    /// file the templates reference in that directory.
    async fn resolve_sizes(
        &self,
        collected: &[(TemplateEntry, Vec<TemplateModel>)],
        hf: &HfClient,
    ) -> HashMap<String, (u64, Option<String>)> {
        let mut groups: HashSet<(String, String, String)> = HashSet::new();
        for (_, models) in collected {
            for m in models {
                if let Some(r) = parse_hf_url(&m.url) {
                    groups.insert((r.repo.clone(), r.revision.clone(), r.dir().to_string()));
                }
            }
        }

        let listings = stream::iter(groups)
            .map(|(repo, revision, dir)| async move {
                match hf.list_dir(&repo, &revision, &dir).await {
                    Ok(files) => files
                        .into_iter()
                        .map(|f| {
                            let reference = HfFileRef {
                                repo: repo.clone(),
                                revision: revision.clone(),
                                path: f.path.clone(),
                            };
                            (reference.resolve_url(), (f.size, f.sha256))
                        })
                        .collect::<Vec<_>>(),
                    Err(e) => {
                        warn!("HuggingFace listing failed for {repo}/{dir}: {e:#}");
                        Vec::new()
                    }
                }
            })
            .buffer_unordered(8)
            .collect::<Vec<_>>()
            .await;

        listings.into_iter().flatten().collect()
    }
}

/// Read `path` when it exists and is younger than `ttl`.
async fn read_fresh(path: &Path, ttl: Duration) -> Option<String> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    let age = SystemTime::now()
        .duration_since(meta.modified().ok()?)
        .unwrap_or(Duration::ZERO);
    if age > ttl {
        return None;
    }
    tokio::fs::read_to_string(path).await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX_FIXTURE: &str = r#"[
      {
        "moduleName": "default",
        "category": "Foundation",
        "title": "Image",
        "type": "image",
        "templates": [
          {
            "name": "image_z_image_turbo",
            "title": "Z-Image-Turbo: Text to Image",
            "description": "An efficient image generation model.",
            "tags": ["Image", "Text to Image"],
            "models": ["Z-Image-Turbo"],
            "size": 20830591386,
            "tutorialUrl": "https://docs.comfy.org/tutorials/image/z-image/z-image-turbo"
          },
          {
            "name": "api_nano_banana",
            "title": "Nano Banana",
            "tags": ["API", "Text to Image"],
            "models": ["Google"],
            "size": 107374182
          }
        ]
      },
      {
        "title": "Video",
        "type": "video",
        "templates": [
          {
            "name": "video_wan2_2_14B_t2v",
            "title": "Wan 2.2 14B: Text to Video",
            "tags": ["Video", "Text to Video"],
            "models": ["Wan2.2", "None"],
            "size": 42949672960
          }
        ]
      }
    ]"#;

    const WORKFLOW_FIXTURE: &str = r###"{
      "nodes": [
        {
          "type": "UNETLoader",
          "properties": {
            "models": [
              {
                "name": "z_image_turbo_bf16.safetensors",
                "url": "https://huggingface.co/Comfy-Org/z_image_turbo/resolve/main/split_files/diffusion_models/z_image_turbo_bf16.safetensors",
                "directory": "diffusion_models"
              }
            ]
          }
        },
        {
          "type": "MarkdownNote",
          "properties": {},
          "widgets_values": ["## Model links\n\n**text_encoders**\n\n- [qwen_3_4b.safetensors](https://huggingface.co/Comfy-Org/z_image_turbo/resolve/main/split_files/text_encoders/qwen_3_4b.safetensors)\n\n**Diffusion model**\n\n- [z_image_turbo_bf16.safetensors](https://huggingface.co/Comfy-Org/z_image_turbo/resolve/main/split_files/diffusion_models/z_image_turbo_bf16.safetensors)\n\n**VAE**\n\n- [ae.safetensors](https://huggingface.co/Comfy-Org/z_image_turbo/resolve/main/split_files/vae/ae.safetensors)\n\n**Input Assets**\n\n- [pose.png](https://raw.githubusercontent.com/Comfy-Org/example_workflows/main/pose.png)\n"]
        }
      ]
    }"###;

    #[test]
    fn test_parse_index_flattens_categories_and_kinds() {
        let entries = parse_index(INDEX_FIXTURE).unwrap();
        assert_eq!(entries.len(), 3);
        let z = &entries[0];
        assert_eq!(z.name, "image_z_image_turbo");
        assert_eq!(z.kind, WorkloadKind::Image);
        assert_eq!(z.category, "Image");
        assert_eq!(z.model_families, vec!["Z-Image-Turbo".to_string()]);
        assert_eq!(z.total_size_bytes, Some(20830591386));
        assert!(!z.is_api);
        let wan = entries
            .iter()
            .find(|e| e.kind == WorkloadKind::Video)
            .unwrap();
        assert_eq!(wan.name, "video_wan2_2_14B_t2v");
        // "None" is a catalog placeholder, not a model family.
        assert_eq!(wan.model_families, vec!["Wan2.2".to_string()]);
    }

    #[test]
    fn test_parse_index_flags_api_templates() {
        let entries = parse_index(INDEX_FIXTURE).unwrap();
        let api = entries
            .iter()
            .find(|e| e.name == "api_nano_banana")
            .unwrap();
        assert!(api.is_api);
    }

    #[test]
    fn test_parse_workflow_models_merges_properties_and_markdown() {
        let models = parse_workflow_models(WORKFLOW_FIXTURE).unwrap();
        let roles: Vec<&str> = models.iter().map(|m| m.role.as_str()).collect();
        assert!(roles.contains(&"diffusion_models"), "roles: {roles:?}");
        assert!(roles.contains(&"text_encoders"), "roles: {roles:?}");
        assert!(roles.contains(&"vae"), "roles: {roles:?}");
        // The diffusion model appears in both sources exactly once.
        assert_eq!(
            models
                .iter()
                .filter(|m| m.file_name == "z_image_turbo_bf16.safetensors")
                .count(),
            1
        );
        // GitHub input assets are not models.
        assert!(models.iter().all(|m| m.url.contains("huggingface.co")));
        assert_eq!(models.len(), 3);
    }

    #[test]
    fn test_parse_workflow_models_prose_role_headings() {
        let wf = r#"{"nodes":[{"type":"MarkdownNote","properties":{},"widgets_values":["**Text encoder**\n\n- [t5.safetensors](https://huggingface.co/org/repo/resolve/main/whatever/t5.safetensors)\n"]}]}"#;
        let models = parse_workflow_models(wf).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].role, "text_encoders");
    }

    #[test]
    fn test_normalize_role_maps_prose_and_directories() {
        assert_eq!(
            normalize_role("Diffusion model").as_deref(),
            Some("diffusion_models")
        );
        assert_eq!(
            normalize_role("Text encoders").as_deref(),
            Some("text_encoders")
        );
        assert_eq!(normalize_role("VAE").as_deref(), Some("vae"));
        assert_eq!(normalize_role("LoRA").as_deref(), Some("loras"));
        assert_eq!(normalize_role("vae").as_deref(), Some("vae"));
        assert_eq!(
            normalize_role("latent_upscale_models").as_deref(),
            Some("latent_upscale_models")
        );
        assert_eq!(normalize_role("Input Assets"), None);
        assert_eq!(normalize_role("Using the other Union modes"), None);
        assert_eq!(normalize_role(""), None);
    }

    fn model(role: &str, size: u64) -> TemplateModel {
        TemplateModel {
            file_name: format!("{role}.safetensors"),
            url: format!("https://huggingface.co/org/repo/resolve/main/{role}/f.safetensors"),
            role: role.to_string(),
            size_bytes: Some(size),
            sha256: None,
        }
    }

    #[test]
    fn test_requirement_splits_compute_and_offloadable() {
        let models = vec![
            model("diffusion_models", 12_000),
            model("loras", 1_000),
            model("text_encoders", 8_000),
            model("vae", 300),
        ];
        let req = requirement_for(&models);
        assert_eq!(req.compute_bytes, 13_000);
        assert_eq!(req.offloadable_bytes, 8_300);
        assert_eq!(req.total_bytes(), 21_300);
    }

    #[test]
    fn test_requirement_ignores_unknown_sizes() {
        let mut m = model("diffusion_models", 5);
        m.size_bytes = None;
        assert_eq!(requirement_for(&[m]), VramRequirement::default());
    }

    #[test]
    fn test_bundle_build_classifies_against_vram() {
        let entries = parse_index(INDEX_FIXTURE).unwrap();
        let entry = entries[0].clone();
        const GIB: u64 = 1024 * 1024 * 1024;
        let models = vec![
            model("diffusion_models", 12 * GIB),
            model("text_encoders", 8 * GIB),
            model("vae", GIB / 2),
        ];
        let bundle = TemplateBundle::build(entry.clone(), models.clone(), Some(20 * GIB));
        assert_eq!(bundle.feasibility, Some(Feasibility::CpuOffload));
        assert_eq!(bundle.download_bytes, 20 * GIB + GIB / 2);
        assert_eq!(bundle.models_by_role().keys().count(), 3);

        let huge = vec![model("diffusion_models", 40 * GIB)];
        assert_eq!(
            TemplateBundle::build(entry.clone(), huge, Some(20 * GIB)).feasibility,
            Some(Feasibility::WontRun)
        );
        let small = vec![model("diffusion_models", 4 * GIB)];
        assert_eq!(
            TemplateBundle::build(entry.clone(), small.clone(), Some(20 * GIB)).feasibility,
            Some(Feasibility::Comfortable)
        );
        // No GPU figure → no verdict, nothing gets hidden.
        assert_eq!(TemplateBundle::build(entry, small, None).feasibility, None);
    }

    #[test]
    fn test_prune_bundles_counts_only_unrunnable_as_hidden() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let entry = parse_index(INDEX_FIXTURE).unwrap()[0].clone();
        let fits = TemplateBundle::build(
            entry.clone(),
            vec![model("diffusion_models", 2 * GIB)],
            Some(20 * GIB),
        );
        let too_big = TemplateBundle::build(
            entry.clone(),
            vec![model("diffusion_models", 60 * GIB)],
            Some(20 * GIB),
        );
        let no_models = TemplateBundle::build(entry, Vec::new(), Some(20 * GIB));

        let (kept, hidden) = prune_bundles(
            vec![fits.clone(), too_big.clone(), no_models.clone()],
            false,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(hidden, 1, "only the unrunnable bundle counts as hidden");

        let (kept, hidden) = prune_bundles(vec![fits, too_big, no_models], true);
        assert_eq!(
            kept.len(),
            2,
            "unrunnable kept, weightless bundle still dropped"
        );
        assert_eq!(hidden, 0);
    }

    #[test]
    fn test_filter_by_kind_selects_all_video_models() {
        let entries = parse_index(INDEX_FIXTURE).unwrap();
        let filter = TemplateFilter {
            kinds: vec![WorkloadKind::Video],
            ..Default::default()
        };
        let hits: Vec<&str> = entries
            .iter()
            .filter(|e| filter.matches(e))
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(hits, vec!["video_wan2_2_14B_t2v"]);
    }

    #[test]
    fn test_filter_by_task_tag() {
        let entries = parse_index(INDEX_FIXTURE).unwrap();
        let filter = TemplateFilter {
            tasks: vec!["text to video".to_string()],
            ..Default::default()
        };
        let hits: Vec<&str> = entries
            .iter()
            .filter(|e| filter.matches(e))
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(hits, vec!["video_wan2_2_14B_t2v"]);
    }

    #[test]
    fn test_filter_by_family_is_separator_insensitive() {
        let entries = parse_index(INDEX_FIXTURE).unwrap();
        let filter = TemplateFilter {
            families: vec!["z-image-turbo".to_string()],
            ..Default::default()
        };
        let hits: Vec<&str> = entries
            .iter()
            .filter(|e| filter.matches(e))
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(hits, vec!["image_z_image_turbo"]);
    }

    #[test]
    fn test_filter_hides_api_templates_unless_requested() {
        let entries = parse_index(INDEX_FIXTURE).unwrap();
        let default_filter = TemplateFilter::default();
        assert!(
            !entries
                .iter()
                .any(|e| e.is_api && default_filter.matches(e))
        );
        let with_api = TemplateFilter {
            include_api: true,
            ..Default::default()
        };
        assert!(entries.iter().any(|e| e.is_api && with_api.matches(e)));
    }

    #[test]
    fn test_filter_free_text_matches_description() {
        let entries = parse_index(INDEX_FIXTURE).unwrap();
        let filter = TemplateFilter {
            text: Some("efficient image".to_string()),
            ..Default::default()
        };
        let hits: Vec<&str> = entries
            .iter()
            .filter(|e| filter.matches(e))
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(hits, vec!["image_z_image_turbo"]);
    }
}
