# Core Completions Design

**Date:** 2026-03-08
**Scope:** All 8 "Core completions" items from TODO.md

## Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Execution strategy | Two parallel git worktrees | Independent tracks, no shared state conflicts |
| Commit granularity | One commit per TODO item | Easy bisect and revert |
| Cancellation signal | `tokio_util::CancellationToken` | Cooperative, already in deps, graceful cleanup |
| Partial file on cancel | Delete `.tmp` | Simple; resume support is a separate TODO item |
| Update found action | Auto-enqueue new version + notify | Consistent with daemon's autonomous nature |

---

## Track A — URL → Metadata → Verification → Updates

### Item 1: Parse model_id / version_id from URL

Add `parse_civitai_url(url: &str) -> (Option<u64>, Option<u64>)` in `src/catalog/mod.rs`.

Supported URL patterns:
- `https://civitai.com/models/{model_id}` → `(Some(model_id), None)`
- `https://civitai.com/models/{model_id}?modelVersionId={version_id}` → `(Some(model_id), Some(version_id))`
- `https://civitai.com/api/download/models/{version_id}` → `(None, Some(version_id))`

`Catalog::enqueue` calls the parser and stores both IDs in the INSERT. If only `version_id` is known, the update checker resolves `model_id` via API on first run.

### Item 2: SHA-256 verification

After streaming completes, if `job.version_id` is set:
1. Call `civitai.get_model_version(version_id)` to fetch `ModelVersion`
2. Find the primary file (`files.iter().find(|f| f.primary == Some(true))`)
3. Compare `file.hashes.sha256` against the computed digest
4. Mismatch → delete `.tmp`, return `Err(...)` → job marked `Failed`
5. `sha256` absent → log warning, proceed (CivitAI sometimes omits it)

`FileHashes.sha256` is already `Option<String>` in `types.rs` — no type changes needed.

### Item 3: Update checker

`updater::check_updates` is implemented:
1. List all `Done` jobs with both `model_id` and `version_id` set
2. Deduplicate by `model_id` (only check each model once per run)
3. For each: call `civitai.get_model(model_id)`, read `model_versions[0].id`
4. If latest ID ≠ stored `version_id`:
   - Build download URL: `https://civitai.com/api/download/models/{latest_id}`
   - Call `catalog.enqueue(url, model_type)` using the same `model_type` as the existing job
   - Call `notifier::notify_update_available(model_name, latest_version_name)`

### Item 8: Wire CheckUpdates to wake updater immediately

Refactor `updater::run` signature to accept `Arc<tokio::sync::Notify>`:

```rust
pub async fn run(config, catalog, civitai, wake: Arc<Notify>)
```

The loop becomes:
```rust
tokio::select! {
    _ = sleep(interval) => {}
    _ = wake.notified() => {}
}
```

The IPC handler for `CheckUpdates` calls `wake.notify_one()`. The daemon creates the `Arc<Notify>` and passes it to both.

---

## Track B — Concurrency → dest_path → Status → Cancellation

### Item 4: Concurrent downloads with Semaphore

Replace the sequential loop in `queue::run` with a task-spawning loop:

```rust
let sem = Arc::new(Semaphore::new(config.daemon.max_concurrent_downloads as usize));
let active: Arc<Mutex<HashMap<Uuid, CancellationToken>>> = Arc::new(Mutex::new(HashMap::new()));
```

Loop:
1. Acquire `sem.clone().acquire_owned().await` (yields until a slot is free)
2. Pick next `Queued` job from catalog
3. Insert a fresh `CancellationToken` for the job ID into `active`
4. `tokio::spawn` the download task (holds the permit — drops on completion)
5. On task completion: remove token from `active`, update job status

The `active` map is passed to the IPC handler for use by cancellation.

### Item 5: Persist dest_path

Add `Catalog::set_dest_path(id: Uuid, path: &Path) -> Result<()>`:
```sql
UPDATE jobs SET dest_path = ?1, updated_at = ?2 WHERE id = ?3
```

In `queue.rs`, after `downloader::download` returns `Ok(dest)`, call `catalog.set_dest_path(job.id, &dest)` before marking the job `Done`.

### Item 6: GetStatus with real data

Add a shared `Arc<Mutex<HashMap<Uuid, DownloadProgress>>>` where:
```rust
pub struct DownloadProgress {
    pub bytes_received: u64,
    pub total_bytes: Option<u64>,   // from Content-Length header
}
```

- The downloader updates this map each chunk
- The map is passed to the IPC handler
- `GetStatus` response includes:
  - `queued_count`: `catalog.count_by_status(JobStatus::Queued)`
  - `active`: vec of `(job_id, progress)` from the map
  - `free_bytes`: result of `nix_statvfs` on the models dir

Add `Catalog::count_by_status(status: JobStatus) -> Result<u64>`.

### Item 7: Cancellation with CancellationToken

`downloader::download` gains a `token: CancellationToken` parameter. The chunk loop becomes:

```rust
loop {
    tokio::select! {
        chunk = stream.next() => match chunk {
            Some(Ok(b)) => { /* write + hash */ }
            Some(Err(e)) => return Err(e.into()),
            None => break,
        },
        _ = token.cancelled() => {
            drop(file);
            let _ = fs::remove_file(&tmp).await;
            bail!("download cancelled");
        }
    }
}
```

The IPC handler for `Cancel { id }` looks up the token in the `active` map and calls `token.cancel()`. The job is set to `Cancelled` when the task exits.

---

## Shared state summary

| State | Type | Owners |
|---|---|---|
| Active task tokens | `Arc<Mutex<HashMap<Uuid, CancellationToken>>>` | queue loop + IPC handler |
| Download progress | `Arc<Mutex<HashMap<Uuid, DownloadProgress>>>` | downloader + IPC handler |
| Updater wake signal | `Arc<tokio::sync::Notify>` | updater loop + IPC handler |
