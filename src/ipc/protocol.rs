use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Commands sent from the CLI client to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "payload", rename_all = "snake_case")]
pub enum Request {
    /// Enqueue a CivitAI model URL for download.
    AddDownload { url: String, model_type: Option<String> },
    /// Return the current queue state.
    ListQueue,
    /// Trigger an immediate update scan.
    CheckUpdates,
    /// Return daemon health and active download progress.
    GetStatus,
    /// Cancel a queued or active download by ID.
    Cancel { id: Uuid },
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
        Self::Err { message: msg.into() }
    }
}
