pub mod schema;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadJob {
    pub id: Uuid,
    pub url: String,
    pub model_id: Option<u64>,
    pub version_id: Option<u64>,
    pub model_type: Option<String>,
    pub dest_path: Option<String>,
    pub status: JobStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Downloading,
    Verifying,
    Done,
    Failed,
    Cancelled,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "unknown".into());
        write!(f, "{s}")
    }
}

pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening catalog at {}", path.display()))?;
        let catalog = Self { conn };
        catalog.migrate()?;
        Ok(catalog)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(schema::MIGRATIONS)
            .context("running migrations")
    }

    pub fn enqueue(&self, url: &str, model_type: Option<&str>) -> Result<DownloadJob> {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        let (model_id, version_id) = parse_civitai_url(url);
        self.conn.execute(
            "INSERT INTO jobs (id, url, model_id, version_id, model_type, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?6)",
            params![id.to_string(), url, model_id, version_id, model_type, now],
        )?;
        self.get_job(id)?.context("job not found after insert")
    }

    pub fn get_job(&self, id: Uuid) -> Result<Option<DownloadJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, url, model_id, version_id, model_type, dest_path, status,
                    created_at, updated_at, error
             FROM jobs WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id.to_string()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_job(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_jobs(&self) -> Result<Vec<DownloadJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, url, model_id, version_id, model_type, dest_path, status,
                    created_at, updated_at, error
             FROM jobs ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(row_to_job(row))
        })?;
        rows.map(|r| r?.map_err(anyhow::Error::from)).collect()
    }

    pub fn set_status(&self, id: Uuid, status: JobStatus, error: Option<&str>) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE jobs SET status = ?1, error = ?2, updated_at = ?3 WHERE id = ?4",
            params![status.to_string(), error, now, id.to_string()],
        )?;
        Ok(())
    }

    pub fn next_queued(&self) -> Result<Option<DownloadJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, url, model_id, version_id, model_type, dest_path, status,
                    created_at, updated_at, error
             FROM jobs WHERE status = 'queued' ORDER BY created_at ASC LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_job(row)?))
        } else {
            Ok(None)
        }
    }
}

/// Extract (model_id, version_id) from a CivitAI URL.
pub fn parse_civitai_url(url: &str) -> (Option<u64>, Option<u64>) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let segments: Vec<&str> = path.trim_end_matches('/').split('/').collect();

    if let Some(pos) = segments.iter().position(|&s| s == "models") {
        if segments.get(pos.wrapping_sub(1)) == Some(&"download") {
            let version_id = segments.get(pos + 1).and_then(|s| s.parse().ok());
            return (None, version_id);
        }
        let model_id: Option<u64> = segments.get(pos + 1).and_then(|s| s.parse().ok());
        let version_id: Option<u64> = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("modelVersionId="))
            .and_then(|v| v.parse().ok());
        return (model_id, version_id);
    }

    (None, None)
}

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<DownloadJob> {
    let status_str: String = row.get(6)?;
    let status = match status_str.as_str() {
        "queued" => JobStatus::Queued,
        "downloading" => JobStatus::Downloading,
        "verifying" => JobStatus::Verifying,
        "done" => JobStatus::Done,
        "failed" => JobStatus::Failed,
        "cancelled" => JobStatus::Cancelled,
        _ => JobStatus::Failed,
    };
    Ok(DownloadJob {
        id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
        url: row.get(1)?,
        model_id: row.get(2)?,
        version_id: row.get(3)?,
        model_type: row.get(4)?,
        dest_path: row.get(5)?,
        status,
        created_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(7)?)
            .unwrap_or_default()
            .with_timezone(&Utc),
        updated_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(8)?)
            .unwrap_or_default()
            .with_timezone(&Utc),
        error: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_model_page_url() {
        let (model_id, version_id) = parse_civitai_url("https://civitai.com/models/12345");
        assert_eq!(model_id, Some(12345));
        assert_eq!(version_id, None);
    }

    #[test]
    fn test_parse_model_page_url_with_version() {
        let (model_id, version_id) =
            parse_civitai_url("https://civitai.com/models/12345?modelVersionId=67890");
        assert_eq!(model_id, Some(12345));
        assert_eq!(version_id, Some(67890));
    }

    #[test]
    fn test_parse_download_url() {
        let (model_id, version_id) =
            parse_civitai_url("https://civitai.com/api/download/models/67890");
        assert_eq!(model_id, None);
        assert_eq!(version_id, Some(67890));
    }

    #[test]
    fn test_parse_unknown_url() {
        let (model_id, version_id) = parse_civitai_url("https://example.com/file.safetensors");
        assert_eq!(model_id, None);
        assert_eq!(version_id, None);
    }
}
