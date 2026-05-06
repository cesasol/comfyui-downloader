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
    /// Open a streaming subscription connection.  The daemon will push
    /// `Frame` messages on the socket until the connection is closed.
    Subscribe,
    /// Re-download a single model (must be in `Done` state).
    RedownloadModel {
        id: Uuid,
    },
}

/// Server-pushed messages sent on a `Subscribe` connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// Acknowledgement: subscription is active.
    Subscribed,
    /// A full state snapshot pushed by the daemon.
    Snapshot(Snapshot),
    /// Server-side error on the subscription stream.
    Error { message: String },
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
    pub catalog_dirty: bool,
    pub updates_dirty: bool,
    pub seq: u64,
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
    fn frame_subscribed_round_trips() {
        let f = Frame::Subscribed;
        let s = serde_json::to_string(&f).unwrap();
        assert_eq!(s, r#"{"type":"subscribed"}"#);
        let back: Frame = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Frame::Subscribed));
    }

    #[test]
    fn request_subscribe_round_trips() {
        let r = Request::Subscribe;
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"cmd":"subscribe"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::Subscribe));
    }

    #[test]
    fn frame_snapshot_round_trips() {
        let f = Frame::Snapshot(Snapshot {
            active: vec![],
            queued: vec![],
            free_bytes: 1024,
            catalog_dirty: true,
            updates_dirty: false,
            seq: 7,
        });
        let s = serde_json::to_string(&f).unwrap();
        let back: Frame = serde_json::from_str(&s).unwrap();
        match back {
            Frame::Snapshot(snap) => {
                assert_eq!(snap.free_bytes, 1024);
                assert_eq!(snap.seq, 7);
                assert!(snap.catalog_dirty);
            }
            _ => panic!("wrong variant"),
        }
    }
}
