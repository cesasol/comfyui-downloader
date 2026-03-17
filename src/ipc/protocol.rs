use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Commands sent from the CLI client to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "payload", rename_all = "snake_case")]
pub enum Request {
    /// Enqueue a CivitAI model URL for download.
    AddDownload {
        url: String,
        model_type: Option<String>,
    },
    /// Return the current queue state.
    ListQueue,
    /// Return downloaded models from the catalog.
    ListModels,
    /// Return downloaded models enriched with metadata (preview, base_model, etc.).
    ListModelsEnriched,
    /// Delete a model by job ID.
    DeleteModel { id: Uuid },
    /// Trigger an immediate update scan.
    CheckUpdates,
    /// Return daemon health and active download progress.
    GetStatus,
    /// Cancel a queued or active download by ID.
    Cancel { id: Uuid },
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
    pub base_model: Option<String>,
    pub preview_path: Option<String>,
    pub preview_nsfw_level: Option<u32>,
    pub file_size: Option<u64>,
    pub sha256: Option<String>,
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
