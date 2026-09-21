pub mod schema;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    pub available_version_id: Option<u64>,
    pub available_version_name: Option<String>,
    pub last_update_check: Option<DateTime<Utc>>,
    pub preferred_file_name: Option<String>,
    /// SHA-256 of the file content, when known. Populated from the source
    /// metadata before download and reconciled with the computed digest after.
    /// Used to deduplicate byte-identical files across repos and platforms.
    pub sha256: Option<String>,
    /// Role the user named for this specific file, replayed whenever placement
    /// is resolved again. `None` means the user named none, never "unknown".
    pub role_override: Option<String>,
    /// Role a template declared for this file.
    pub template_declared_role: Option<String>,
    /// Family labels of the templates that asked for this file.
    pub template_families: Vec<String>,
    /// Browsing label the user chose for this specific file.
    pub family_override: Option<String>,
    /// How `model_type` was arrived at. `None` on rows written before
    /// placement provenance existed; absence is not a declaration.
    pub role_source: Option<crate::placement::RoleSource>,
    /// Browsing label the file was filed under.
    pub family: Option<String>,
    /// Source label before aliasing.
    pub raw_family: Option<String>,
    /// Which evidence level supplied `family`.
    pub family_source: Option<crate::placement::FamilySource>,
}

/// What the catalog and the filesystem disagree about.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogReport {
    /// Rows naming a file that is not on disk. Left alone, `redownload-missing`
    /// would fetch every one of them again.
    pub dangling: Vec<DownloadJob>,
    /// Paths named by more than one row, with how many rows name them.
    pub duplicate_paths: Vec<(String, u64)>,
}

/// What a repair changed, and what it refused to decide.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepairOutcome {
    pub removed_duplicates: u64,
    pub removed_dangling: u64,
    /// Rows naming a missing file that nothing else accounts for. Kept, because
    /// each is the only remaining record of that model.
    pub unresolved: Vec<DownloadJob>,
}

/// Explicit per-file placement instructions carried with a queued job. A user
/// override and a template declaration are stored apart: both are authoritative
/// over inference, but only the user's outranks the template's.
#[derive(Debug, Clone, Default)]
pub struct PlacementOverrides {
    pub user_role: Option<String>,
    pub template_role: Option<String>,
    pub user_family: Option<String>,
    pub template_families: Vec<String>,
}

const JOB_COLUMNS: &str = "id, url, model_id, version_id, model_type, dest_path, status, \
     created_at, updated_at, error, download_reason, \
     available_version_id, available_version_name, last_update_check, preferred_file_name, sha256, \
     role_override, family_override, role_source, family, raw_family, family_source, \
     template_declared_role, template_families";

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
            .context("running migrations")?;
        for alter in schema::ALTER_MIGRATIONS {
            let _ = self.conn.execute(alter, []);
        }
        Ok(())
    }

    pub fn enqueue(
        &self,
        url: &str,
        model_type: Option<&str>,
        reason: DownloadReason,
        preferred_file_name: Option<&str>,
    ) -> Result<DownloadJob> {
        self.enqueue_with_placement(
            url,
            model_type,
            reason,
            preferred_file_name,
            &PlacementOverrides::default(),
        )
    }

    /// Enqueue a job carrying explicit per-file placement instructions.
    pub fn enqueue_with_placement(
        &self,
        url: &str,
        model_type: Option<&str>,
        reason: DownloadReason,
        preferred_file_name: Option<&str>,
        overrides: &PlacementOverrides,
    ) -> Result<DownloadJob> {
        let (model_id, version_id) = parse_civitai_url(url);

        // If we already have a non-terminal-or-completed job for this exact
        // request, return it instead of inserting a duplicate. This prevents
        // redownload-missing (and any caller) from accumulating duplicate done
        // rows for the same file. A version alone does not identify content.
        if let Some(vid) = version_id
            && let Some(existing) =
                self.find_active_or_done_job_by_version(vid, model_type, preferred_file_name)?
        {
            return Ok(existing);
        }

        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO jobs (id, url, model_id, version_id, model_type, status, created_at, updated_at, download_reason, preferred_file_name, role_override, family_override, template_declared_role, template_families)
             VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![id.to_string(), url, model_id, version_id, model_type, now, reason.to_string(), preferred_file_name, overrides.user_role, overrides.user_family, overrides.template_role, encode_template_families(&overrides.template_families)],
        )?;
        self.get_job(id)?.context("job not found after insert")
    }

    /// Returns the first job matching `version_id` whose status indicates it is
    /// in flight (`queued`/`downloading`/`verifying`) or already completed
    /// (`done`). Failed/cancelled rows are ignored so the user can re-add and
    /// retry after a failure.
    fn find_active_or_done_job_by_version(
        &self,
        version_id: u64,
        model_type: Option<&str>,
        preferred_file_name: Option<&str>,
    ) -> Result<Option<DownloadJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs \
             WHERE version_id = ?1 \
               AND (model_type IS ?2 OR ?2 IS NULL) \
               AND (preferred_file_name IS ?3 OR ?3 IS NULL) \
               AND status IN ('queued', 'downloading', 'verifying', 'done') \
             ORDER BY CASE status \
                        WHEN 'done' THEN 0 \
                        WHEN 'downloading' THEN 1 \
                        WHEN 'verifying' THEN 2 \
                        WHEN 'queued' THEN 3 \
                        ELSE 4 END, \
                      created_at ASC \
             LIMIT 1"
        ))?;
        let mut rows = stmt.query(params![version_id, model_type, preferred_file_name])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_job(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn enqueue_version_update(
        &self,
        model_id: u64,
        new_version_id: u64,
        model_type: Option<&str>,
    ) -> Result<DownloadJob> {
        let url = format!("https://civitai.com/models/{model_id}?modelVersionId={new_version_id}");
        self.enqueue(&url, model_type, DownloadReason::UpdateAvailable, None)
    }

    pub fn get_job(&self, id: Uuid) -> Result<Option<DownloadJob>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"))?;
        let mut rows = stmt.query(params![id.to_string()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_job(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_jobs(&self) -> Result<Vec<DownloadJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map([], |row| Ok(row_to_job(row)))?;
        rows.map(|r| r?.map_err(anyhow::Error::from)).collect()
    }

    pub fn list_done_models(&self) -> Result<Vec<DownloadJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE status = 'done' ORDER BY created_at DESC"
        ))?;
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

    /// Store a resolved placement: the role, how it was reached, and the family
    /// attribution behind it, so the decision can be explained after a restart.
    pub fn record_placement(
        &self,
        id: Uuid,
        placement: &crate::placement::Placement,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE jobs SET model_type = ?2, role_source = ?3, family = ?4, raw_family = ?5, \
             family_source = ?6, updated_at = ?7 WHERE id = ?1",
            params![
                id.to_string(),
                placement.role,
                placement.role_source.as_str(),
                placement.family,
                placement.raw_family,
                placement.family_source.as_str(),
                Utc::now().to_rfc3339(),
            ],
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

    /// Record the SHA-256 of a job's file. Stored lowercase so lookups are
    /// case-insensitive across sources (CivitAI and HuggingFace differ).
    pub fn set_sha256(&self, id: Uuid, sha256: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE jobs SET sha256 = ?1, updated_at = ?2 WHERE id = ?3",
            params![sha256.to_ascii_lowercase(), now, id.to_string()],
        )?;
        Ok(())
    }

    /// Find a completed job whose file has the given SHA-256 and still exists on
    /// disk. Excludes `exclude_id` so a job never matches itself. This is the
    /// content-addressed dedup: it catches byte-identical files served from a
    /// different repo or platform (HuggingFace vs CivitAI), which URL/version
    /// matching cannot see through.
    pub fn find_done_job_by_sha256(
        &self,
        sha256: &str,
        exclude_id: Uuid,
    ) -> Result<Option<DownloadJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs \
             WHERE sha256 = ?1 AND status = 'done' AND id != ?2 \
             ORDER BY created_at ASC"
        ))?;
        let mut rows = stmt.query(params![sha256.to_ascii_lowercase(), exclude_id.to_string()])?;
        while let Some(row) = rows.next()? {
            let job = row_to_job(row)?;
            // A `done` row whose file was deleted is stale; keep looking.
            if job
                .dest_path
                .as_ref()
                .is_some_and(|p| std::path::Path::new(p).exists())
            {
                return Ok(Some(job));
            }
        }
        Ok(None)
    }

    /// Compare the catalog against the filesystem without changing anything.
    pub fn diagnose(&self) -> Result<CatalogReport> {
        let mut dangling = Vec::new();
        for job in self.list_jobs()? {
            if let Some(path) = job.dest_path.as_deref()
                && !std::path::Path::new(path).exists()
            {
                dangling.push(job);
            }
        }
        let mut stmt = self.conn.prepare(
            "SELECT dest_path, count(*) FROM jobs WHERE dest_path IS NOT NULL \
             GROUP BY dest_path HAVING count(*) > 1 ORDER BY dest_path",
        )?;
        let duplicate_paths = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(CatalogReport {
            dangling,
            duplicate_paths,
        })
    }

    /// Reconcile the catalog with the filesystem.
    ///
    /// Two rows naming one path collapse onto the one that still carries the
    /// CivitAI identifiers, because losing those loses update tracking for that
    /// model. A row naming a missing file is dropped only when another row
    /// accounts for the same content; otherwise it is reported and kept.
    pub fn repair(&self) -> Result<RepairOutcome> {
        let mut outcome = RepairOutcome::default();
        let jobs = self.list_jobs()?;
        let exists = |p: &str| std::path::Path::new(p).exists();

        let mut by_path: HashMap<&str, Vec<&DownloadJob>> = HashMap::new();
        for job in &jobs {
            if let Some(path) = job.dest_path.as_deref() {
                by_path.entry(path).or_default().push(job);
            }
        }
        let mut dropped: Vec<Uuid> = Vec::new();
        for (_path, mut rows) in by_path {
            if rows.len() < 2 {
                continue;
            }
            rows.sort_by_key(|job| {
                (
                    job.model_id.is_none(),
                    job.version_id.is_none(),
                    job.sha256.is_none(),
                    job.created_at,
                )
            });
            for job in rows.into_iter().skip(1) {
                dropped.push(job.id);
                outcome.removed_duplicates += 1;
            }
        }

        let live: Vec<&DownloadJob> = jobs
            .iter()
            .filter(|job| job.dest_path.as_deref().is_some_and(exists))
            .collect();
        for job in &jobs {
            let Some(path) = job.dest_path.as_deref() else {
                continue;
            };
            if exists(path) || dropped.contains(&job.id) {
                continue;
            }
            let name = std::path::Path::new(path).file_name();
            let covered = live.iter().any(|other| {
                let same_content = match (job.sha256.as_deref(), other.sha256.as_deref()) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    _ => false,
                };
                let same_name = name.is_some()
                    && std::path::Path::new(other.dest_path.as_deref().unwrap_or_default())
                        .file_name()
                        == name;
                same_content || same_name
            });
            if covered {
                dropped.push(job.id);
                outcome.removed_dangling += 1;
            } else {
                outcome.unresolved.push(job.clone());
            }
        }

        for id in &dropped {
            self.delete_job(*id)?;
        }
        Ok(outcome)
    }

    pub fn count_by_status(&self, status: JobStatus) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM jobs WHERE status = ?1",
            params![status.to_string()],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub fn list_jobs_by_status(&self, status: JobStatus) -> Result<Vec<DownloadJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE status = ?1 ORDER BY created_at ASC"
        ))?;
        let rows = stmt.query_map(params![status.to_string()], |row| Ok(row_to_job(row)))?;
        rows.map(|r| r?.map_err(anyhow::Error::from)).collect()
    }

    pub fn delete_job(&self, id: Uuid) -> Result<()> {
        self.conn
            .execute("DELETE FROM jobs WHERE id = ?1", params![id.to_string()])?;
        Ok(())
    }

    /// Delete one placement and return the files that belong to it.
    ///
    /// Only files this placement owns are returned. Older rows can share a
    /// single path, and a distinct placement can hold the same bytes at another
    /// path; neither may be unlinked because this one was deleted.
    pub fn delete_model(&self, id: Uuid) -> Result<Vec<std::path::PathBuf>> {
        let job = self.get_job(id)?.context("job not found")?;

        let mut paths_to_delete = Vec::new();

        if let Some(dest_path) = job.dest_path.filter(|path| {
            self.other_jobs_sharing_path(id, path)
                .map(|count| count == 0)
                .unwrap_or(false)
        }) {
            let model_path = std::path::PathBuf::from(&dest_path);
            let metadata_path = model_path.with_extension("metadata.json");
            let placement_path = model_path.with_extension("placement.json");
            let preview_path_jpg = model_path.with_extension("preview.jpg");
            let preview_path_webp = model_path.with_extension("preview.webp");

            paths_to_delete.push(model_path);
            paths_to_delete.push(metadata_path);
            paths_to_delete.push(placement_path);
            paths_to_delete.push(preview_path_jpg);
            paths_to_delete.push(preview_path_webp);
        }

        Ok(paths_to_delete)
    }

    /// Drop the catalog row once its files are known to be gone. Kept separate
    /// from [`Catalog::delete_model`] so a failed unlink leaves the information
    /// needed to retry rather than an orphaned file with no record.
    pub fn forget_model(&self, id: Uuid) -> Result<()> {
        self.delete_job(id)
    }

    /// How many other rows still name this exact path.
    fn other_jobs_sharing_path(&self, id: Uuid, dest_path: &str) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM jobs WHERE dest_path = ?1 AND id != ?2",
            params![dest_path, id.to_string()],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub fn done_jobs_for_model(&self, model_id: u64) -> Result<Vec<DownloadJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE model_id = ?1 AND status = 'done'"
        ))?;
        let rows = stmt.query_map(params![model_id], |row| Ok(row_to_job(row)))?;
        rows.map(|r| r?.map_err(anyhow::Error::from)).collect()
    }

    pub fn get_job_by_version_id(&self, version_id: u64) -> Result<Option<DownloadJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE version_id = ?1 LIMIT 1"
        ))?;
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
    #[allow(clippy::too_many_arguments)]
    pub fn register_existing(
        &self,
        url: &str,
        model_id: Option<u64>,
        version_id: Option<u64>,
        model_type: Option<&str>,
        dest_path: &Path,
        reason: DownloadReason,
        preferred_file_name: Option<&str>,
    ) -> Result<Option<DownloadJob>> {
        // A placement is identified by its path. A path already in the catalog
        // must not gain a second record, and a path that is not in the catalog
        // is a placement of its own even when its version is known elsewhere:
        // one file can legitimately sit in two folders.
        let dest_str = dest_path.to_string_lossy();
        let tracked: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM jobs WHERE dest_path = ?1",
            params![dest_str.as_ref()],
            |row| row.get(0),
        )?;
        if tracked > 0 {
            return Ok(None);
        }

        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO jobs
             (id, url, model_id, version_id, model_type, dest_path, status, created_at, updated_at, download_reason, preferred_file_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'done', ?7, ?7, ?8, ?9)",
            params![
                id.to_string(),
                url,
                model_id,
                version_id,
                model_type,
                dest_path.to_string_lossy().as_ref(),
                now,
                reason.to_string(),
                preferred_file_name,
            ],
        )?;
        self.get_job(id)?
            .context("job not found after register_existing")
            .map(Some)
    }

    pub fn flag_update_available(
        &self,
        model_id: u64,
        new_version_id: u64,
        version_name: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE jobs SET available_version_id = ?1, available_version_name = ?2, updated_at = ?3
             WHERE model_id = ?4 AND status = 'done'",
            params![new_version_id, version_name, now, model_id],
        )?;
        Ok(())
    }

    pub fn clear_update_flag(&self, model_id: u64) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE jobs SET available_version_id = NULL, available_version_name = NULL, updated_at = ?1
             WHERE model_id = ?2 AND status = 'done'",
            params![now, model_id],
        )?;
        Ok(())
    }

    pub fn list_updates_available(&self) -> Result<Vec<DownloadJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs \
             WHERE status = 'done' AND available_version_id IS NOT NULL \
             ORDER BY updated_at DESC"
        ))?;
        let rows = stmt.query_map([], |row| Ok(row_to_job(row)))?;
        rows.map(|r| r?.map_err(anyhow::Error::from)).collect()
    }

    pub fn set_last_update_check(&self, model_id: u64) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE jobs SET last_update_check = ?1
             WHERE model_id = ?2 AND status = 'done'",
            params![now, model_id],
        )?;
        Ok(())
    }

    pub fn should_check_update(&self, model_id: u64) -> Result<bool> {
        let last_check: Option<String> = self.conn.query_row(
            "SELECT last_update_check FROM jobs
             WHERE model_id = ?1 AND status = 'done'
             ORDER BY last_update_check DESC LIMIT 1",
            params![model_id],
            |row| row.get(0),
        )?;
        match last_check {
            None => Ok(true),
            Some(ts) => {
                let parsed = DateTime::parse_from_rfc3339(&ts)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_default();
                let elapsed = Utc::now().signed_duration_since(parsed);
                Ok(elapsed.num_hours() >= 24)
            }
        }
    }

    /// Collapse duplicate `done` rows that point at the same model/version.
    ///
    /// Earlier code paths (CLI re-adds, repeated `enqueue_version_update`
    /// calls) inserted a fresh row each time, so a single completed model
    /// could end up with many `done` rows sharing a `version_id` and
    /// `dest_path`. That broke `requeue_done`, which would flip every copy
    /// to `queued` and download the same file once per duplicate.
    ///
    /// Grouping key: `version_id` when set, otherwise `dest_path`. Rows with
    /// neither are left alone. Within a group we keep the row with a
    /// non-empty `dest_path` and the earliest `created_at`; the rest are
    /// deleted. Returns the number of rows removed.
    pub fn dedupe_done_jobs(&self) -> Result<usize> {
        #[derive(Hash, Eq, PartialEq)]
        enum DedupKey {
            Version(u64),
            Dest(String),
        }

        let candidates = self.list_done_models()?;
        let mut groups: HashMap<DedupKey, Vec<DownloadJob>> = HashMap::new();
        for job in candidates {
            let key = match job.version_id {
                Some(v) => DedupKey::Version(v),
                None => match job.dest_path.as_deref() {
                    Some(p) if !p.is_empty() => DedupKey::Dest(p.to_string()),
                    _ => continue,
                },
            };
            groups.entry(key).or_default().push(job);
        }

        let mut deleted = 0;
        for (_, mut group) in groups {
            if group.len() < 2 {
                continue;
            }
            // Canonical row first: prefer non-null dest_path, then earliest created_at.
            group.sort_by(|a, b| {
                let a_missing = a.dest_path.as_deref().map(|p| p.is_empty()).unwrap_or(true);
                let b_missing = b.dest_path.as_deref().map(|p| p.is_empty()).unwrap_or(true);
                a_missing
                    .cmp(&b_missing)
                    .then_with(|| a.created_at.cmp(&b.created_at))
            });
            for dup in &group[1..] {
                self.delete_job(dup.id)?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Cancel any `queued`/`downloading`/`verifying` jobs whose `version_id`
    /// already has a `done` row. These can only exist because of pre-fix
    /// duplicate enqueues; processing them would download the same file
    /// again. Returns the number of rows cancelled.
    pub fn cancel_redundant_pending_jobs(&self) -> Result<usize> {
        let now = Utc::now().to_rfc3339();
        let affected = self.conn.execute(
            "UPDATE jobs SET status = 'cancelled', \
                              error = 'duplicate of an already-completed version', \
                              updated_at = ?1 \
             WHERE status IN ('queued', 'downloading', 'verifying') \
               AND version_id IS NOT NULL \
               AND EXISTS ( \
                 SELECT 1 FROM jobs d \
                 WHERE d.status = 'done' AND d.version_id = jobs.version_id \
               )",
            params![now],
        )?;
        Ok(affected)
    }

    /// Flip `Done` jobs back to `Queued` so the worker re-downloads them.
    ///
    /// When `only_missing` is true, jobs whose `dest_path` still exists on
    /// disk are skipped. Returns the jobs that were re-queued.
    pub fn requeue_done(&self, only_missing: bool) -> Result<Vec<DownloadJob>> {
        // Collapse duplicates first so we don't redownload the same file once
        // per duplicate row.
        self.dedupe_done_jobs()?;

        let candidates = self.list_done_models()?;
        let now = Utc::now().to_rfc3339();
        let mut requeued = Vec::new();
        for job in candidates {
            if only_missing
                && job
                    .dest_path
                    .as_deref()
                    .map(|p| Path::new(p).exists())
                    .unwrap_or(false)
            {
                continue;
            }
            self.conn.execute(
                "UPDATE jobs SET status = 'queued', dest_path = NULL, error = NULL, updated_at = ?1 \
                 WHERE id = ?2",
                params![now, job.id.to_string()],
            )?;
            if let Some(updated) = self.get_job(job.id)? {
                requeued.push(updated);
            }
        }
        Ok(requeued)
    }

    /// Re-queue a single model by ID, recycling the existing row in place
    /// (UUID, model_id, version_id, model_type, url all preserved).
    /// Errors if the row isn't `Done`.
    pub fn requeue_one(&self, id: Uuid) -> Result<DownloadJob> {
        let job = self.get_job(id)?.context("model not found")?;
        if job.status != JobStatus::Done {
            anyhow::bail!("model is not in Done state (status = {})", job.status);
        }
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE jobs SET status = 'queued', dest_path = NULL, error = NULL, updated_at = ?1 \
             WHERE id = ?2",
            params![now, id.to_string()],
        )?;
        self.get_job(id)?.context("job not found after requeue_one")
    }

    pub fn list_queued(&self) -> Result<Vec<DownloadJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE status = 'queued' ORDER BY created_at ASC"
        ))?;
        let rows = stmt.query_map([], |row| Ok(row_to_job(row)))?;
        rows.map(|r| r?.map_err(anyhow::Error::from)).collect()
    }

    pub fn next_queued(&self) -> Result<Option<DownloadJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE status = 'queued' ORDER BY created_at ASC LIMIT 1"
        ))?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_job(row)?))
        } else {
            Ok(None)
        }
    }

    /// Reset jobs left mid-flight by a crashed or restarted daemon back to
    /// `queued` so the worker picks them up again. `Downloading`/`Verifying`
    /// rows only ever reach a terminal state through a running worker; if the
    /// process dies first they are stranded, since `next_queued` never selects
    /// them. Returns the number of jobs that were re-queued.
    pub fn requeue_interrupted(&self) -> Result<usize> {
        let now = Utc::now().to_rfc3339();
        let count = self.conn.execute(
            "UPDATE jobs SET status = 'queued', error = NULL, updated_at = ?1 \
             WHERE status IN ('downloading', 'verifying')",
            params![now],
        )?;
        Ok(count)
    }
}

/// Store template family labels as JSON. `None` when there are none, so an
/// absent value stays absent rather than becoming an empty claim.
fn encode_template_families(labels: &[String]) -> Option<String> {
    if labels.is_empty() {
        return None;
    }
    serde_json::to_string(labels).ok()
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
    let last_update_check: Option<DateTime<Utc>> = row
        .get::<_, Option<String>>(13)?
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc));
    let preferred_file_name: Option<String> = row.get(14)?;
    let sha256: Option<String> = row.get(15)?;
    let role_override: Option<String> = row.get(16)?;
    let family_override: Option<String> = row.get(17)?;
    let role_source = row
        .get::<_, Option<String>>(18)?
        .as_deref()
        .and_then(crate::placement::RoleSource::parse);
    let family: Option<String> = row.get(19)?;
    let raw_family: Option<String> = row.get(20)?;
    let family_source = row
        .get::<_, Option<String>>(21)?
        .as_deref()
        .and_then(crate::placement::FamilySource::parse);
    let template_declared_role: Option<String> = row.get(22)?;
    let template_families: Vec<String> = row
        .get::<_, Option<String>>(23)?
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
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
        available_version_id: row.get(11)?,
        available_version_name: row.get(12)?,
        last_update_check,
        preferred_file_name,
        sha256,
        role_override,
        template_declared_role,
        template_families,
        family_override,
        role_source,
        family,
        raw_family,
        family_source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done_job_at(catalog: &Catalog, url: &str, dest: &str, sha256: &str) -> Uuid {
        let job = catalog
            .enqueue(url, Some("loras"), DownloadReason::CliAdd, None)
            .unwrap();
        catalog
            .set_dest_path(job.id, std::path::Path::new(dest))
            .unwrap();
        catalog.set_sha256(job.id, sha256).unwrap();
        catalog.set_status(job.id, JobStatus::Done, None).unwrap();
        job.id
    }

    #[test]
    fn test_scanning_does_not_add_a_second_record_for_a_tracked_path() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let path = std::path::Path::new("/models/loras/m.safetensors");
        done_job_at(
            &catalog,
            "https://civitai.com/models/30?modelVersionId=31",
            "/models/loras/m.safetensors",
            "dd",
        );

        let registered = catalog
            .register_existing(
                "https://civitai.com/models/30?modelVersionId=32",
                Some(30),
                Some(32),
                Some("loras"),
                path,
                DownloadReason::StartupScan,
                None,
            )
            .unwrap();

        assert!(
            registered.is_none(),
            "a path already in the catalog must not gain a second record"
        );
    }

    #[test]
    fn test_scanning_records_a_second_placement_of_a_known_version() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        done_job_at(
            &catalog,
            "https://civitai.com/models/40?modelVersionId=41",
            "/models/loras/m.safetensors",
            "ee",
        );

        let registered = catalog
            .register_existing(
                "https://civitai.com/models/40?modelVersionId=41",
                Some(40),
                Some(41),
                Some("checkpoints"),
                std::path::Path::new("/models/checkpoints/m.safetensors"),
                DownloadReason::StartupScan,
                None,
            )
            .unwrap();

        assert!(
            registered.is_some(),
            "a distinct path is a distinct placement even for a known version"
        );
    }

    #[test]
    fn test_one_version_wanted_in_two_roles_yields_two_jobs() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let url = "https://civitai.com/models/20?modelVersionId=21";

        let first = catalog
            .enqueue(url, Some("checkpoints"), DownloadReason::CliAdd, None)
            .unwrap();
        let second = catalog
            .enqueue(url, Some("diffusion_models"), DownloadReason::CliAdd, None)
            .unwrap();

        assert_ne!(
            first.id, second.id,
            "two roles are two placements, not one job"
        );
    }

    #[test]
    fn test_repair_keeps_the_row_that_can_still_track_updates() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("model.safetensors");
        std::fs::write(&live, b"weights").unwrap();
        let path = live.to_str().unwrap();
        // the rich row: a CivitAI url gives it a model_id, so the updater can use it
        let rich = done_job_at(
            &catalog,
            "https://civitai.com/models/80?modelVersionId=81",
            path,
            "cc",
        );
        // the poor row: registered by a scan, hash only
        let poor = done_job_at(
            &catalog,
            "https://huggingface.co/org/repo/resolve/main/m.safetensors",
            path,
            "cc",
        );

        let outcome = catalog.repair().unwrap();

        assert_eq!(outcome.removed_duplicates, 1);
        assert!(
            catalog.get_job(rich).unwrap().is_some(),
            "the update-trackable row must survive"
        );
        assert!(catalog.get_job(poor).unwrap().is_none());
    }

    #[test]
    fn test_repair_drops_a_dangling_row_whose_content_lives_elsewhere() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("model.safetensors");
        std::fs::write(&live, b"weights").unwrap();
        let kept = done_job_at(
            &catalog,
            "https://civitai.com/models/90",
            live.to_str().unwrap(),
            "dd",
        );
        let stale = done_job_at(
            &catalog,
            "https://civitai.com/models/91",
            "/models/loras/old.safetensors",
            "dd",
        );

        let outcome = catalog.repair().unwrap();

        assert_eq!(outcome.removed_dangling, 1);
        assert!(catalog.get_job(stale).unwrap().is_none());
        assert!(catalog.get_job(kept).unwrap().is_some());
    }

    #[test]
    fn test_repair_keeps_a_dangling_row_nothing_else_covers() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let orphan = done_job_at(
            &catalog,
            "https://civitai.com/models/92",
            "/models/loras/vanished.safetensors",
            "ee",
        );

        let outcome = catalog.repair().unwrap();

        assert_eq!(outcome.removed_dangling, 0);
        assert_eq!(outcome.unresolved.len(), 1);
        assert!(
            catalog.get_job(orphan).unwrap().is_some(),
            "a row that is the only record of a model must not be discarded silently"
        );
    }

    #[test]
    fn test_diagnose_reports_dangling_rows_and_duplicate_paths() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("present.safetensors");
        std::fs::write(&live, b"weights").unwrap();

        // one row whose file is gone
        done_job_at(
            &catalog,
            "https://civitai.com/models/70",
            "/models/loras/gone.safetensors",
            "aa",
        );
        // two rows naming one live path
        done_job_at(
            &catalog,
            "https://civitai.com/models/71",
            live.to_str().unwrap(),
            "bb",
        );
        done_job_at(
            &catalog,
            "https://civitai.com/models/72",
            live.to_str().unwrap(),
            "bb",
        );

        let report = catalog.diagnose().unwrap();

        assert_eq!(report.dangling.len(), 1);
        assert!(
            report.dangling[0]
                .dest_path
                .as_deref()
                .unwrap()
                .ends_with("gone.safetensors")
        );
        assert_eq!(report.duplicate_paths.len(), 1);
        assert_eq!(report.duplicate_paths[0].1, 2);
    }

    #[test]
    fn test_a_routine_update_check_leaves_an_established_placement_alone() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue(
                "https://civitai.com/models/60?modelVersionId=61",
                Some("checkpoints"),
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        // A file the old routing rules put somewhere the current rules would not.
        let established = "/models/checkpoints/Wan2.1/model.safetensors";
        catalog
            .set_dest_path(job.id, std::path::Path::new(established))
            .unwrap();
        catalog
            .record_placement(
                job.id,
                &crate::placement::Placement {
                    role: "checkpoints".to_string(),
                    role_source: crate::placement::RoleSource::SourceMetadata,
                    family: Some("Wan2.1".to_string()),
                    raw_family: Some("Wan2.1".to_string()),
                    family_source: crate::placement::FamilySource::FileSpecificMetadata,
                    relative_dir: std::path::PathBuf::from("checkpoints/Wan2.1"),
                },
            )
            .unwrap();
        catalog.set_status(job.id, JobStatus::Done, None).unwrap();

        catalog.flag_update_available(60, 62, "v2").unwrap();

        let reloaded = catalog.get_job(job.id).unwrap().unwrap();
        assert_eq!(reloaded.dest_path.as_deref(), Some(established));
        assert_eq!(reloaded.model_type.as_deref(), Some("checkpoints"));
        assert_eq!(reloaded.family.as_deref(), Some("Wan2.1"));
        assert_eq!(reloaded.available_version_id, Some(62));
    }

    #[test]
    fn test_one_version_wanted_as_two_file_variants_yields_two_jobs() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let url = "https://civitai.com/models/50?modelVersionId=51";

        let fp16 = catalog
            .enqueue(
                url,
                Some("checkpoints"),
                DownloadReason::CliAdd,
                Some("model-fp16.safetensors"),
            )
            .unwrap();
        let fp8 = catalog
            .enqueue(
                url,
                Some("checkpoints"),
                DownloadReason::CliAdd,
                Some("model-fp8.safetensors"),
            )
            .unwrap();

        assert_ne!(
            fp16.id, fp8.id,
            "file selection changes the requested content, so these are two placements"
        );
    }

    #[test]
    fn test_one_version_wanted_twice_in_one_role_yields_one_job() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let url = "https://civitai.com/models/22?modelVersionId=23";

        let first = catalog
            .enqueue(url, Some("loras"), DownloadReason::CliAdd, None)
            .unwrap();
        let second = catalog
            .enqueue(url, Some("loras"), DownloadReason::CliAdd, None)
            .unwrap();

        assert_eq!(first.id, second.id);
    }

    #[test]
    fn test_a_placement_is_only_forgotten_after_its_files_are_gone() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let id = done_job_at(
            &catalog,
            "https://civitai.com/models/14",
            "/models/loras/m.safetensors",
            "cc",
        );

        let paths = catalog.delete_model(id).unwrap();

        assert!(!paths.is_empty());
        assert!(
            catalog.get_job(id).unwrap().is_some(),
            "the row must survive until the caller reports the files were removed"
        );

        catalog.forget_model(id).unwrap();

        assert!(catalog.get_job(id).unwrap().is_none());
    }

    #[test]
    fn test_deleting_a_placement_spares_a_path_another_job_still_references() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let shared = "/models/loras/shared.safetensors";
        let first = done_job_at(&catalog, "https://civitai.com/models/10", shared, "aa");
        done_job_at(&catalog, "https://civitai.com/models/11", shared, "aa");

        let paths = catalog.delete_model(first).unwrap();

        assert!(
            !paths.iter().any(|p| p == std::path::Path::new(shared)),
            "a path another job still points at must not be unlinked: {paths:?}"
        );
    }

    #[test]
    fn test_deleting_one_placement_leaves_a_distinct_placement_of_the_same_bytes() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let mine = "/models/loras/Flux.1 D/model.safetensors";
        let first = done_job_at(&catalog, "https://civitai.com/models/12", mine, "bb");
        let second = done_job_at(
            &catalog,
            "https://civitai.com/models/13",
            "/models/checkpoints/Flux.1 D/model.safetensors",
            "bb",
        );

        let paths = catalog.delete_model(first).unwrap();

        assert!(paths.iter().any(|p| p == std::path::Path::new(mine)));
        assert!(
            catalog.get_job(second).unwrap().is_some(),
            "the other placement of the same bytes must survive"
        );
    }

    #[test]
    fn test_a_template_declaration_is_not_stored_as_a_user_override() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let overrides = PlacementOverrides {
            template_role: Some("checkpoints".to_string()),
            ..PlacementOverrides::default()
        };

        let job = catalog
            .enqueue_with_placement(
                "https://huggingface.co/org/repo/resolve/main/m.safetensors",
                Some("checkpoints"),
                DownloadReason::CliAdd,
                None,
                &overrides,
            )
            .unwrap();
        let reloaded = catalog.get_job(job.id).unwrap().unwrap();

        assert_eq!(
            reloaded.template_declared_role.as_deref(),
            Some("checkpoints")
        );
        assert_eq!(reloaded.role_override, None);
    }

    #[test]
    fn test_a_recorded_placement_decision_survives_a_round_trip() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue(
                "https://civitai.com/models/2",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        let placement = crate::placement::Placement {
            role: "diffusion_models".to_string(),
            role_source: crate::placement::RoleSource::SourceMetadata,
            family: Some("Wan Video 2.2".to_string()),
            raw_family: Some("Wan2.2".to_string()),
            family_source: crate::placement::FamilySource::FileSpecificMetadata,
            relative_dir: std::path::PathBuf::from("diffusion_models/Wan Video 2.2"),
        };

        catalog.record_placement(job.id, &placement).unwrap();
        let reloaded = catalog.get_job(job.id).unwrap().unwrap();

        assert_eq!(reloaded.model_type.as_deref(), Some("diffusion_models"));
        assert_eq!(
            reloaded.role_source,
            Some(crate::placement::RoleSource::SourceMetadata)
        );
        assert_eq!(reloaded.family.as_deref(), Some("Wan Video 2.2"));
        assert_eq!(reloaded.raw_family.as_deref(), Some("Wan2.2"));
        assert_eq!(
            reloaded.family_source,
            Some(crate::placement::FamilySource::FileSpecificMetadata)
        );
    }

    #[test]
    fn test_a_legacy_row_reports_no_placement_provenance() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        catalog
            .conn
            .execute(
                "INSERT INTO jobs (id, url, model_type, status, created_at, updated_at, download_reason)
                 VALUES (?1, ?2, 'checkpoints', 'done', ?3, ?3, 'cli_add')",
                params![id.to_string(), "https://civitai.com/models/3", now],
            )
            .unwrap();

        let job = catalog.get_job(id).unwrap().unwrap();

        assert_eq!(job.model_type.as_deref(), Some("checkpoints"));
        assert_eq!(job.role_source, None);
        assert_eq!(job.family, None);
        assert_eq!(job.family_source, None);
        assert_eq!(job.role_override, None);
    }

    #[test]
    fn test_placement_overrides_survive_a_round_trip() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let overrides = PlacementOverrides {
            user_role: Some("diffusion_models".to_string()),
            user_family: Some("My Flux Pile".to_string()),
            ..PlacementOverrides::default()
        };

        let job = catalog
            .enqueue_with_placement(
                "https://civitai.com/models/1",
                None,
                DownloadReason::CliAdd,
                None,
                &overrides,
            )
            .unwrap();
        let reloaded = catalog.get_job(job.id).unwrap().unwrap();

        assert_eq!(reloaded.role_override.as_deref(), Some("diffusion_models"));
        assert_eq!(reloaded.family_override.as_deref(), Some("My Flux Pile"));
    }

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
            .enqueue(
                "https://civitai.com/models/1",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        catalog
            .enqueue(
                "https://civitai.com/models/2",
                None,
                DownloadReason::CliAdd,
                None,
            )
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
                None,
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
    fn test_register_existing_keeps_two_paths_of_one_version_apart() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let first = catalog
            .register_existing(
                "https://civitai.com/api/download/models/67890",
                Some(12345),
                Some(67890),
                Some("loras"),
                std::path::Path::new("/models/loras/Pony/lora.safetensors"),
                DownloadReason::StartupScan,
                None,
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
                None,
            )
            .unwrap();
        assert!(
            second.is_some(),
            "a second file of the same version at another path is another placement"
        );
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
                None,
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
                None,
            )
            .unwrap();
        assert!(second.is_none(), "duplicate dest_path must return None");
    }

    #[test]
    fn test_set_dest_path() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue(
                "https://civitai.com/models/1",
                None,
                DownloadReason::CliAdd,
                None,
            )
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
                None,
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
                None,
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
                None,
            )
            .unwrap();
        assert!(dup.is_none(), "duplicate version_id must be rejected");
    }

    #[test]
    fn test_list_done_models() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();

        let job1 = catalog
            .enqueue(
                "https://civitai.com/models/1",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        let _job2 = catalog
            .enqueue(
                "https://civitai.com/models/2",
                None,
                DownloadReason::CliAdd,
                None,
            )
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
            .enqueue(
                "https://civitai.com/models/1",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();

        catalog
            .set_dest_path(job.id, std::path::Path::new("/tmp/model.safetensors"))
            .unwrap();

        let paths = catalog.delete_model(job.id).unwrap();

        assert_eq!(paths.len(), 5);
        assert!(paths.contains(&std::path::PathBuf::from("/tmp/model.safetensors")));
        assert!(paths.contains(&std::path::PathBuf::from("/tmp/model.metadata.json")));
        assert!(paths.contains(&std::path::PathBuf::from("/tmp/model.preview.jpg")));
        assert!(paths.contains(&std::path::PathBuf::from("/tmp/model.preview.webp")));
        assert!(
            paths.contains(&std::path::PathBuf::from("/tmp/model.placement.json")),
            "the placement-local sidecar belongs to this placement: {paths:?}"
        );

        catalog.forget_model(job.id).unwrap();
        assert!(catalog.get_job(job.id).unwrap().is_none());
    }

    fn create_done_model(catalog: &Catalog, model_id: u64, version_id: u64) -> DownloadJob {
        let url = format!("https://civitai.com/models/{model_id}?modelVersionId={version_id}");
        let job = catalog
            .enqueue(&url, Some("checkpoints"), DownloadReason::CliAdd, None)
            .unwrap();
        catalog.set_status(job.id, JobStatus::Done, None).unwrap();
        catalog.get_job(job.id).unwrap().unwrap()
    }

    #[test]
    fn test_flag_update_available() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = create_done_model(&catalog, 100, 200);

        catalog.flag_update_available(100, 300, "v3").unwrap();

        let updated = catalog.get_job(job.id).unwrap().unwrap();
        assert_eq!(updated.available_version_id, Some(300));
        assert_eq!(updated.available_version_name.as_deref(), Some("v3"));
    }

    #[test]
    fn test_clear_update_flag() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        create_done_model(&catalog, 100, 200);

        catalog.flag_update_available(100, 300, "v3").unwrap();
        catalog.clear_update_flag(100).unwrap();

        let updates = catalog.list_updates_available().unwrap();
        assert!(updates.is_empty());
    }

    #[test]
    fn test_list_updates_available() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        create_done_model(&catalog, 100, 200);
        create_done_model(&catalog, 101, 201);

        catalog.flag_update_available(100, 300, "v3").unwrap();

        let updates = catalog.list_updates_available().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].model_id, Some(100));
        assert_eq!(updates[0].available_version_id, Some(300));
    }

    #[test]
    fn test_requeue_done_only_missing_skips_present_files() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue(
                "https://civitai.com/models/1",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        let tmp = std::env::temp_dir().join(format!("requeue-test-{}.bin", job.id));
        std::fs::write(&tmp, b"x").unwrap();
        catalog.set_dest_path(job.id, &tmp).unwrap();
        catalog.set_status(job.id, JobStatus::Done, None).unwrap();

        let requeued = catalog.requeue_done(true).unwrap();
        assert!(requeued.is_empty(), "present file must not be re-queued");
        let after = catalog.get_job(job.id).unwrap().unwrap();
        assert_eq!(after.status, JobStatus::Done);

        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_requeue_done_only_missing_requeues_missing_files() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue(
                "https://civitai.com/models/1",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        catalog
            .set_dest_path(
                job.id,
                std::path::Path::new("/nonexistent/missing.safetensors"),
            )
            .unwrap();
        catalog.set_status(job.id, JobStatus::Done, None).unwrap();

        let requeued = catalog.requeue_done(true).unwrap();
        assert_eq!(requeued.len(), 1);
        let after = catalog.get_job(job.id).unwrap().unwrap();
        assert_eq!(after.status, JobStatus::Queued);
        assert!(after.dest_path.is_none());
    }

    #[test]
    fn test_requeue_done_all_requeues_everything() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue(
                "https://civitai.com/models/1",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        let tmp = std::env::temp_dir().join(format!("requeue-all-{}.bin", job.id));
        std::fs::write(&tmp, b"x").unwrap();
        catalog.set_dest_path(job.id, &tmp).unwrap();
        catalog.set_status(job.id, JobStatus::Done, None).unwrap();

        let requeued = catalog.requeue_done(false).unwrap();
        assert_eq!(requeued.len(), 1, "--all must re-queue even present files");
        let after = catalog.get_job(job.id).unwrap().unwrap();
        assert_eq!(after.status, JobStatus::Queued);

        std::fs::remove_file(&tmp).unwrap();
    }

    /// Insert a `done` row directly, bypassing `enqueue`'s dedup so tests can
    /// reproduce the duplicate-row state seen in real catalogs.
    fn insert_raw_done(
        catalog: &Catalog,
        model_id: Option<u64>,
        version_id: Option<u64>,
        dest: Option<&str>,
        when: &str,
    ) -> Uuid {
        let id = Uuid::new_v4();
        catalog.conn.execute(
            "INSERT INTO jobs (id, url, model_id, version_id, model_type, dest_path, status, created_at, updated_at, download_reason) \
             VALUES (?1, 'https://example.test/raw', ?2, ?3, NULL, ?4, 'done', ?5, ?5, 'cli_add')",
            params![id.to_string(), model_id, version_id, dest, when],
        ).unwrap();
        id
    }

    #[test]
    fn test_dedupe_done_jobs_collapses_duplicates_by_version_id() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let dest = "/tmp/file.safetensors";

        // 3 rows with dest_path set, 2 with empty — same version_id.
        let keep = insert_raw_done(
            &catalog,
            Some(480835),
            Some(717403),
            Some(dest),
            "2026-03-09T00:00:00+00:00",
        );
        let _newer_with_dest = insert_raw_done(
            &catalog,
            Some(480835),
            Some(717403),
            Some(dest),
            "2026-03-10T00:00:00+00:00",
        );
        let _newer_with_dest2 = insert_raw_done(
            &catalog,
            Some(480835),
            Some(717403),
            Some(dest),
            "2026-03-13T00:00:00+00:00",
        );
        let _empty_dest = insert_raw_done(
            &catalog,
            None,
            Some(717403),
            None,
            "2026-03-16T00:00:00+00:00",
        );
        let _empty_dest2 = insert_raw_done(
            &catalog,
            None,
            Some(717403),
            Some(""),
            "2026-03-16T01:00:00+00:00",
        );

        let removed = catalog.dedupe_done_jobs().unwrap();
        assert_eq!(removed, 4, "must remove 4 duplicate rows");

        let remaining = catalog.list_done_models().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, keep, "must keep oldest row with dest_path");
    }

    #[test]
    fn test_dedupe_done_jobs_collapses_by_dest_path_when_version_null() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let dest = "/tmp/legacy.bin";

        let keep = insert_raw_done(
            &catalog,
            None,
            None,
            Some(dest),
            "2026-03-01T00:00:00+00:00",
        );
        let _dup = insert_raw_done(
            &catalog,
            None,
            None,
            Some(dest),
            "2026-03-02T00:00:00+00:00",
        );

        assert_eq!(catalog.dedupe_done_jobs().unwrap(), 1);
        let remaining = catalog.list_done_models().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, keep);
    }

    #[test]
    fn test_dedupe_done_jobs_leaves_distinct_versions_alone() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        insert_raw_done(
            &catalog,
            Some(1),
            Some(100),
            Some("/a"),
            "2026-03-01T00:00:00+00:00",
        );
        insert_raw_done(
            &catalog,
            Some(1),
            Some(101),
            Some("/b"),
            "2026-03-02T00:00:00+00:00",
        );
        assert_eq!(catalog.dedupe_done_jobs().unwrap(), 0);
        assert_eq!(catalog.list_done_models().unwrap().len(), 2);
    }

    #[test]
    fn test_requeue_done_collapses_duplicates_and_requeues_once() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        // 3 done rows for the same version, all pointing at a missing path.
        for ts in [
            "2026-03-09T00:00:00+00:00",
            "2026-03-10T00:00:00+00:00",
            "2026-03-13T00:00:00+00:00",
        ] {
            insert_raw_done(
                &catalog,
                Some(42),
                Some(999),
                Some("/nonexistent/dup.safetensors"),
                ts,
            );
        }

        let requeued = catalog.requeue_done(true).unwrap();
        assert_eq!(
            requeued.len(),
            1,
            "must requeue exactly one job per unique version"
        );
        // And no zombie done rows left behind.
        assert!(catalog.list_done_models().unwrap().is_empty());
    }

    #[test]
    fn test_enqueue_returns_existing_done_job_for_same_version() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let url = "https://civitai.com/models/100?modelVersionId=200";
        let first = catalog
            .enqueue(url, None, DownloadReason::CliAdd, None)
            .unwrap();
        catalog.set_status(first.id, JobStatus::Done, None).unwrap();

        let second = catalog
            .enqueue(url, None, DownloadReason::CliAdd, None)
            .unwrap();
        assert_eq!(
            second.id, first.id,
            "second enqueue must return the existing done job, not insert"
        );
        let total: i64 = catalog
            .conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE version_id = 200",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 1);
    }

    #[test]
    fn test_enqueue_allows_retry_after_failure() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let url = "https://civitai.com/models/100?modelVersionId=200";
        let first = catalog
            .enqueue(url, None, DownloadReason::CliAdd, None)
            .unwrap();
        catalog
            .set_status(first.id, JobStatus::Failed, Some("boom"))
            .unwrap();

        let second = catalog
            .enqueue(url, None, DownloadReason::CliAdd, None)
            .unwrap();
        assert_ne!(
            second.id, first.id,
            "failed job must not block fresh enqueue"
        );
    }

    #[test]
    fn test_enqueue_returns_existing_in_flight_job_for_same_version() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let url = "https://civitai.com/models/100?modelVersionId=200";
        let first = catalog
            .enqueue(url, None, DownloadReason::CliAdd, None)
            .unwrap();

        let second = catalog
            .enqueue(url, None, DownloadReason::CliAdd, None)
            .unwrap();
        assert_eq!(
            second.id, first.id,
            "second enqueue must not double-queue an in-flight version"
        );
    }

    #[test]
    fn test_cancel_redundant_pending_jobs_cancels_dup_of_done() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        // Existing done row for version 200.
        let done = catalog
            .enqueue(
                "https://civitai.com/models/100?modelVersionId=200",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        catalog.set_status(done.id, JobStatus::Done, None).unwrap();
        // A leftover queued row for the SAME version_id (insert raw to bypass
        // the new enqueue dedup, simulating pre-fix state).
        let queued_id = insert_raw_pending(&catalog, Some(200));
        // Independent queued job — must be left alone.
        let other = catalog
            .enqueue(
                "https://civitai.com/models/999?modelVersionId=999",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();

        let cancelled = catalog.cancel_redundant_pending_jobs().unwrap();
        assert_eq!(cancelled, 1);
        assert_eq!(
            catalog.get_job(queued_id).unwrap().unwrap().status,
            JobStatus::Cancelled
        );
        assert_eq!(
            catalog.get_job(other.id).unwrap().unwrap().status,
            JobStatus::Queued
        );
    }

    fn insert_raw_pending(catalog: &Catalog, version_id: Option<u64>) -> Uuid {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        catalog.conn.execute(
            "INSERT INTO jobs (id, url, model_id, version_id, model_type, dest_path, status, created_at, updated_at, download_reason) \
             VALUES (?1, 'https://example.test/raw', NULL, ?2, NULL, NULL, 'queued', ?3, ?3, 'cli_add')",
            params![id.to_string(), version_id, now],
        ).unwrap();
        id
    }

    #[test]
    fn test_requeue_one_preserves_uuid_and_clears_path() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue(
                "https://civitai.com/models/1",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        let original_id = job.id;
        catalog
            .set_dest_path(job.id, std::path::Path::new("/tmp/model.safetensors"))
            .unwrap();
        catalog.set_status(job.id, JobStatus::Done, None).unwrap();

        let requeued = catalog.requeue_one(original_id).unwrap();

        // UUID must be preserved (in-place update, not a new INSERT).
        assert_eq!(requeued.id, original_id, "UUID must be preserved");
        // Status must flip to Queued.
        assert_eq!(requeued.status, JobStatus::Queued);
        // dest_path must be cleared.
        assert!(requeued.dest_path.is_none());

        // list_done_models must be empty after requeue.
        let done = catalog.list_done_models().unwrap();
        assert!(done.is_empty(), "Done list must be empty after requeue_one");

        // list_queued must have exactly one entry with the original id.
        let queued = catalog.list_queued().unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].id, original_id);
    }

    #[test]
    fn test_requeue_interrupted_resets_stranded_jobs() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let downloading = catalog
            .enqueue(
                "https://civitai.com/models/1",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        let verifying = catalog
            .enqueue(
                "https://civitai.com/models/2",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        let queued = catalog
            .enqueue(
                "https://civitai.com/models/3",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        catalog
            .set_status(downloading.id, JobStatus::Downloading, None)
            .unwrap();
        catalog
            .set_status(verifying.id, JobStatus::Verifying, Some("stale"))
            .unwrap();

        let n = catalog.requeue_interrupted().unwrap();
        assert_eq!(n, 2, "only downloading/verifying rows are re-queued");

        assert_eq!(
            catalog.get_job(downloading.id).unwrap().unwrap().status,
            JobStatus::Queued
        );
        let recovered = catalog.get_job(verifying.id).unwrap().unwrap();
        assert_eq!(recovered.status, JobStatus::Queued);
        assert!(recovered.error.is_none(), "stale error must be cleared");
        // An already-queued job is untouched (and not double-counted).
        assert_eq!(
            catalog.get_job(queued.id).unwrap().unwrap().status,
            JobStatus::Queued
        );
    }

    #[test]
    fn test_find_done_job_by_sha256_matches_existing_file_on_disk() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue(
                "https://huggingface.co/org/repo/resolve/main/a.safetensors",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        let tmp = std::env::temp_dir().join(format!("sha-dedup-{}.bin", job.id));
        std::fs::write(&tmp, b"content").unwrap();
        catalog.set_dest_path(job.id, &tmp).unwrap();
        catalog.set_sha256(job.id, "ABCDEF").unwrap();
        catalog.set_status(job.id, JobStatus::Done, None).unwrap();

        // Case-insensitive match, and the querying job excludes itself.
        let other = Uuid::new_v4();
        let found = catalog.find_done_job_by_sha256("abcdef", other).unwrap();
        assert!(found.is_some(), "identical hash must match");
        assert_eq!(found.unwrap().id, job.id);

        // A job never dedups against itself.
        let self_match = catalog.find_done_job_by_sha256("abcdef", job.id).unwrap();
        assert!(self_match.is_none());

        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_find_done_job_by_sha256_skips_missing_file() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue(
                "https://civitai.com/models/1?modelVersionId=2",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        // Point at a path that does not exist: a stale done row must not match.
        catalog
            .set_dest_path(
                job.id,
                std::path::Path::new("/nonexistent/model.safetensors"),
            )
            .unwrap();
        catalog.set_sha256(job.id, "deadbeef").unwrap();
        catalog.set_status(job.id, JobStatus::Done, None).unwrap();

        let found = catalog
            .find_done_job_by_sha256("deadbeef", Uuid::new_v4())
            .unwrap();
        assert!(
            found.is_none(),
            "stale done row (file gone) must be skipped"
        );
    }

    #[test]
    fn test_find_done_job_by_sha256_ignores_non_done_status() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue(
                "https://civitai.com/models/3",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        let tmp = std::env::temp_dir().join(format!("sha-nondone-{}.bin", job.id));
        std::fs::write(&tmp, b"x").unwrap();
        catalog.set_dest_path(job.id, &tmp).unwrap();
        catalog.set_sha256(job.id, "cafe").unwrap();
        // Still downloading, not done -> must not be a dedup target.
        catalog
            .set_status(job.id, JobStatus::Downloading, None)
            .unwrap();

        let found = catalog
            .find_done_job_by_sha256("cafe", Uuid::new_v4())
            .unwrap();
        assert!(found.is_none());

        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_set_sha256_stores_lowercase() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue(
                "https://civitai.com/models/9",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        catalog.set_sha256(job.id, "AABBCCDD").unwrap();
        let after = catalog.get_job(job.id).unwrap().unwrap();
        assert_eq!(after.sha256.as_deref(), Some("aabbccdd"));
    }

    #[test]
    fn test_requeue_one_errors_on_wrong_state() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        let job = catalog
            .enqueue(
                "https://civitai.com/models/2",
                None,
                DownloadReason::CliAdd,
                None,
            )
            .unwrap();
        // Advance to Failed without going through Done.
        catalog
            .set_status(job.id, JobStatus::Failed, Some("network error"))
            .unwrap();

        let result = catalog.requeue_one(job.id);
        assert!(result.is_err(), "requeue_one must fail for non-Done jobs");

        // Row must remain Failed.
        let after = catalog.get_job(job.id).unwrap().unwrap();
        assert_eq!(after.status, JobStatus::Failed);
    }

    #[test]
    fn test_should_check_update_first_time() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        create_done_model(&catalog, 100, 200);

        assert!(catalog.should_check_update(100).unwrap());
    }

    #[test]
    fn test_should_check_update_within_24h() {
        let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
        create_done_model(&catalog, 100, 200);

        catalog.set_last_update_check(100).unwrap();

        assert!(!catalog.should_check_update(100).unwrap());
    }
}
