pub mod schema;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

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
    pub download_reason: DownloadReason,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DownloadReason {
    CliAdd,
    /// Covers both the periodic scheduler and a manual `check-updates` trigger.
    UpdateAvailable,
    /// Registered as a `Done` job (not `Queued`) so the update daemon can track it.
    StartupScan,
    AccessDeniedFallback,
}

impl std::fmt::Display for DownloadReason {
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

    pub fn enqueue(
        &self,
        url: &str,
        model_type: Option<&str>,
        reason: DownloadReason,
    ) -> Result<DownloadJob> {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        let (model_id, version_id) = parse_civitai_url(url);
        self.conn.execute(
            "INSERT INTO jobs (id, url, model_id, version_id, model_type, status, created_at, updated_at, download_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?6, ?7)",
            params![id.to_string(), url, model_id, version_id, model_type, now, reason.to_string()],
        )?;
        self.get_job(id)?.context("job not found after insert")
    }

    pub fn enqueue_version_update(
        &self,
        model_id: u64,
        new_version_id: u64,
        model_type: Option<&str>,
    ) -> Result<DownloadJob> {
        let url = format!("https://civitai.com/models/{model_id}?modelVersionId={new_version_id}");
        self.enqueue(&url, model_type, DownloadReason::UpdateAvailable)
    }

    pub fn get_job(&self, id: Uuid) -> Result<Option<DownloadJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, url, model_id, version_id, model_type, dest_path, status,
                    created_at, updated_at, error, download_reason
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
                    created_at, updated_at, error, download_reason
             FROM jobs ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| Ok(row_to_job(row)))?;
        rows.map(|r| r?.map_err(anyhow::Error::from)).collect()
    }

    pub fn list_done_models(&self) -> Result<Vec<DownloadJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, url, model_id, version_id, model_type, dest_path, status,
                    created_at, updated_at, error, download_reason
             FROM jobs WHERE status = 'done' ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| Ok(row_to_job(row)))?;
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

    pub fn set_model_type(&self, id: Uuid, model_type: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE jobs SET model_type = ?1, updated_at = ?2 WHERE id = ?3",
            params![model_type, now, id.to_string()],
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

    pub fn count_by_status(&self, status: JobStatus) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM jobs WHERE status = ?1",
            params![status.to_string()],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub fn delete_job(&self, id: Uuid) -> Result<()> {
        self.conn
            .execute("DELETE FROM jobs WHERE id = ?1", params![id.to_string()])?;
        Ok(())
    }

    pub fn delete_model(&self, id: Uuid) -> Result<Vec<std::path::PathBuf>> {
        let job = self.get_job(id)?.context("job not found")?;

        let mut paths_to_delete = Vec::new();

        if let Some(dest_path) = job.dest_path {
            let model_path = std::path::PathBuf::from(&dest_path);
            let metadata_path = model_path.with_extension("metadata.json");
            let preview_path_jpg = model_path.with_extension("preview.jpg");
            let preview_path_webp = model_path.with_extension("preview.webp");

            paths_to_delete.push(model_path);
            paths_to_delete.push(metadata_path);
            paths_to_delete.push(preview_path_jpg);
            paths_to_delete.push(preview_path_webp);
        }

        self.delete_job(id)?;
        Ok(paths_to_delete)
    }

    pub fn done_jobs_for_model(&self, model_id: u64) -> Result<Vec<DownloadJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, url, model_id, version_id, model_type, dest_path, status,
                    created_at, updated_at, error, download_reason
             FROM jobs WHERE model_id = ?1 AND status = 'done'",
        )?;
        let rows = stmt.query_map(params![model_id], |row| Ok(row_to_job(row)))?;
        rows.map(|r| r?.map_err(anyhow::Error::from)).collect()
    }

    pub fn get_job_by_version_id(&self, version_id: u64) -> Result<Option<DownloadJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, url, model_id, version_id, model_type, dest_path, status,
                    created_at, updated_at, error, download_reason
             FROM jobs WHERE version_id = ?1 LIMIT 1",
        )?;
        let mut rows = stmt.query(params![version_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_job(row)?))
        } else {
            Ok(None)
        }
    }

    /// Register a model already present on disk as a `Done` job.
    ///
    /// Used by the startup scanner so that pre-existing model files become
    /// visible to the update daemon without going through the normal
    /// `Queued → Downloading → Done` lifecycle.
    ///
    /// Returns `Ok(Some(job))` on successful insertion or `Ok(None)` when a
    /// row with the same `version_id` (or, when `version_id` is `None`, the
    /// same `dest_path`) already exists, preventing duplicates on every
    /// daemon restart.
    pub fn register_existing(
        &self,
        url: &str,
        model_id: Option<u64>,
        version_id: Option<u64>,
        model_type: Option<&str>,
        dest_path: &Path,
        reason: DownloadReason,
    ) -> Result<Option<DownloadJob>> {
        // Deduplicate: prefer version_id; fall back to dest_path.
        if let Some(vid) = version_id {
            let count: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM jobs WHERE version_id = ?1",
                params![vid],
                |row| row.get(0),
            )?;
            if count > 0 {
                return Ok(None);
            }
        } else {
            let dest_str = dest_path.to_string_lossy();
            let count: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM jobs WHERE dest_path = ?1",
                params![dest_str.as_ref()],
                |row| row.get(0),
            )?;
            if count > 0 {
                return Ok(None);
            }
        }

        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO jobs
             (id, url, model_id, version_id, model_type, dest_path, status, created_at, updated_at, download_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'done', ?7, ?7, ?8)",
            params![
                id.to_string(),
                url,
                model_id,
                version_id,
                model_type,
                dest_path.to_string_lossy().as_ref(),
                now,
                reason.to_string(),
            ],
        )?;
        self.get_job(id)?
            .context("job not found after register_existing")
            .map(Some)
    }

    pub fn list_queued(&self) -> Result<Vec<DownloadJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, url, model_id, version_id, model_type, dest_path, status,
                    created_at, updated_at, error, download_reason
             FROM jobs WHERE status = 'queued' ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok(row_to_job(row)))?;
        rows.map(|r| r?.map_err(anyhow::Error::from)).collect()
    }

    pub fn next_queued(&self) -> Result<Option<DownloadJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, url, model_id, version_id, model_type, dest_path, status,
                    created_at, updated_at, error, download_reason
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
pub(crate) fn parse_civitai_url(url: &str) -> (Option<u64>, Option<u64>) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let segments: Vec<&str> = path.trim_end_matches('/').split('/').collect();

    if let Some(pos) = segments.iter().position(|&s| s == "models") {
        if pos.checked_sub(1).and_then(|i| segments.get(i)).copied() == Some("download") {
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
    let reason_str: String = row.get(10)?;
    let download_reason = match reason_str.as_str() {
        "cli_add" => DownloadReason::CliAdd,
        "update_available" => DownloadReason::UpdateAvailable,
        "startup_scan" => DownloadReason::StartupScan,
        "access_denied_fallback" => DownloadReason::AccessDeniedFallback,
        _ => DownloadReason::CliAdd,
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
        download_reason,
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

    #[test]
    fn test_parse_ambiguous_download_segment() {
        // A URL where "download" appears before "models" but is NOT the CivitAI API path.
        // Should still be treated as a download URL (same structural check).
        let (model_id, version_id) = parse_civitai_url("https://civitai.com/download/models/99999");
        assert_eq!(model_id, None);
        assert_eq!(version_id, Some(99999));
    }

    #[test]
    fn test_enqueue_version_update() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue_version_update(12345, 67890, Some("checkpoints"))
            .unwrap();
        assert_eq!(job.model_id, Some(12345));
        assert_eq!(job.version_id, Some(67890));
        assert_eq!(job.model_type.as_deref(), Some("checkpoints"));
        assert_eq!(job.status, JobStatus::Queued);
    }

    #[test]
    fn test_count_by_status() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        catalog
            .enqueue("https://civitai.com/models/1", None, DownloadReason::CliAdd)
            .unwrap();
        catalog
            .enqueue("https://civitai.com/models/2", None, DownloadReason::CliAdd)
            .unwrap();
        let count = catalog.count_by_status(JobStatus::Queued).unwrap();
        assert_eq!(count, 2);
        let done_count = catalog.count_by_status(JobStatus::Done).unwrap();
        assert_eq!(done_count, 0);
    }

    #[test]
    fn test_register_existing_inserts_done_job() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .register_existing(
                "https://civitai.com/api/download/models/67890",
                Some(12345),
                Some(67890),
                Some("loras"),
                std::path::Path::new("/models/loras/Pony/my_lora.safetensors"),
                DownloadReason::StartupScan,
            )
            .unwrap()
            .expect("should insert new row");
        assert_eq!(job.status, JobStatus::Done);
        assert_eq!(job.version_id, Some(67890));
        assert_eq!(job.model_id, Some(12345));
        assert_eq!(job.model_type.as_deref(), Some("loras"));
        assert_eq!(
            job.dest_path.as_deref(),
            Some("/models/loras/Pony/my_lora.safetensors")
        );
    }

    #[test]
    fn test_register_existing_deduplicates_by_version_id() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let first = catalog
            .register_existing(
                "https://civitai.com/api/download/models/67890",
                Some(12345),
                Some(67890),
                Some("loras"),
                std::path::Path::new("/models/loras/Pony/lora.safetensors"),
                DownloadReason::StartupScan,
            )
            .unwrap();
        assert!(first.is_some());
        let second = catalog
            .register_existing(
                "https://civitai.com/api/download/models/67890",
                Some(12345),
                Some(67890),
                Some("loras"),
                std::path::Path::new("/models/loras/Pony/lora_copy.safetensors"),
                DownloadReason::StartupScan,
            )
            .unwrap();
        assert!(second.is_none(), "duplicate version_id must return None");
    }

    #[test]
    fn test_register_existing_deduplicates_by_dest_path_when_no_version_id() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let path = std::path::Path::new("/models/other/unknown.bin");
        let first = catalog
            .register_existing(
                "https://example.com/unknown.bin",
                None,
                None,
                Some("other"),
                path,
                DownloadReason::StartupScan,
            )
            .unwrap();
        assert!(first.is_some());
        let second = catalog
            .register_existing(
                "https://example.com/unknown.bin",
                None,
                None,
                Some("other"),
                path,
                DownloadReason::StartupScan,
            )
            .unwrap();
        assert!(second.is_none(), "duplicate dest_path must return None");
    }

    #[test]
    fn test_set_dest_path() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue("https://civitai.com/models/1", None, DownloadReason::CliAdd)
            .unwrap();
        catalog
            .set_dest_path(job.id, std::path::Path::new("/tmp/model.safetensors"))
            .unwrap();
        let updated = catalog.get_job(job.id).unwrap().unwrap();
        assert_eq!(updated.dest_path.as_deref(), Some("/tmp/model.safetensors"));
    }

    fn load_metadata_value() -> serde_json::Value {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/stubs/metadata.stub.json");
        let json = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn test_catalog_register_from_stub_metadata() {
        let meta = load_metadata_value();

        let version_id = meta["civitai"]["id"].as_u64();
        let model_id = meta["civitai"]["modelId"].as_u64();
        let download_url = meta["civitai"]["downloadUrl"].as_str().unwrap();

        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .register_existing(
                download_url,
                model_id,
                version_id,
                Some("diffusion_models"),
                std::path::Path::new(
                    "/tmp/test-models/diffusion_models/Flux.1 D/syntheticTestModel_v2.safetensors",
                ),
                DownloadReason::StartupScan,
            )
            .unwrap()
            .expect("should insert new row");

        assert_eq!(job.version_id, Some(5550001));
        assert_eq!(job.model_id, Some(990001));
        assert_eq!(job.model_type.as_deref(), Some("diffusion_models"));
    }

    #[test]
    fn test_catalog_dedup_with_stub_metadata() {
        let meta = load_metadata_value();

        let version_id = meta["civitai"]["id"].as_u64();
        let model_id = meta["civitai"]["modelId"].as_u64();
        let download_url = meta["civitai"]["downloadUrl"].as_str().unwrap();
        let dest = std::path::Path::new(
            "/tmp/test-models/diffusion_models/Flux.1 D/syntheticTestModel_v2.safetensors",
        );

        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();

        let first = catalog
            .register_existing(
                download_url,
                model_id,
                version_id,
                Some("diffusion_models"),
                dest,
                DownloadReason::StartupScan,
            )
            .unwrap();
        assert!(first.is_some());

        let dup = catalog
            .register_existing(
                download_url,
                model_id,
                version_id,
                Some("diffusion_models"),
                dest,
                DownloadReason::StartupScan,
            )
            .unwrap();
        assert!(dup.is_none(), "duplicate version_id must be rejected");
    }

    #[test]
    fn test_list_done_models() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();

        let job1 = catalog
            .enqueue("https://civitai.com/models/1", None, DownloadReason::CliAdd)
            .unwrap();
        let _job2 = catalog
            .enqueue("https://civitai.com/models/2", None, DownloadReason::CliAdd)
            .unwrap();

        catalog.set_status(job1.id, JobStatus::Done, None).unwrap();

        let done_models = catalog.list_done_models().unwrap();
        assert_eq!(done_models.len(), 1);
        assert_eq!(done_models[0].id, job1.id);
        assert_eq!(done_models[0].status, JobStatus::Done);
    }

    #[test]
    fn test_delete_model() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue("https://civitai.com/models/1", None, DownloadReason::CliAdd)
            .unwrap();

        catalog
            .set_dest_path(job.id, std::path::Path::new("/tmp/model.safetensors"))
            .unwrap();

        let paths = catalog.delete_model(job.id).unwrap();

        assert_eq!(paths.len(), 4);
        assert!(paths.contains(&std::path::PathBuf::from("/tmp/model.safetensors")));
        assert!(paths.contains(&std::path::PathBuf::from("/tmp/model.metadata.json")));
        assert!(paths.contains(&std::path::PathBuf::from("/tmp/model.preview.jpg")));
        assert!(paths.contains(&std::path::PathBuf::from("/tmp/model.preview.webp")));

        assert!(catalog.get_job(job.id).unwrap().is_none());
    }
}
