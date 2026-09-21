use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Commands sent from the CLI client to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "payload", rename_all = "snake_case")]
pub enum Request {
    AddDownload {
        url: String,
        model_type: Option<String>,
        #[serde(default)]
        preferred_file_name: Option<String>,
        /// Browsing label the user chose for this one file.
        #[serde(default)]
        family: Option<String>,
    },
    /// Enqueue several files at once (used by the template picker).
    AddDownloads {
        items: Vec<QueueItem>,
    },
    /// List the ComfyUI default workflow templates with their model bundles and
    /// the VRAM verdict for the local GPU.
    ListTemplates {
        #[serde(default)]
        filter: crate::templates::TemplateFilter,
        /// Bypass the on-disk catalog cache.
        #[serde(default)]
        refresh: bool,
        /// Keep bundles that cannot run on this GPU at all.
        #[serde(default)]
        include_unrunnable: bool,
    },
    GetVersionInfo {
        url: String,
    },
    ListQueue,
    ListModels,
    ListModelsEnriched,
    DeleteModel {
        id: Uuid,
    },
    CheckUpdates,
    GetStatus,
    Cancel {
        id: Uuid,
    },
    ListUpdates,
    DownloadVersion {
        model_id: u64,
        version_id: u64,
    },
    RedownloadMissing {
        all: bool,
    },
    /// Re-download a single model (must be in `Done` state).
    RedownloadModel {
        id: Uuid,
    },
    /// Compare the catalog against the filesystem. Reports only, unless
    /// `repair` is set.
    Diagnose {
        #[serde(default)]
        repair: bool,
    },
}

/// One file to enqueue, with the ComfyUI subdirectory it belongs in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub url: String,
    #[serde(default)]
    pub model_type: Option<String>,
    /// Browsing label chosen for this specific file. A template bundle never
    /// sets this for every file it contains.
    #[serde(default)]
    pub family: Option<String>,
    /// Family labels of every selected template that references this file. Two
    /// or more mean the file is shared across families and cannot be attributed
    /// to any one of them.
    #[serde(default)]
    pub template_families: Vec<String>,
}

/// Response payload of [`Request::ListTemplates`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateListing {
    /// GPU the verdicts were computed for, when one was detected.
    pub gpu: Option<crate::gpu::GpuInfo>,
    /// VRAM used for the verdicts (detected or configured override).
    pub vram_bytes: Option<u64>,
    /// Templates matching the filter, alphabetical by template name.
    pub bundles: Vec<crate::templates::TemplateBundle>,
    /// Bundles dropped because they cannot run on this GPU.
    pub hidden_unrunnable: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileVariantInfo {
    pub name: String,
    pub size_kb: f64,
    pub primary: Option<bool>,
    pub format: Option<String>,
    pub size: Option<String>,
    pub fp: Option<String>,
    pub quant_type: Option<String>,
    pub component_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version_id: u64,
    pub version_name: String,
    pub base_model: Option<String>,
    pub files: Vec<FileVariantInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichedModel {
    pub id: Uuid,
    pub url: String,
    pub model_id: Option<u64>,
    pub version_id: Option<u64>,
    pub model_type: Option<String>,
    pub dest_path: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub model_name: Option<String>,
    pub version_name: Option<String>,
    pub base_model: Option<String>,
    pub preview_path: Option<String>,
    pub preview_nsfw_level: Option<u32>,
    pub file_size: Option<u64>,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveJob {
    pub id: Uuid,
    pub model_name: Option<String>,
    pub version_name: Option<String>,
    pub model_type: Option<String>,
    pub bytes_received: u64,
    pub total_bytes: Option<u64>,
    pub dest_path: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub download_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedJob {
    pub id: Uuid,
    pub url: String,
    pub model_name: Option<String>,
    pub version_name: Option<String>,
    pub model_type: Option<String>,
    pub download_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub active: Vec<ActiveJob>,
    pub queued: Vec<QueuedJob>,
    pub free_bytes: u64,
}

/// Responses sent from the daemon back to the CLI client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", content = "data", rename_all = "snake_case")]
pub enum Response {
    Ok(serde_json::Value),
    Err { message: String },
}

impl Response {
    pub fn ok(data: impl Serialize) -> Self {
        Self::Ok(serde_json::to_value(data).unwrap_or(serde_json::Value::Null))
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self::Err {
            message: msg.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnose_defaults_to_reporting_only() {
        let request: Request = serde_json::from_str(r#"{"cmd":"diagnose","payload":{}}"#).unwrap();

        match request {
            Request::Diagnose { repair } => {
                assert!(!repair, "doctor must not change anything unasked")
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn per_file_placement_overrides_round_trip() {
        let request = Request::AddDownload {
            url: "https://civitai.com/models/1".to_string(),
            model_type: Some("diffusion_models".to_string()),
            preferred_file_name: None,
            family: Some("My Flux Pile".to_string()),
        };

        let text = serde_json::to_string(&request).unwrap();
        let back: Request = serde_json::from_str(&text).unwrap();

        match back {
            Request::AddDownload {
                model_type, family, ..
            } => {
                assert_eq!(model_type.as_deref(), Some("diffusion_models"));
                assert_eq!(family.as_deref(), Some("My Flux Pile"));
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn a_queue_item_without_an_override_decodes_as_absent() {
        let item: QueueItem =
            serde_json::from_str(r#"{"url":"https://example.com/m.safetensors"}"#).unwrap();

        assert_eq!(item.model_type, None);
        assert_eq!(item.family, None);
        assert!(item.template_families.is_empty());
    }

    #[test]
    fn snapshot_round_trips() {
        let snap = Snapshot {
            active: vec![],
            queued: vec![],
            free_bytes: 1024,
        };
        let s = serde_json::to_string(&snap).unwrap();
        let back: Snapshot = serde_json::from_str(&s).unwrap();
        assert_eq!(back.free_bytes, 1024);
        assert!(back.active.is_empty());
        assert!(back.queued.is_empty());
    }
}
