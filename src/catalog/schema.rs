pub const MIGRATIONS: &str = "
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS jobs (
    id               TEXT PRIMARY KEY,
    url              TEXT NOT NULL,
    model_id         INTEGER,
    version_id       INTEGER,
    model_type       TEXT,
    dest_path        TEXT,
    status           TEXT NOT NULL DEFAULT 'queued',
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    error            TEXT,
    download_reason  TEXT NOT NULL DEFAULT 'unknown'
);

CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);
";
