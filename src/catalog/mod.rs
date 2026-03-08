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
        self.conn.execute(
            "INSERT INTO jobs (id, url, model_type, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'queued', ?4, ?4)",
            params![id.to_string(), url, model_type, now],
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

    pub fn set_dest_path(&self, id: Uuid, path: &std::path::Path) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE jobs SET dest_path = ?1, updated_at = ?2 WHERE id = ?3",
            params![path.to_string_lossy().as_ref(), now, id.to_string()],
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_dest_path() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog.enqueue("https://civitai.com/models/1", None).unwrap();
        catalog
            .set_dest_path(job.id, std::path::Path::new("/tmp/model.safetensors"))
            .unwrap();
        let updated = catalog.get_job(job.id).unwrap().unwrap();
        assert_eq!(
            updated.dest_path.as_deref(),
            Some("/tmp/model.safetensors")
        );
    }
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
