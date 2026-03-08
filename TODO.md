# TODO

## Core completions

Scaffolded code that needs to be finished.

- [x] **[high]** Parse `model_id` / `version_id` from CivitAI URL on enqueue
  (`src/catalog/mod.rs` — populate currently-null columns)
- [x] **[high]** Verify downloaded file SHA-256 against `ModelFile.hashes.sha256`
  (`src/daemon/downloader.rs` — hash is computed but discarded)
- [x] **[high]** Implement update checker: query `CivitaiClient::get_model`, compare
  `model_versions[0].id` with stored `version_id`, enqueue newer version and call
  `notify_update_available` (`src/daemon/updater.rs`)
- [ ] **[high]** Honour `max_concurrent_downloads` config: replace sequential queue loop
  with a semaphore-bounded pool (`src/daemon/queue.rs`, `tokio::sync::Semaphore`)
- [ ] **[medium]** Persist `dest_path` to catalog after successful download
  (`src/daemon/queue.rs` → `src/daemon/downloader.rs` return value)
- [ ] **[medium]** Implement `GetStatus` response with real data: queue length, active
  download progress, free disk space (`src/daemon/mod.rs`)
- [ ] **[medium]** Implement download cancellation: signal the in-flight downloader task
  to abort when `Cancel { id }` is received (currently only sets the DB flag)
- [x] **[low]** Wire `CheckUpdates` IPC command to immediately wake the updater task
  instead of waiting for the next poll interval

## Feature work

New subsystems within original scope, not yet started.

- [ ] **[high]** Resume partial downloads: check for an existing `.tmp` file and send an
  HTTP `Range` header (`src/daemon/downloader.rs`)
- [ ] **[medium]** `comfyui-dl list` command: print the queue table to stdout (IPC side
  already backed by `ListQueue`)
- [ ] **[medium]** Progress reporting: stream bytes-received / total back through the IPC
  `GetStatus` response so the CLI can show a progress bar

## Planned features

Items listed in the README as future work.

- [ ] ZFS snapshot integration: take a snapshot before and after bulk downloads
- [ ] ComfyUI execution status as desktop notifications
- [ ] Manage ComfyUI as a SystemD sub-daemon
- [ ] Execute saved ComfyUI workflow templates with parameter patching
