# TUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement `comfyui-dl tui` per `docs/plans/2026-05-06-tui-design.md` — a three-tab terminal UI for managing the queue, browsing the catalogue, and applying updates, fed by a daemon-pushed snapshot stream.

**Architecture:** A long-lived `Subscribe` connection from the TUI to the daemon receives `Frame::Snapshot` pushes whenever the daemon's broadcast `EventBus` fires (progress tick, queue change, catalog change, updates change). User actions (cancel, add, delete, etc.) use ephemeral connections matching the existing CLI pattern. The TUI is a pure-reducer `App` driven by an `mpsc<AppEvent>`; rendering uses ratatui+crossterm.

**Tech Stack:** Rust 2024, tokio, ratatui, crossterm, tokio-stream, existing `IpcClient`/`IpcServer`, `rusqlite`, `tokio::sync::broadcast`.

---

## File Structure

**Created:**
- `src/daemon/events.rs` — `EventBus`, `Event` enum
- `src/tui/mod.rs` — `pub async fn run(config: Config) -> Result<()>`
- `src/tui/app.rs` — `App`, `AppEvent`, `AppAction`, reducer
- `src/tui/ipc.rs` — snapshot stream task + action dispatcher
- `src/tui/input.rs` — crossterm event → `AppEvent::Key`
- `src/tui/ui/mod.rs` — top-level draw + tabs + status bar
- `src/tui/ui/queue.rs` — Queue pane render + helpers
- `src/tui/ui/catalog.rs` — Catalog pane render + search filter
- `src/tui/ui/updates.rs` — Updates pane render
- `src/tui/ui/modal.rs` — Confirm/Error/Help/AddDownload modals
- `src/tui/format.rs` — path trimming, byte/duration formatting (extracted from `cli/mod.rs`)
- `tests/ipc_subscribe.rs` — integration test for `Subscribe` flow

**Modified:**
- `Cargo.toml` — add `ratatui`, `crossterm`, `tokio-stream` deps
- `src/lib.rs` — `pub mod tui;`
- `src/cli/mod.rs` — add `Tui` subcommand
- `src/ipc/protocol.rs` — add `Request::Subscribe`, `Frame`, `ActiveJob`, `QueuedJob`; add `version_name` to `EnrichedModel`
- `src/ipc/server.rs` — split request handling so `Subscribe` keeps the connection open
- `src/ipc/client.rs` — add `IpcSubscriber` (streams `Frame` lines)
- `src/ipc/mod.rs` — re-export `Frame`, `ActiveJob`, `QueuedJob`, `IpcSubscriber`
- `src/daemon/mod.rs` — construct `EventBus`; pass it everywhere; add streaming `Subscribe` handler; refactor `GetStatus` to share snapshot builder
- `src/daemon/queue.rs` — emit `QueueChanged` / `ProgressTick`; ProgressTick interval task
- `src/daemon/downloader.rs` — write `version_name` into the metadata sidecar
- `src/daemon/updater.rs` — emit `UpdatesChanged`
- `src/catalog/mod.rs` — methods that mutate state take `&EventBus` (or return success and the caller emits — see Task 2)

---

## Tasks

### Task 1: Add `version_name` to enriched metadata

**Goal:** Surface the version name (e.g. `"better_hands"`) so the TUI can render `<model> — <version>` rows.

**Files:**
- Modify: `src/daemon/downloader.rs` — write `version_name` to the sidecar
- Modify: `src/ipc/protocol.rs` — add `version_name` to `EnrichedModel`
- Modify: `src/daemon/mod.rs:enrich_models` — read `version_name` from sidecar
- Test: `src/daemon/mod.rs` (unit, in `#[cfg(test)] mod tests`)

- [ ] **Step 1: Find the sidecar write site**

Run: `rg -n "metadata.json|preview_url|preview_nsfw_level" src/daemon/downloader.rs`
Expected: a function that builds a `serde_json::Map`/`serde_json::json!(...)` with keys like `"model_name"`, `"base_model"`, etc. Note the line number.

- [ ] **Step 2: Add `version_name` to the sidecar JSON**

In the sidecar-write function, add the new key. The version name comes from the `ModelVersion` struct fetched in the same scope (look for a `let mv = ...` or similar binding — the metadata write happens after the version is resolved).

```rust
// inside the sidecar JSON construction
"version_name": mv.name,                  // ← add this
"version_id": mv.id,                      // (existing)
```

(If the binding isn't named `mv`, adapt to whatever the local variable is — but it must be the resolved `ModelVersion`.)

- [ ] **Step 3: Add the field to `EnrichedModel`**

Edit `src/ipc/protocol.rs`:

```rust
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
    pub version_name: Option<String>,        // ← add
    pub base_model: Option<String>,
    pub preview_path: Option<String>,
    pub preview_nsfw_level: Option<u32>,
    pub file_size: Option<u64>,
    pub sha256: Option<String>,
}
```

- [ ] **Step 4: Read it in `enrich_models`**

Edit `src/daemon/mod.rs` — inside the `match metadata { Some(meta) => (...)` tuple, add a parallel field for `version_name`:

```rust
let (model_name, version_name, base_model, preview_path, preview_nsfw_level, file_size, sha256) =
    match metadata {
        Some(meta) => (
            meta.get("model_name").and_then(|v| v.as_str()).map(String::from),
            meta.get("version_name").and_then(|v| v.as_str()).map(String::from),
            meta.get("base_model").and_then(|v| v.as_str()).map(String::from),
            meta.get("preview_url").and_then(|v| v.as_str()).map(String::from),
            meta.get("preview_nsfw_level").and_then(|v| v.as_u64()).map(|n| n as u32),
            meta.get("size").and_then(|v| v.as_u64()),
            meta.get("sha256").and_then(|v| v.as_str()).map(String::from),
        ),
        None => (None, None, None, None, None, None, None),
    };
enriched.push(EnrichedModel {
    id: job.id,
    url: job.url,
    model_id: job.model_id,
    version_id: job.version_id,
    model_type: job.model_type,
    dest_path: job.dest_path,
    created_at: job.created_at,
    updated_at: job.updated_at,
    model_name,
    version_name,
    base_model,
    preview_path,
    preview_nsfw_level,
    file_size,
    sha256,
});
```

- [ ] **Step 5: Verify compile**

Run: `cargo check`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src/ipc/protocol.rs src/daemon/mod.rs src/daemon/downloader.rs
git commit -m "feat(catalog): surface version_name in enriched models"
```

---

### Task 2: Introduce `EventBus` and emit events from existing mutation sites

**Goal:** Add a daemon-wide `tokio::sync::broadcast::Sender<Event>` and start firing events from the queue, downloader, updater, and IPC handlers. No subscriber yet — this task only makes the events available.

**Files:**
- Create: `src/daemon/events.rs`
- Modify: `src/daemon/mod.rs` — construct bus, thread it through tasks and request handler
- Modify: `src/daemon/queue.rs` — emit `QueueChanged` on transitions; add `ProgressTick` interval task
- Modify: `src/daemon/downloader.rs` — emit `QueueChanged` on completion (success / cancelled / failed)
- Modify: `src/daemon/updater.rs` — emit `UpdatesChanged` after each poll cycle
- Test: `src/daemon/events.rs` (unit, in `#[cfg(test)] mod tests`)

Design rule: The catalog API stays unchanged in this task. Emission happens in the daemon layer (the call sites of catalog mutators), not inside `Catalog`. Rationale: catalog code has zero IPC awareness today; keeping the event bus out of it preserves that boundary, and every catalog mutation goes through a small set of daemon callers we can audit.

- [ ] **Step 1: Write the `EventBus` test**

Create `src/daemon/events.rs`:

```rust
use tokio::sync::broadcast;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    ProgressTick,
    QueueChanged,
    CatalogChanged,
    UpdatesChanged,
}

pub type EventBus = broadcast::Sender<Event>;

pub fn new_bus() -> EventBus {
    broadcast::channel(256).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscriber_receives_emitted_event() {
        let bus = new_bus();
        let mut rx = bus.subscribe();
        bus.send(Event::QueueChanged).unwrap();
        assert_eq!(rx.recv().await.unwrap(), Event::QueueChanged);
    }

    #[tokio::test]
    async fn send_with_no_subscribers_is_not_an_error_for_callers() {
        // broadcast::send returns Err when there are no receivers; we ignore it.
        let bus = new_bus();
        let _ = bus.send(Event::ProgressTick); // must not panic
    }
}
```

- [ ] **Step 2: Wire `events` into the daemon module tree**

Edit `src/daemon/mod.rs` — at the top:

```rust
pub mod downloader;
pub mod events;
pub mod notifier;
pub mod queue;
pub mod scanner;
pub mod updater;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --quiet daemon::events`
Expected: 2 passed.

- [ ] **Step 4: Construct the bus in `daemon::run` and clone it into every task**

Edit `src/daemon/mod.rs` — inside `pub async fn run()`, after the `update_wake` line:

```rust
    let event_bus: crate::daemon::events::EventBus = crate::daemon::events::new_bus();
```

Then thread `event_bus.clone()` into `scanner::run`, `queue::run`, `updater::run`, and into the `handle_request` closure (alongside `cat`, `act`, `prog`, `wake`). Update each function signature to take `bus: EventBus`.

- [ ] **Step 5: Emit `QueueChanged` from `queue::run`**

Edit `src/daemon/queue.rs`:
- Add `bus: EventBus` parameter
- Inside the spawned per-job task, after each `cat.set_status(...)` call (Done, Cancelled, Failed), call `let _ = bus.send(Event::QueueChanged);`
- Also fire `bus.send(Event::QueueChanged)` immediately after `cat.set_status(job.id, JobStatus::Downloading, None)` (the transition out of `Queued`)

(The leading `let _ =` is intentional: `broadcast::send` errors with `SendError` only when there are zero subscribers, which is fine.)

- [ ] **Step 6: Spawn the `ProgressTick` interval**

Inside `daemon::run`, after the queue handle is spawned, add:

```rust
let tick_handle = {
    let bus = event_bus.clone();
    let prog = progress.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if !prog.lock().await.is_empty() {
                let _ = bus.send(crate::daemon::events::Event::ProgressTick);
            }
        }
    })
};
```

And add `tick_handle.abort();` to the shutdown block.

- [ ] **Step 7: Emit `UpdatesChanged` from `updater::run`**

Edit `src/daemon/updater.rs`:
- Add `bus: EventBus` parameter to `pub async fn run`
- After each completed poll cycle (the place where `info!("Running update check")` block finishes — find it via `rg -n "info!" src/daemon/updater.rs`), call `let _ = bus.send(Event::UpdatesChanged);`

- [ ] **Step 8: Emit `CatalogChanged` from IPC handlers**

Edit `src/daemon/mod.rs` — inside `handle_request`, take `bus: EventBus` as an extra arg, and after each successful catalog mutation in:
- `Request::AddDownload` (after `cat.enqueue`)
- `Request::DeleteModel` (after `cat.delete_model`)
- `Request::Cancel` (after `token.cancel()` AND after the catalog-side `set_status` fallback)
- `Request::RedownloadMissing` (after `cat.requeue_done`)
- `Request::DownloadVersion` (after `cat.enqueue`)

…fire `let _ = bus.send(Event::CatalogChanged);` (or `Event::QueueChanged` for cancels — pick `QueueChanged` for cancels and `CatalogChanged` for all others; `AddDownload`/`DownloadVersion` should fire BOTH because they create a queued job).

- [ ] **Step 9: Verify build still clean**

Run: `cargo clippy --quiet -- -D warnings`
Expected: zero warnings.

Run: `cargo test --quiet`
Expected: all existing 60 tests still pass.

- [ ] **Step 10: Commit**

```bash
git add src/daemon/events.rs src/daemon/mod.rs src/daemon/queue.rs src/daemon/updater.rs src/lib.rs
git commit -m "feat(daemon): add EventBus and emit lifecycle events"
```

---

### Task 3: Add typed snapshot types to the IPC protocol

**Goal:** Replace the ad-hoc `serde_json::json!` blobs in `Request::GetStatus` with typed `ActiveJob`, `QueuedJob`, and a snapshot constructor reusable by the upcoming `Subscribe` handler.

**Files:**
- Modify: `src/ipc/protocol.rs` — add `ActiveJob`, `QueuedJob`, `Snapshot`
- Modify: `src/daemon/mod.rs` — add `build_snapshot()` helper; rewrite `GetStatus` to call it
- Modify: `src/cli/mod.rs:print_status` — switch to typed deserialisation
- Test: `src/daemon/mod.rs` (unit test against the helper)

- [ ] **Step 1: Add the types in `protocol.rs`**

```rust
use chrono::{DateTime, Utc};
// ...

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
```

Note: `version_name` on queued jobs is `None` until metadata is fetched at download start; the protocol allows `None` to flow through.

- [ ] **Step 2: Add `version_name` lookup helper**

The active-job source (`progress: ProgressMap`) and queued-job source (`Catalog::list_queued`) need to surface `version_name`. Add a helper to `src/daemon/mod.rs`:

```rust
async fn lookup_version_name(
    catalog: &Arc<Mutex<Catalog>>,
    job_id: Uuid,
) -> Option<String> {
    let cat = catalog.lock().await;
    let job = cat.get_job(job_id).ok().flatten()?;
    let dest = job.dest_path?;
    drop(cat);
    let meta = read_sidecar_metadata(std::path::Path::new(&dest)).await?;
    meta.get("version_name")
        .and_then(|v| v.as_str())
        .map(String::from)
}
```

For *queued* jobs the file may not exist yet — in that case the helper returns `None`, which is the desired behaviour. For *active* jobs we extend `DownloadProgress` to carry it.

- [ ] **Step 3: Add `version_name` to `DownloadProgress`**

Edit `src/daemon/queue.rs`:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DownloadProgress {
    pub bytes_received: u64,
    pub total_bytes: Option<u64>,
    pub model_name: Option<String>,
    pub version_name: Option<String>,         // ← add
    pub dest_path: Option<String>,
    pub model_type: Option<String>,
    pub download_reason: Option<String>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}
```

Then update the place in `src/daemon/downloader.rs` that constructs `DownloadProgress` (find via `rg -n "DownloadProgress \{" src/daemon/downloader.rs`) — populate `version_name` from the `ModelVersion` already in scope. If construction is via field-init shorthand, add `version_name: mv.name.clone().into()` (use the actual local binding name).

- [ ] **Step 4: Implement `build_snapshot`**

In `src/daemon/mod.rs`:

```rust
async fn build_snapshot(
    catalog: &Arc<Mutex<Catalog>>,
    progress: &ProgressMap,
    catalog_dirty: bool,
    updates_dirty: bool,
    seq: u64,
) -> crate::ipc::protocol::Snapshot {
    use crate::ipc::protocol::{ActiveJob, QueuedJob, Snapshot};

    let queued_jobs = {
        let cat = catalog.lock().await;
        cat.list_queued().unwrap_or_default()
    };
    let active = {
        let prog = progress.lock().await;
        prog.iter()
            .map(|(id, p)| ActiveJob {
                id: *id,
                model_name: p.model_name.clone(),
                version_name: p.version_name.clone(),
                model_type: p.model_type.clone(),
                bytes_received: p.bytes_received,
                total_bytes: p.total_bytes,
                dest_path: p.dest_path.clone(),
                started_at: p.started_at,
                download_reason: p.download_reason.clone(),
            })
            .collect()
    };
    let queued = queued_jobs
        .into_iter()
        .map(|j| QueuedJob {
            id: j.id,
            url: j.url,
            model_name: None,            // queued jobs haven't been enriched yet
            version_name: None,
            model_type: j.model_type,
            download_reason: Some(j.download_reason.to_string()),
        })
        .collect();
    let free_bytes = crate::config::Config::load()
        .ok()
        .map(|c| crate::daemon::downloader::free_disk_bytes(&c.paths.models_dir).unwrap_or(0))
        .unwrap_or(0);

    Snapshot { active, queued, free_bytes, catalog_dirty, updates_dirty, seq }
}
```

- [ ] **Step 5: Rewrite `Request::GetStatus` to call the helper**

In `handle_request`, replace the body of the `Request::GetStatus` arm with:

```rust
Request::GetStatus => {
    let snap = build_snapshot(&catalog, &progress, false, false, 0).await;
    Response::ok(snap)
}
```

The `serde_json::Value` callers in the CLI's `print_status` need to keep working. Because `Snapshot` serialises as `{"active": [...], "queued": [...], "free_bytes": N, ...}`, the existing CLI which already reads `data["active"]`, `data["queued_jobs"]`, etc., needs minor updates: it currently looks up `data["queued_jobs"]` — change to `data["queued"]` (the new field name in `Snapshot`).

- [ ] **Step 6: Update `cli::mod::print_status`**

In `src/cli/mod.rs`, in `print_status`:
- `let queued_jobs = data["queued_jobs"].as_array();` → `let queued_jobs = data["queued"].as_array();`
- `let queued = data["queued"].as_u64().unwrap_or(0);` → `let queued = queued_jobs.map(|a| a.len() as u64).unwrap_or(0);`

Adjust the `if queued > 0` block accordingly. Also: `print_active_job` and `print_queued_job` both reference `model_name`/`url`/`model_type` already — those keys are unchanged in `ActiveJob`/`QueuedJob`. The short-id rendering still works because `id` is still serialized.

- [ ] **Step 7: Build & test**

Run: `cargo clippy --quiet -- -D warnings`
Run: `cargo test --quiet`
Expected: clean, all tests pass.

- [ ] **Step 8: Manual smoke**

```bash
cargo build --release
RUST_LOG=info ./target/release/comfyui-downloader &
DAEMON_PID=$!
./target/release/comfyui-dl status
kill $DAEMON_PID
```

Expected: `status` output renders identically to before (active list, queued list, free disk bytes).

- [ ] **Step 9: Commit**

```bash
git add src/ipc/protocol.rs src/daemon/mod.rs src/daemon/queue.rs src/daemon/downloader.rs src/cli/mod.rs
git commit -m "refactor(ipc): introduce typed Snapshot for status responses"
```

---

### Task 4: Add `Subscribe` request, `Frame` push type, and per-model redownload

**Goal:** Extend the IPC protocol with a streaming-only `Subscribe` request, a `Frame` enum for server-pushed messages, and a `RedownloadModel { id }` variant the catalog pane uses for `R`. The Subscribe handler is built in Task 6; the per-model redownload handler is built in this task because it's a self-contained one-shot.

**Files:**
- Modify: `src/ipc/protocol.rs`
- Modify: `src/ipc/mod.rs` — re-export `Frame`, `ActiveJob`, `QueuedJob`, `Snapshot`
- Modify: `src/catalog/mod.rs` — add `Catalog::requeue_one(id)`
- Modify: `src/daemon/mod.rs` — handle `Request::RedownloadModel`

- [ ] **Step 1: Add `Subscribe` and `RedownloadModel` to `Request`**

```rust
pub enum Request {
    // existing variants...
    Subscribe,
    RedownloadModel { id: Uuid },
}
```

- [ ] **Step 2: Add `Frame`**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Subscribed,
    Snapshot(Snapshot),
    Error { message: String },
}
```

- [ ] **Step 3: Re-export from `ipc/mod.rs`**

Add to `src/ipc/mod.rs`:

```rust
pub use protocol::{ActiveJob, Frame, QueuedJob, Snapshot};
```

(Plus existing exports.)

- [ ] **Step 4: Round-trip serialisation test**

Add to the existing `#[cfg(test)] mod tests` in `src/ipc/protocol.rs` (create the module if it doesn't exist):

```rust
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
```

- [ ] **Step 5: Run the protocol tests**

Run: `cargo test --quiet ipc::protocol`
Expected: 3 passed.

- [ ] **Step 6: Add `Catalog::requeue_one`**

In `src/catalog/mod.rs`, alongside `requeue_done`:

```rust
/// Re-queue a single model by ID, regardless of whether the file is missing.
/// Returns the new queued `DownloadJob`. Errors if the row isn't `Done`.
pub fn requeue_one(&self, id: Uuid) -> Result<DownloadJob> {
    let job = self.get_job(id)?.context("model not found")?;
    if job.status != JobStatus::Done {
        anyhow::bail!("model is not in Done state (status = {})", job.status);
    }
    self.enqueue(&job.url, job.model_type.as_deref(), DownloadReason::CliAdd)
}
```

- [ ] **Step 7: Handle `RedownloadModel` in the daemon**

In `src/daemon/mod.rs:handle_request`, add:

```rust
Request::RedownloadModel { id } => {
    let cat = catalog.lock().await;
    match cat.requeue_one(id) {
        Ok(job) => {
            let _ = bus.send(crate::daemon::events::Event::CatalogChanged);
            let _ = bus.send(crate::daemon::events::Event::QueueChanged);
            Response::ok(job)
        }
        Err(e) => Response::err(e.to_string()),
    }
}
```

(The `bus` parameter was added to `handle_request` in Task 2.)

- [ ] **Step 8: Run tests + clippy**

Run: `cargo test --quiet && cargo clippy --quiet -- -D warnings`
Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add src/ipc/protocol.rs src/ipc/mod.rs src/catalog/mod.rs src/daemon/mod.rs
git commit -m "feat(ipc): add Subscribe, Frame, and per-model redownload"
```

---

### Task 5: Refactor `IpcServer` to support a streaming branch

**Goal:** Make the server keep a connection open after a `Subscribe` request and run a per-connection streaming loop. Other requests are unchanged.

**Files:**
- Modify: `src/ipc/server.rs`
- Modify: `src/daemon/mod.rs` — provide a streaming handler

The cleanest approach: split `serve` into accepting two handlers — one for one-shot requests, one for `Subscribe`. The server reads the first line, dispatches, and either:
- Writes one response and continues looping on the same connection (existing behaviour), or
- Hands the writer to the streaming handler, which keeps the connection open until it returns or the client disconnects.

- [ ] **Step 1: Update `serve` signature to accept a streamer**

Edit `src/ipc/server.rs`:

```rust
use crate::ipc::protocol::{Frame, Request, Response};
use anyhow::Result;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info};

pub struct IpcServer {
    listener: UnixListener,
}

impl IpcServer {
    pub fn bind(path: &Path) -> Result<Self> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        info!("IPC socket bound at {}", path.display());
        Ok(Self { listener })
    }

    /// Accept connections in a loop. `request_handler` handles one-shot
    /// `Request → Response` exchanges. `subscribe_handler` is called when a
    /// connection's first request is `Request::Subscribe`; it owns the
    /// connection's writer and runs until the client disconnects.
    pub async fn serve<F, Fut, S, SFut>(
        &self,
        request_handler: F,
        subscribe_handler: S,
    ) -> Result<()>
    where
        F: Fn(Request) -> Fut + Clone + Send + 'static,
        Fut: std::future::Future<Output = Response> + Send,
        S: Fn(SubscribeWriter) -> SFut + Clone + Send + 'static,
        SFut: std::future::Future<Output = ()> + Send,
    {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let req_handler = request_handler.clone();
            let sub_handler = subscribe_handler.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, req_handler, sub_handler).await {
                    error!("IPC connection error: {e}");
                }
            });
        }
    }
}

/// Owned writer half of a subscribed connection. The subscribe handler uses
/// this to push `Frame` lines until the client disconnects.
pub struct SubscribeWriter {
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl SubscribeWriter {
    pub async fn send(&mut self, frame: &Frame) -> Result<()> {
        let mut line = serde_json::to_string(frame)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        Ok(())
    }
}

async fn handle_connection<F, Fut, S, SFut>(
    stream: UnixStream,
    request_handler: F,
    subscribe_handler: S,
) -> Result<()>
where
    F: Fn(Request) -> Fut,
    Fut: std::future::Future<Output = Response>,
    S: Fn(SubscribeWriter) -> SFut,
    SFut: std::future::Future<Output = ()>,
{
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let req = match serde_json::from_str::<Request>(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::err(format!("bad request: {e}"));
                let mut encoded = serde_json::to_string(&resp)?;
                encoded.push('\n');
                let mut writer = writer;
                writer.write_all(encoded.as_bytes()).await?;
                return Ok(());
            }
        };

        if matches!(req, Request::Subscribe) {
            let sw = SubscribeWriter { writer };
            subscribe_handler(sw).await;
            return Ok(());
        }

        let response = request_handler(req).await;
        let mut encoded = serde_json::to_string(&response)?;
        encoded.push('\n');
        let mut writer = writer;
        writer.write_all(encoded.as_bytes()).await?;
        return Ok(());
    }
    Ok(())
}
```

Note: this changes the semantics from "loop reading multiple requests on one connection" to "one request per connection, then close". The CLI already opens a fresh connection per command (see `IpcClient::connect`), so this is consistent with actual usage. Verify by `rg -n "client.send" src/cli/mod.rs` — there's exactly one `send` per CLI invocation. **This is intentional**: it simplifies the streaming branch and removes a never-exercised loop.

- [ ] **Step 2: Update `daemon::run` to pass two handlers**

In `src/daemon/mod.rs`, change the `server.serve(...)` call to pass both:

```rust
let bus = event_bus.clone();
let cat_h = catalog.clone();
let act_h = active.clone();
let prog_h = progress.clone();
let wake_h = update_wake.clone();
let bus_h = bus.clone();

let cat_s = catalog.clone();
let prog_s = progress.clone();
let bus_s = bus.clone();

server
    .serve(
        move |req| {
            let cat = cat_h.clone();
            let act = act_h.clone();
            let prog = prog_h.clone();
            let wake = wake_h.clone();
            let bus = bus_h.clone();
            async move { handle_request(req, cat, act, prog, wake, bus).await }
        },
        move |writer| {
            let cat = cat_s.clone();
            let prog = prog_s.clone();
            let bus = bus_s.clone();
            async move { run_subscribe(writer, cat, prog, bus).await }
        },
    )
    .await?;
```

- [ ] **Step 3: Stub `run_subscribe`**

Add to `src/daemon/mod.rs`:

```rust
async fn run_subscribe(
    mut writer: crate::ipc::server::SubscribeWriter,
    _catalog: Arc<Mutex<Catalog>>,
    _progress: ProgressMap,
    _bus: crate::daemon::events::EventBus,
) {
    use crate::ipc::protocol::Frame;
    let _ = writer.send(&Frame::Subscribed).await;
    // Real implementation lands in Task 6.
}
```

- [ ] **Step 4: Re-export `SubscribeWriter`**

Add to `src/ipc/mod.rs`:

```rust
pub use server::SubscribeWriter;
```

- [ ] **Step 5: Build clean**

Run: `cargo clippy --quiet -- -D warnings`
Expected: zero warnings. (The `_` prefixes on unused params are intentional and silence clippy.)

- [ ] **Step 6: Commit**

```bash
git add src/ipc/server.rs src/ipc/mod.rs src/daemon/mod.rs
git commit -m "feat(ipc): split serve into request and subscribe handlers"
```

---

### Task 6: Implement the subscribe loop on the daemon side

**Goal:** The streaming handler subscribes to `EventBus`, sends an initial snapshot, then loops on events, coalescing within a 10 ms window, and pushes a fresh snapshot for each batch.

**Files:**
- Modify: `src/daemon/mod.rs` — flesh out `run_subscribe`
- Test: `tests/ipc_subscribe.rs` (new integration test)

- [ ] **Step 1: Implement `run_subscribe`**

Replace the stub from Task 5:

```rust
async fn run_subscribe(
    mut writer: crate::ipc::server::SubscribeWriter,
    catalog: Arc<Mutex<Catalog>>,
    progress: ProgressMap,
    bus: crate::daemon::events::EventBus,
) {
    use crate::daemon::events::Event;
    use crate::ipc::protocol::Frame;

    if writer.send(&Frame::Subscribed).await.is_err() {
        return;
    }

    let mut rx = bus.subscribe();
    let mut seq: u64 = 0;
    let mut catalog_dirty = false;
    let mut updates_dirty = false;

    // Initial snapshot.
    seq += 1;
    let snap = build_snapshot(&catalog, &progress, false, false, seq).await;
    if writer.send(&Frame::Snapshot(snap)).await.is_err() {
        return;
    }

    loop {
        let event = match rx.recv().await {
            Ok(ev) => ev,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // Re-sync: send a fresh full snapshot.
                seq += 1;
                let snap = build_snapshot(&catalog, &progress, true, true, seq).await;
                if writer.send(&Frame::Snapshot(snap)).await.is_err() {
                    return;
                }
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };

        match event {
            Event::CatalogChanged => catalog_dirty = true,
            Event::UpdatesChanged => updates_dirty = true,
            Event::QueueChanged | Event::ProgressTick => {}
        }

        // Coalesce: drain anything else that arrived in the last 10 ms.
        let coalesce = tokio::time::sleep(std::time::Duration::from_millis(10));
        tokio::pin!(coalesce);
        loop {
            tokio::select! {
                biased;
                _ = &mut coalesce => break,
                ev = rx.recv() => match ev {
                    Ok(Event::CatalogChanged) => catalog_dirty = true,
                    Ok(Event::UpdatesChanged) => updates_dirty = true,
                    Ok(_) => {}
                    Err(_) => break,
                },
            }
        }

        seq += 1;
        let snap = build_snapshot(&catalog, &progress, catalog_dirty, updates_dirty, seq).await;
        catalog_dirty = false;
        updates_dirty = false;
        if writer.send(&Frame::Snapshot(snap)).await.is_err() {
            return;
        }
    }
}
```

- [ ] **Step 2: Write the integration test**

Create `tests/ipc_subscribe.rs`:

```rust
//! Integration test: client opens a Subscribe connection, daemon emits events,
//! client observes the corresponding snapshot frames.

use comfyui_downloader::catalog::Catalog;
use comfyui_downloader::daemon::events::{Event, EventBus, new_bus};
use comfyui_downloader::ipc::protocol::{Frame, Request};
use comfyui_downloader::ipc::IpcServer;
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

#[tokio::test]
async fn subscribe_emits_initial_snapshot_then_responds_to_events() {
    let tmp = TempDir::new().unwrap();
    let socket_path = tmp.path().join("test.sock");
    let db_path = tmp.path().join("catalog.db");

    let catalog = Arc::new(Mutex::new(Catalog::open(&db_path).unwrap()));
    let progress = Arc::new(Mutex::new(HashMap::new()));
    let bus: EventBus = new_bus();

    let server = IpcServer::bind(&socket_path).unwrap();

    // Spawn server in background (only the subscribe handler matters here).
    let cat_s = catalog.clone();
    let prog_s = progress.clone();
    let bus_s = bus.clone();
    tokio::spawn(async move {
        let cat_r = cat_s.clone();
        let _ = server
            .serve(
                move |_req| {
                    let _ = cat_r.clone();
                    async move { comfyui_downloader::ipc::protocol::Response::ok(serde_json::Value::Null) }
                },
                move |writer| {
                    let cat = cat_s.clone();
                    let prog = prog_s.clone();
                    let bus = bus_s.clone();
                    async move {
                        comfyui_downloader::daemon::run_subscribe_for_test(writer, cat, prog, bus).await
                    }
                },
            )
            .await;
    });

    // Give the server a beat to bind.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Client side: open a connection, send Subscribe, read frames.
    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let mut req = serde_json::to_string(&Request::Subscribe).unwrap();
    req.push('\n');
    writer.write_all(req.as_bytes()).await.unwrap();

    // Frame 1: Subscribed
    let line = lines.next_line().await.unwrap().unwrap();
    let frame: Frame = serde_json::from_str(&line).unwrap();
    assert!(matches!(frame, Frame::Subscribed));

    // Frame 2: initial Snapshot
    let line = lines.next_line().await.unwrap().unwrap();
    let frame: Frame = serde_json::from_str(&line).unwrap();
    let snap1 = match frame {
        Frame::Snapshot(s) => s,
        _ => panic!("expected snapshot"),
    };
    assert_eq!(snap1.seq, 1);
    assert!(!snap1.catalog_dirty);
    assert!(!snap1.updates_dirty);

    // Fire an event → expect a fresh snapshot with catalog_dirty.
    bus.send(Event::CatalogChanged).unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let frame: Frame = serde_json::from_str(&line).unwrap();
    let snap2 = match frame {
        Frame::Snapshot(s) => s,
        _ => panic!("expected snapshot"),
    };
    assert_eq!(snap2.seq, 2);
    assert!(snap2.catalog_dirty);

    // Subsequent unrelated event → catalog_dirty should reset to false.
    bus.send(Event::QueueChanged).unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let frame: Frame = serde_json::from_str(&line).unwrap();
    let snap3 = match frame {
        Frame::Snapshot(s) => s,
        _ => panic!("expected snapshot"),
    };
    assert_eq!(snap3.seq, 3);
    assert!(!snap3.catalog_dirty);
}
```

- [ ] **Step 3: Expose `run_subscribe_for_test`**

The test calls `comfyui_downloader::daemon::run_subscribe_for_test`, but `run_subscribe` is private. Add a `pub` re-export under `#[cfg(any(test, feature = "test-helpers"))]` — simplest is to make `run_subscribe` `pub(crate)` and add a public re-export in `src/lib.rs`:

```rust
// In src/daemon/mod.rs:
pub(crate) async fn run_subscribe(/* ... */) { /* ... */ }

// In src/lib.rs (or src/daemon/mod.rs):
#[doc(hidden)]
pub use crate::daemon::run_subscribe as run_subscribe_for_test;
```

(`#[doc(hidden)]` keeps it out of the public API surface in rustdoc.)

- [ ] **Step 4: Run the test**

Run: `cargo test --test ipc_subscribe --quiet`
Expected: 1 passed.

- [ ] **Step 5: Run full test suite + clippy**

Run: `cargo test --quiet && cargo clippy --quiet -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src/daemon/mod.rs src/lib.rs tests/ipc_subscribe.rs
git commit -m "feat(daemon): implement Subscribe streaming with snapshot coalescing"
```

---

### Task 7: Add `IpcSubscriber` on the client side

**Goal:** A client API that opens a `Subscribe` connection and yields `Frame`s as a stream.

**Files:**
- Modify: `src/ipc/client.rs`
- Modify: `src/ipc/mod.rs` — re-export `IpcSubscriber`

- [ ] **Step 1: Implement `IpcSubscriber`**

Append to `src/ipc/client.rs`:

```rust
use crate::ipc::protocol::Frame;
use tokio::io::Lines;

pub struct IpcSubscriber {
    lines: Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
}

impl IpcSubscriber {
    pub async fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("connecting to daemon socket {}", path.display()))?;
        let (reader, mut writer) = stream.into_split();

        let mut req = serde_json::to_string(&Request::Subscribe)?;
        req.push('\n');
        writer.write_all(req.as_bytes()).await?;
        // We don't need the writer half after subscribing; drop it so the
        // server sees no further requests.
        drop(writer);

        Ok(Self {
            lines: BufReader::new(reader).lines(),
        })
    }

    /// Read the next frame. Returns `Ok(None)` on clean EOF.
    pub async fn next_frame(&mut self) -> Result<Option<Frame>> {
        match self.lines.next_line().await? {
            Some(line) => Ok(Some(serde_json::from_str::<Frame>(&line)?)),
            None => Ok(None),
        }
    }
}
```

- [ ] **Step 2: Re-export**

Add to `src/ipc/mod.rs`:

```rust
pub use client::{IpcClient, IpcSubscriber};
```

- [ ] **Step 3: Smoke test**

Add to the bottom of `src/ipc/client.rs`, in `#[cfg(test)] mod tests`:

```rust
#[cfg(test)]
mod tests {
    // No unit test for IpcSubscriber here — the integration test in
    // tests/ipc_subscribe.rs already exercises the wire protocol end-to-end.
    // Replace it with a real test if a regression slips through.
}
```

(One short comment is enough — there's a real integration test next door.)

- [ ] **Step 4: Build clean**

Run: `cargo clippy --quiet -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add src/ipc/client.rs src/ipc/mod.rs
git commit -m "feat(ipc): add IpcSubscriber client streaming type"
```

---

### Task 8: Add deps and scaffold `src/tui/`

**Goal:** Create the empty module tree and a `Tui` subcommand stub that exits with a placeholder message. No real TUI yet — this just establishes the surface.

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`
- Modify: `src/cli/mod.rs`
- Create: `src/tui/mod.rs`
- Create: `src/tui/format.rs`

- [ ] **Step 1: Add deps**

Edit `Cargo.toml`, in `[dependencies]`:

```toml
ratatui = "0.29"
crossterm = { version = "0.28", features = ["event-stream"] }
tokio-stream = "0.1"
```

- [ ] **Step 2: Run `cargo fetch` to pull**

Run: `cargo fetch`
Expected: Compiling… (just verifies resolution).

- [ ] **Step 3: Create `src/tui/mod.rs`**

```rust
pub mod format;

use anyhow::Result;
use crate::config::Config;

pub async fn run(_config: Config) -> Result<()> {
    eprintln!("TUI not yet implemented (Task 8 stub)");
    Ok(())
}
```

- [ ] **Step 4: Create `src/tui/format.rs`**

Move byte/duration/path-trim helpers here (extracted from `cli/mod.rs`):

```rust
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

pub fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;
    if hours > 0 {
        format!("{hours}h {mins:02}m {s:02}s")
    } else if mins > 0 {
        format!("{mins}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Trim a model path to its last two components (e.g. "SDXL/foo.safetensors").
pub fn trim_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_path_keeps_last_two_components() {
        assert_eq!(
            trim_path("/home/u/comfyui/models/loras/SDXL/foo.safetensors"),
            "SDXL/foo.safetensors",
        );
    }

    #[test]
    fn trim_path_short_path_returned_unchanged() {
        assert_eq!(trim_path("foo.safetensors"), "foo.safetensors");
        assert_eq!(trim_path("dir/foo.safetensors"), "dir/foo.safetensors");
    }

    #[test]
    fn trim_path_handles_trailing_slash() {
        assert_eq!(trim_path("a/b/c/"), "c/");
    }
}
```

(Leave `format_bytes` / `format_duration` ALSO in `cli/mod.rs` for now — duplicating ~30 lines is cheaper than refactoring the CLI to depend on the TUI module right now. A later task can DRY this up if it bothers anyone.)

- [ ] **Step 4b: Wire `tui` into the lib**

Edit `src/lib.rs` — add `pub mod tui;`

- [ ] **Step 5: Add `Tui` subcommand**

Edit `src/cli/mod.rs` — add to the `Command` enum:

```rust
    /// Open the interactive terminal UI.
    Tui,
```

In the match where commands become requests, **before** the `let mut client = IpcClient::connect(...)` line, intercept `Tui` like `SetKey` does:

```rust
    if let Some(Command::Tui) = cli.command {
        return crate::tui::run(Config::load()?).await;
    }
```

(Place this check just before `let config = Config::load()?;` to avoid the unnecessary daemon connect for the TUI's own startup.)

- [ ] **Step 6: Build & run smoke**

Run: `cargo build --quiet`
Run: `./target/debug/comfyui-dl tui`
Expected: prints `TUI not yet implemented (Task 8 stub)` and exits.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/cli/mod.rs src/tui/mod.rs src/tui/format.rs
git commit -m "feat(tui): scaffold Tui subcommand and tui module"
```

---

### Task 9: Implement `App` state and the pure reducer

**Goal:** All state transitions live in `App::handle(event) -> Option<AppAction>`. Fully unit-testable, no I/O.

**Files:**
- Create: `src/tui/app.rs`
- Modify: `src/tui/mod.rs` — `pub mod app;`

- [ ] **Step 1: Define types**

Create `src/tui/app.rs`:

```rust
use crate::ipc::protocol::{ActiveJob, EnrichedModel, QueuedJob, Snapshot};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Queue,
    Catalog,
    Updates,
}

impl Tab {
    pub fn next(self) -> Self {
        match self {
            Self::Queue => Self::Catalog,
            Self::Catalog => Self::Updates,
            Self::Updates => Self::Queue,
        }
    }
    pub fn prev(self) -> Self {
        match self {
            Self::Queue => Self::Updates,
            Self::Catalog => Self::Queue,
            Self::Updates => Self::Catalog,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct QueuePane {
    pub active: Vec<ActiveJob>,
    pub queued: Vec<QueuedJob>,
    pub cursor: usize,
}

#[derive(Clone, Debug, Default)]
pub struct CatalogPane {
    pub models: Vec<EnrichedModel>,
    pub filter: String,
    pub cursor: usize,
    pub search_focused: bool,
    pub loaded: bool,
}

#[derive(Clone, Debug, Default)]
pub struct UpdatesPane {
    pub items: Vec<UpdateInfo>,
    pub cursor: usize,
    pub loaded: bool,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct UpdateInfo {
    pub model_id: u64,
    pub version_id: u64,
    pub available_version_id: u64,
    pub available_version_name: String,
    pub model_type: Option<String>,
    pub dest_path: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Modal {
    Confirm { prompt: String, on_yes: AppAction },
    AddDownload { url: String, model_type: String, focus: AddDownloadField },
    Help,
    Error { message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddDownloadField { Url, ModelType, Submit }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppAction {
    Cancel(Uuid),
    Delete(Uuid),
    RedownloadModel(Uuid),
    AddDownload { url: String, model_type: Option<String> },
    DownloadVersion { model_id: u64, version_id: u64 },
    CheckUpdates,
    RefetchCatalog,
    RefetchUpdates,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnState {
    Connected,
    Reconnecting { attempt: u32 },
}

impl Default for ConnState {
    fn default() -> Self { Self::Reconnecting { attempt: 0 } }
}

#[derive(Clone, Debug)]
pub enum AppEvent {
    Snapshot(Snapshot),
    StreamReconnecting { attempt: u32 },
    Key(KeyEvent),
    Tick,
    CatalogLoaded(Vec<EnrichedModel>),
    UpdatesLoaded(Vec<UpdateInfo>),
    ActionResult { action: AppAction, outcome: Result<(), String> },
}

#[derive(Clone, Debug, Default)]
pub struct App {
    pub tab: Tab,
    pub queue: QueuePane,
    pub catalog: CatalogPane,
    pub updates: UpdatesPane,
    pub modal: Option<Modal>,
    pub connection: ConnState,
    pub last_seq: Option<u64>,
    pub free_bytes: u64,
    pub should_quit: bool,
}

impl Default for Tab {
    fn default() -> Self { Self::Queue }
}
```

- [ ] **Step 2: Implement `App::handle`**

Append to `src/tui/app.rs`:

```rust
impl App {
    /// Apply an event to the App state. Returns an action the caller should
    /// dispatch (over a fresh IpcClient connection), if any.
    pub fn handle(&mut self, event: AppEvent) -> Option<AppAction> {
        match event {
            AppEvent::Snapshot(snap) => self.apply_snapshot(snap),
            AppEvent::StreamReconnecting { attempt } => {
                self.connection = ConnState::Reconnecting { attempt };
                None
            }
            AppEvent::Tick => None,
            AppEvent::CatalogLoaded(models) => {
                self.catalog.models = models;
                self.catalog.cursor = self.catalog.cursor.min(self.catalog.models.len().saturating_sub(1));
                self.catalog.loaded = true;
                None
            }
            AppEvent::UpdatesLoaded(items) => {
                self.updates.items = items;
                self.updates.cursor = self.updates.cursor.min(self.updates.items.len().saturating_sub(1));
                self.updates.loaded = true;
                None
            }
            AppEvent::ActionResult { outcome: Err(message), .. } => {
                self.modal = Some(Modal::Error { message });
                None
            }
            AppEvent::ActionResult { outcome: Ok(()), .. } => None,
            AppEvent::Key(k) => self.handle_key(k),
        }
    }

    fn apply_snapshot(&mut self, snap: Snapshot) -> Option<AppAction> {
        self.connection = ConnState::Connected;
        self.queue.active = snap.active;
        self.queue.queued = snap.queued;
        self.queue.cursor = self.queue.cursor.min(self.queue_len().saturating_sub(1));
        self.free_bytes = snap.free_bytes;
        self.last_seq = Some(snap.seq);

        if snap.catalog_dirty {
            return Some(AppAction::RefetchCatalog);
        }
        if snap.updates_dirty {
            return Some(AppAction::RefetchUpdates);
        }
        None
    }

    fn queue_len(&self) -> usize {
        self.queue.active.len() + self.queue.queued.len()
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        // Modal interception
        if let Some(modal) = self.modal.clone() {
            return self.handle_key_modal(modal, key);
        }

        match key.code {
            KeyCode::Char('Q') => { self.should_quit = true; None }
            KeyCode::Char('q') | KeyCode::Esc => { self.should_quit = true; None }
            KeyCode::Char('?') => { self.modal = Some(Modal::Help); None }
            KeyCode::Char('1') => { self.tab = Tab::Queue; None }
            KeyCode::Char('2') => self.switch_to(Tab::Catalog),
            KeyCode::Char('3') => self.switch_to(Tab::Updates),
            KeyCode::Tab => self.switch_to(self.tab.next()),
            KeyCode::BackTab => self.switch_to(self.tab.prev()),
            KeyCode::Char('r') => self.refresh_current_pane(),
            KeyCode::Down | KeyCode::Char('j') => { self.move_cursor(1); None }
            KeyCode::Up   | KeyCode::Char('k') => { self.move_cursor(-1); None }
            _ => self.handle_pane_key(key),
        }
    }

    fn switch_to(&mut self, tab: Tab) -> Option<AppAction> {
        self.tab = tab;
        match tab {
            Tab::Catalog if !self.catalog.loaded => Some(AppAction::RefetchCatalog),
            Tab::Updates if !self.updates.loaded => Some(AppAction::RefetchUpdates),
            _ => None,
        }
    }

    fn refresh_current_pane(&mut self) -> Option<AppAction> {
        match self.tab {
            Tab::Queue => None, // queue is live
            Tab::Catalog => Some(AppAction::RefetchCatalog),
            Tab::Updates => Some(AppAction::RefetchUpdates),
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let (cursor, len) = match self.tab {
            Tab::Queue => (&mut self.queue.cursor, self.queue_len()),
            Tab::Catalog => (&mut self.catalog.cursor, self.catalog.models.len()),
            Tab::Updates => (&mut self.updates.cursor, self.updates.items.len()),
        };
        if len == 0 { return; }
        let new = (*cursor as isize + delta).rem_euclid(len as isize);
        *cursor = new as usize;
    }

    fn handle_pane_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        match self.tab {
            Tab::Queue => self.handle_queue_key(key),
            Tab::Catalog => self.handle_catalog_key(key),
            Tab::Updates => self.handle_updates_key(key),
        }
    }

    fn handle_queue_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        let selected_id = self.selected_queue_id()?;
        match key.code {
            KeyCode::Char('a') => {
                self.modal = Some(Modal::AddDownload {
                    url: String::new(),
                    model_type: String::new(),
                    focus: AddDownloadField::Url,
                });
                None
            }
            KeyCode::Char('c') => {
                self.modal = Some(Modal::Confirm {
                    prompt: format!("Cancel download {}?", short(selected_id)),
                    on_yes: AppAction::Cancel(selected_id),
                });
                None
            }
            KeyCode::Char('d') => {
                self.modal = Some(Modal::Confirm {
                    prompt: format!("Delete job {} from catalog?", short(selected_id)),
                    on_yes: AppAction::Delete(selected_id),
                });
                None
            }
            _ => None,
        }
    }

    fn handle_catalog_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        if self.catalog.search_focused {
            return self.handle_search_key(key);
        }
        let selected = self.catalog.models.get(self.catalog.cursor)?;
        match key.code {
            KeyCode::Char('/') => { self.catalog.search_focused = true; None }
            KeyCode::Char('D') => {
                self.modal = Some(Modal::Confirm {
                    prompt: format!("Delete {} and its file?",
                        selected.model_name.as_deref().unwrap_or("model")),
                    on_yes: AppAction::Delete(selected.id),
                });
                None
            }
            KeyCode::Char('R') => Some(AppAction::RedownloadModel(selected.id)),
            _ => None,
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => { self.catalog.search_focused = false; None }
            KeyCode::Backspace => { self.catalog.filter.pop(); None }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.catalog.filter.push(c);
                None
            }
            _ => None,
        }
    }

    fn handle_updates_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        let selected = self.updates.items.get(self.updates.cursor)?;
        match key.code {
            KeyCode::Enter => Some(AppAction::DownloadVersion {
                model_id: selected.model_id,
                version_id: selected.available_version_id,
            }),
            KeyCode::Char('R') => Some(AppAction::CheckUpdates),
            _ => None,
        }
    }

    fn selected_queue_id(&self) -> Option<Uuid> {
        let cursor = self.queue.cursor;
        let active_len = self.queue.active.len();
        if cursor < active_len {
            Some(self.queue.active[cursor].id)
        } else {
            self.queue.queued.get(cursor - active_len).map(|q| q.id)
        }
    }

    fn handle_key_modal(&mut self, modal: Modal, key: KeyEvent) -> Option<AppAction> {
        match (modal, key.code) {
            (Modal::Confirm { on_yes, .. }, KeyCode::Char('y')) | (Modal::Confirm { on_yes, .. }, KeyCode::Char('Y')) => {
                self.modal = None;
                Some(on_yes)
            }
            (Modal::Confirm { .. }, _) => { self.modal = None; None }
            (Modal::Help, _) => { self.modal = None; None }
            (Modal::Error { .. }, _) => { self.modal = None; None }
            (Modal::AddDownload { url, model_type, focus }, code) => {
                self.handle_add_download(url, model_type, focus, code, key.modifiers)
            }
        }
    }

    fn handle_add_download(
        &mut self,
        mut url: String,
        mut model_type: String,
        focus: AddDownloadField,
        code: KeyCode,
        mods: KeyModifiers,
    ) -> Option<AppAction> {
        match (focus, code) {
            (_, KeyCode::Esc) => { self.modal = None; None }
            (_, KeyCode::Tab) => {
                let next = match focus {
                    AddDownloadField::Url => AddDownloadField::ModelType,
                    AddDownloadField::ModelType => AddDownloadField::Submit,
                    AddDownloadField::Submit => AddDownloadField::Url,
                };
                self.modal = Some(Modal::AddDownload { url, model_type, focus: next });
                None
            }
            (AddDownloadField::Submit, KeyCode::Enter) => {
                self.modal = None;
                let mt = if model_type.is_empty() { None } else { Some(model_type) };
                Some(AppAction::AddDownload { url, model_type: mt })
            }
            (field, KeyCode::Backspace) => {
                let target = match field {
                    AddDownloadField::Url => &mut url,
                    AddDownloadField::ModelType => &mut model_type,
                    AddDownloadField::Submit => return None,
                };
                target.pop();
                self.modal = Some(Modal::AddDownload { url, model_type, focus });
                None
            }
            (field, KeyCode::Char(c)) if !mods.contains(KeyModifiers::CONTROL) => {
                let target = match field {
                    AddDownloadField::Url => &mut url,
                    AddDownloadField::ModelType => &mut model_type,
                    AddDownloadField::Submit => return None,
                };
                target.push(c);
                self.modal = Some(Modal::AddDownload { url, model_type, focus });
                None
            }
            _ => {
                self.modal = Some(Modal::AddDownload { url, model_type, focus });
                None
            }
        }
    }
}

fn short(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}
```

- [ ] **Step 3: Reducer tests**

Append to `src/tui/app.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn snap_with_one_active() -> Snapshot {
        Snapshot {
            active: vec![ActiveJob {
                id: Uuid::nil(),
                model_name: Some("foo".into()),
                version_name: Some("v1".into()),
                model_type: Some("LORA".into()),
                bytes_received: 0,
                total_bytes: Some(100),
                dest_path: None,
                started_at: None,
                download_reason: None,
            }],
            queued: vec![],
            free_bytes: 0,
            catalog_dirty: false,
            updates_dirty: false,
            seq: 1,
        }
    }

    #[test]
    fn snapshot_marks_connected_and_populates_queue() {
        let mut app = App::default();
        let action = app.handle(AppEvent::Snapshot(snap_with_one_active()));
        assert_eq!(app.connection, ConnState::Connected);
        assert_eq!(app.queue.active.len(), 1);
        assert!(action.is_none());
    }

    #[test]
    fn catalog_dirty_triggers_refetch() {
        let mut app = App::default();
        let mut snap = snap_with_one_active();
        snap.catalog_dirty = true;
        let action = app.handle(AppEvent::Snapshot(snap));
        assert_eq!(action, Some(AppAction::RefetchCatalog));
    }

    #[test]
    fn tab_keys_switch_panes() {
        let mut app = App::default();
        app.handle(AppEvent::Key(key('2')));
        assert_eq!(app.tab, Tab::Catalog);
        app.handle(AppEvent::Key(key('3')));
        assert_eq!(app.tab, Tab::Updates);
    }

    #[test]
    fn switching_to_unloaded_catalog_triggers_refetch() {
        let mut app = App::default();
        let action = app.handle(AppEvent::Key(key('2')));
        assert_eq!(action, Some(AppAction::RefetchCatalog));
    }

    #[test]
    fn cursor_movement_clamps_to_list_length() {
        let mut app = App::default();
        app.handle(AppEvent::Snapshot(snap_with_one_active()));
        app.handle(AppEvent::Key(key('j'))); // 0 → 1, but len=1 ⇒ wraps to 0
        assert_eq!(app.queue.cursor, 0);
        app.handle(AppEvent::Key(key('k')));
        assert_eq!(app.queue.cursor, 0);
    }

    #[test]
    fn cancel_opens_confirm_modal_with_action() {
        let mut app = App::default();
        app.handle(AppEvent::Snapshot(snap_with_one_active()));
        app.handle(AppEvent::Key(key('c')));
        match app.modal {
            Some(Modal::Confirm { on_yes: AppAction::Cancel(_), .. }) => {}
            other => panic!("expected confirm-cancel modal, got {other:?}"),
        }
    }

    #[test]
    fn confirm_y_returns_action_and_closes_modal() {
        let mut app = App::default();
        app.handle(AppEvent::Snapshot(snap_with_one_active()));
        app.handle(AppEvent::Key(key('c')));
        let out = app.handle(AppEvent::Key(key('y')));
        assert!(matches!(out, Some(AppAction::Cancel(_))));
        assert!(app.modal.is_none());
    }

    #[test]
    fn confirm_n_closes_modal_without_action() {
        let mut app = App::default();
        app.handle(AppEvent::Snapshot(snap_with_one_active()));
        app.handle(AppEvent::Key(key('c')));
        let out = app.handle(AppEvent::Key(key('n')));
        assert!(out.is_none());
        assert!(app.modal.is_none());
    }

    #[test]
    fn action_failure_opens_error_modal() {
        let mut app = App::default();
        let out = app.handle(AppEvent::ActionResult {
            action: AppAction::CheckUpdates,
            outcome: Err("daemon error: nope".into()),
        });
        assert!(out.is_none());
        match app.modal {
            Some(Modal::Error { ref message }) => assert!(message.contains("nope")),
            _ => panic!("expected error modal"),
        }
    }

    #[test]
    fn reconnecting_event_updates_conn_state() {
        let mut app = App::default();
        app.handle(AppEvent::StreamReconnecting { attempt: 3 });
        assert_eq!(app.connection, ConnState::Reconnecting { attempt: 3 });
    }
}
```

- [ ] **Step 4: Wire `app` into `tui/mod.rs`**

```rust
pub mod app;
pub mod format;
```

- [ ] **Step 5: Run tests**

Run: `cargo test --quiet tui::app`
Expected: 9 passed.

Run: `cargo clippy --quiet -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src/tui/app.rs src/tui/mod.rs
git commit -m "feat(tui): pure-reducer App state and tests"
```

---

### Task 10: Snapshot stream task, input task, main loop

**Goal:** Wire the App to live data — snapshot stream + input task feed events into an mpsc, main loop drains it and redraws.

**Files:**
- Create: `src/tui/ipc.rs`
- Create: `src/tui/input.rs`
- Modify: `src/tui/mod.rs` — replace stub `run` with the real loop
- Create: `src/tui/ui/mod.rs` — minimal placeholder draw

- [ ] **Step 1: Snapshot stream task**

Create `src/tui/ipc.rs`:

```rust
use crate::ipc::{IpcSubscriber, protocol::Frame};
use crate::tui::app::AppEvent;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

pub async fn snapshot_task(socket: PathBuf, tx: mpsc::Sender<AppEvent>) {
    let mut attempt: u32 = 0;
    loop {
        let _ = tx.send(AppEvent::StreamReconnecting { attempt }).await;
        match IpcSubscriber::connect(&socket).await {
            Ok(mut sub) => loop {
                match sub.next_frame().await {
                    Ok(Some(Frame::Snapshot(snap))) => {
                        attempt = 0;
                        if tx.send(AppEvent::Snapshot(snap)).await.is_err() { return; }
                    }
                    Ok(Some(Frame::Subscribed)) => continue,
                    Ok(Some(Frame::Error { .. })) => break,
                    Ok(None) => break, // EOF, reconnect
                    Err(_) => break,
                }
            },
            Err(_) => {}
        }
        attempt = attempt.saturating_add(1);
        let backoff_ms = (250u64.saturating_mul(1u64 << attempt.min(5))).min(5000);
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
    }
}
```

- [ ] **Step 2: Action dispatcher (stubs for now)**

Append to `src/tui/ipc.rs`:

```rust
use crate::ipc::IpcClient;
use crate::ipc::protocol::Request;
use crate::tui::app::{AppAction, AppEvent};

pub async fn dispatch_action(
    socket: PathBuf,
    action: AppAction,
    tx: mpsc::Sender<AppEvent>,
) {
    let outcome = run_action(&socket, action.clone()).await;
    let _ = tx.send(AppEvent::ActionResult { action, outcome }).await;
}

async fn run_action(socket: &std::path::Path, action: AppAction) -> Result<(), String> {
    let mut client = IpcClient::connect(socket).await.map_err(|e| e.to_string())?;
    match action {
        AppAction::Cancel(id) => one_shot(&mut client, &Request::Cancel { id }).await,
        AppAction::Delete(id) => one_shot(&mut client, &Request::DeleteModel { id }).await,
        AppAction::RedownloadModel(id) => one_shot(&mut client, &Request::RedownloadModel { id }).await,
        AppAction::AddDownload { url, model_type } =>
            one_shot(&mut client, &Request::AddDownload { url, model_type }).await,
        AppAction::DownloadVersion { model_id, version_id } =>
            one_shot(&mut client, &Request::DownloadVersion { model_id, version_id }).await,
        AppAction::CheckUpdates => one_shot(&mut client, &Request::CheckUpdates).await,
        AppAction::RefetchCatalog => {
            let resp = client.send(&Request::ListModelsEnriched).await.map_err(|e| e.to_string())?;
            // Parse and forward via tx is the responsibility of the caller; for
            // RefetchCatalog/RefetchUpdates the dispatcher uses dedicated functions.
            // See `dispatch_refetch_catalog` below.
            let _ = resp;
            Ok(())
        }
        AppAction::RefetchUpdates => Ok(()),
    }
}

async fn one_shot(client: &mut IpcClient, req: &Request) -> Result<(), String> {
    let resp = client.send(req).await.map_err(|e| e.to_string())?;
    match resp {
        crate::ipc::protocol::Response::Ok(_) => Ok(()),
        crate::ipc::protocol::Response::Err { message } => Err(message),
    }
}

/// Dispatch a catalog refetch and forward the typed result.
pub async fn dispatch_refetch_catalog(socket: PathBuf, tx: mpsc::Sender<AppEvent>) {
    let result: Result<Vec<crate::ipc::protocol::EnrichedModel>, String> = async {
        let mut client = IpcClient::connect(&socket).await.map_err(|e| e.to_string())?;
        let resp = client.send(&Request::ListModelsEnriched).await.map_err(|e| e.to_string())?;
        match resp {
            crate::ipc::protocol::Response::Ok(value) =>
                serde_json::from_value(value).map_err(|e| e.to_string()),
            crate::ipc::protocol::Response::Err { message } => Err(message),
        }
    }.await;

    match result {
        Ok(models) => { let _ = tx.send(AppEvent::CatalogLoaded(models)).await; }
        Err(message) => {
            let _ = tx.send(AppEvent::ActionResult {
                action: AppAction::RefetchCatalog,
                outcome: Err(message),
            }).await;
        }
    }
}

/// Dispatch an updates refetch and forward the typed result.
pub async fn dispatch_refetch_updates(socket: PathBuf, tx: mpsc::Sender<AppEvent>) {
    use crate::tui::app::UpdateInfo;
    let result: Result<Vec<UpdateInfo>, String> = async {
        let mut client = IpcClient::connect(&socket).await.map_err(|e| e.to_string())?;
        let resp = client.send(&Request::ListUpdates).await.map_err(|e| e.to_string())?;
        match resp {
            crate::ipc::protocol::Response::Ok(value) =>
                serde_json::from_value(value).map_err(|e| e.to_string()),
            crate::ipc::protocol::Response::Err { message } => Err(message),
        }
    }.await;

    match result {
        Ok(items) => { let _ = tx.send(AppEvent::UpdatesLoaded(items)).await; }
        Err(message) => {
            let _ = tx.send(AppEvent::ActionResult {
                action: AppAction::RefetchUpdates,
                outcome: Err(message),
            }).await;
        }
    }
}
```

The Refetch* arms in `run_action` end up unused because the main loop dispatches them via `dispatch_refetch_*` instead. Keep `run_action`'s match arms for them returning `Ok(())` so a stray dispatch doesn't crash; the real path is via the dedicated functions.

- [ ] **Step 3: Input task**

Create `src/tui/input.rs`:

```rust
use crate::tui::app::AppEvent;
use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use tokio::sync::mpsc;

pub async fn input_task(tx: mpsc::Sender<AppEvent>) {
    let mut events = EventStream::new();
    while let Some(Ok(event)) = events.next().await {
        if let Event::Key(k) = event {
            if tx.send(AppEvent::Key(k)).await.is_err() { return; }
        }
        // Resize and other events: no-op; ratatui handles redraw on draw.
    }
}
```

- [ ] **Step 4: Tick task**

Append to `src/tui/input.rs`:

```rust
pub async fn tick_task(tx: mpsc::Sender<AppEvent>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        if tx.send(AppEvent::Tick).await.is_err() { return; }
    }
}
```

- [ ] **Step 5: Minimal `ui/mod.rs`**

Create `src/tui/ui/mod.rs`:

```rust
use crate::tui::app::App;
use ratatui::Frame;
use ratatui::widgets::{Block, Borders, Paragraph};

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let text = format!(
        "TUI scaffold — tab={:?}  active={} queued={}  modal={:?}",
        app.tab,
        app.queue.active.len(),
        app.queue.queued.len(),
        app.modal.as_ref().map(|_| "open"),
    );
    f.render_widget(
        Paragraph::new(text).block(Block::default().title("comfyui-dl tui").borders(Borders::ALL)),
        area,
    );
}
```

- [ ] **Step 6: Replace `tui::run`**

Edit `src/tui/mod.rs`:

```rust
pub mod app;
pub mod format;
pub mod input;
pub mod ipc;
pub mod ui;

use crate::config::Config;
use anyhow::Result;
use std::io::IsTerminal;
use tokio::sync::mpsc;

pub async fn run(config: Config) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("comfyui-dl tui requires an interactive terminal");
    }

    let socket = config.daemon.socket_path.clone();
    let mut terminal = ratatui::init();
    let result = run_inner(socket, &mut terminal).await;
    ratatui::restore();
    result
}

async fn run_inner(
    socket: std::path::PathBuf,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<()> {
    use app::{App, AppEvent, AppAction};

    let (tx, mut rx) = mpsc::channel::<AppEvent>(64);
    tokio::spawn(ipc::snapshot_task(socket.clone(), tx.clone()));
    tokio::spawn(input::input_task(tx.clone()));
    tokio::spawn(input::tick_task(tx.clone()));

    let mut app = App::default();
    while let Some(event) = rx.recv().await {
        let action = app.handle(event);
        if let Some(a) = action {
            match a {
                AppAction::RefetchCatalog =>
                    tokio::spawn(ipc::dispatch_refetch_catalog(socket.clone(), tx.clone())),
                AppAction::RefetchUpdates =>
                    tokio::spawn(ipc::dispatch_refetch_updates(socket.clone(), tx.clone())),
                other =>
                    tokio::spawn(ipc::dispatch_action(socket.clone(), other, tx.clone())),
            };
        }
        terminal.draw(|f| ui::draw(f, &app))?;
        if app.should_quit { break; }
    }
    Ok(())
}
```

- [ ] **Step 7: Build & smoke**

Run: `cargo build --quiet`

In one shell start the daemon:
```bash
RUST_LOG=info ./target/debug/comfyui-downloader
```

In another:
```bash
./target/debug/comfyui-dl tui
```

Expected: a bordered box, "TUI scaffold — tab=Queue active=0 queued=0 modal=None". Pressing `2` switches to Catalog (visible in the title text), `q` exits cleanly with the terminal restored.

- [ ] **Step 8: Commit**

```bash
git add src/tui/
git commit -m "feat(tui): wire snapshot stream, input loop, and minimal scaffold UI"
```

---

### Task 11: Render the Queue pane and global chrome

**Goal:** Replace the placeholder `ui::draw` with a real layout: tab bar at the top, footer with keybinds, and a Queue pane render.

**Files:**
- Modify: `src/tui/ui/mod.rs`
- Create: `src/tui/ui/queue.rs`

The render code is mechanical ratatui composition. The plan shows the structure; the implementer fills in styling.

- [ ] **Step 1: Top-level layout**

Replace `src/tui/ui/mod.rs`:

```rust
mod queue;
mod catalog;
mod updates;
mod modal;

use crate::tui::app::{App, ConnState, Tab};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // banner (Reconnecting)
            Constraint::Length(3),  // tabs
            Constraint::Min(0),     // body
            Constraint::Length(1),  // footer (keybinds)
        ])
        .split(area);

    draw_banner(f, app, chunks[0]);
    draw_tabs(f, app, chunks[1]);
    match app.tab {
        Tab::Queue => queue::render(f, app, chunks[2]),
        Tab::Catalog => catalog::render(f, app, chunks[2]),
        Tab::Updates => updates::render(f, app, chunks[2]),
    }
    draw_footer(f, app, chunks[3]);

    if let Some(m) = &app.modal {
        modal::render(f, app, m);
    }
}

fn draw_banner(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let text = match app.connection {
        ConnState::Connected => format!("● Connected   Free: {}", crate::tui::format::format_bytes(app.free_bytes)),
        ConnState::Reconnecting { attempt } => format!("● Disconnected — reconnecting (attempt {attempt})…"),
    };
    let style = match app.connection {
        ConnState::Connected => Style::default().fg(Color::Green),
        ConnState::Reconnecting { .. } => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    };
    f.render_widget(Paragraph::new(text).style(style), area);
}

fn draw_tabs(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let titles: Vec<Line> = ["Queue", "Catalog", "Updates"]
        .iter().map(|t| Line::from(Span::raw(*t))).collect();
    let selected = match app.tab {
        Tab::Queue => 0, Tab::Catalog => 1, Tab::Updates => 2,
    };
    let widget = Tabs::new(titles)
        .block(Block::default().borders(Borders::BOTTOM))
        .select(selected)
        .highlight_style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan));
    f.render_widget(widget, area);
}

fn draw_footer(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let hints = match app.tab {
        Tab::Queue   => "a:add  c:cancel  d:delete  Enter:details  ?:help  q:quit",
        Tab::Catalog => "/:search  D:delete  R:redownload-missing  Enter:details  ?:help",
        Tab::Updates => "Enter:download  R:check-updates  ?:help",
    };
    f.render_widget(Paragraph::new(hints).style(Style::default().fg(Color::DarkGray)), area);
}
```

- [ ] **Step 2: Queue pane render**

Create `src/tui/ui/queue.rs`:

```rust
use crate::ipc::protocol::{ActiveJob, QueuedJob};
use crate::tui::app::{App, ConnState};
use crate::tui::format::{format_bytes, format_duration, trim_path};
use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let dimmed = matches!(app.connection, ConnState::Reconnecting { .. });

    if app.queue.active.is_empty() && app.queue.queued.is_empty() {
        let p = Paragraph::new("No downloads. Press `a` to add one.")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title("Queue"));
        f.render_widget(p, area);
        return;
    }

    let mut items: Vec<ListItem> = Vec::new();
    if !app.queue.active.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "Active",
            Style::default().add_modifier(Modifier::BOLD),
        ))));
        for job in &app.queue.active {
            items.push(active_item(job, dimmed));
        }
    }
    if !app.queue.queued.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("Queued ({})", app.queue.queued.len()),
            Style::default().add_modifier(Modifier::BOLD),
        ))));
        for job in &app.queue.queued {
            items.push(queued_item(job, dimmed));
        }
    }

    // The selectable rows are interleaved with header rows; map cursor → list index.
    let selectable_idx = cursor_to_list_index(app);
    let mut state = ListState::default();
    state.select(Some(selectable_idx));

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Queue"))
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, area, &mut state);
}

fn active_item(job: &ActiveJob, dimmed: bool) -> ListItem {
    let mut lines: Vec<Line> = Vec::new();
    let title = display_title(job.model_name.as_deref(), job.version_name.as_deref(), None);
    let mt = job.model_type.as_deref().unwrap_or("?");
    lines.push(line(format!("  {title}    [{mt}]"), dimmed));

    let pct = match job.total_bytes {
        Some(t) if t > 0 => (job.bytes_received as f64 / t as f64 * 100.0) as u64,
        _ => 0,
    };
    let bar = progress_bar(pct, 30);
    let total_str = job.total_bytes.map(format_bytes).unwrap_or_else(|| "?".into());
    lines.push(line(
        format!("  {bar} {pct:>3}%  ({} / {})", format_bytes(job.bytes_received), total_str),
        dimmed,
    ));

    if let (Some(started), Some(total)) = (job.started_at, job.total_bytes) {
        let elapsed = Utc::now().signed_duration_since(started).num_seconds().max(1) as f64;
        if job.bytes_received > 0 && total > job.bytes_received {
            let speed = job.bytes_received as f64 / elapsed;
            let eta = ((total - job.bytes_received) as f64 / speed) as u64;
            lines.push(line(
                format!("  ETA: {}  ({}/s)", format_duration(eta), format_bytes(speed as u64)),
                dimmed,
            ));
        }
    }

    if let Some(p) = &job.dest_path {
        lines.push(line(format!("  → {}", trim_path(p)), dimmed));
    }

    ListItem::new(lines)
}

fn queued_item(job: &QueuedJob, dimmed: bool) -> ListItem {
    let title = display_title(job.model_name.as_deref(), job.version_name.as_deref(), Some(&job.url));
    let mt = job.model_type.as_deref().unwrap_or("?");
    let upgrade = if job.download_reason.as_deref() == Some("update_available") {
        "  (upgrade)"
    } else { "" };
    ListItem::new(line(format!("  {title}    [{mt}]{upgrade}"), dimmed))
}

fn display_title(name: Option<&str>, version: Option<&str>, fallback: Option<&str>) -> String {
    match (name, version) {
        (Some(n), Some(v)) => format!("{n} — {v}"),
        (Some(n), None) => n.to_string(),
        _ => fallback.unwrap_or("?").to_string(),
    }
}

fn line(text: String, dimmed: bool) -> Line<'static> {
    let style = if dimmed { Style::default().fg(Color::DarkGray) } else { Style::default() };
    Line::from(Span::styled(text, style))
}

fn progress_bar(pct: u64, width: usize) -> String {
    let filled = (pct as usize * width / 100).min(width);
    let empty = width - filled;
    format!("[{}{}]", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

/// Map the App's queue cursor (which counts only selectable rows) to the
/// list-widget index (which includes "Active" / "Queued (N)" headers and the
/// multi-line items). Each active item renders as up to 4 lines but the List
/// widget treats each ListItem as one logical row, so we just count items.
fn cursor_to_list_index(app: &App) -> usize {
    let mut idx = 0;
    if !app.queue.active.is_empty() { idx += 1; } // "Active" header
    let active_len = app.queue.active.len();
    if app.queue.cursor < active_len {
        return idx + app.queue.cursor;
    }
    idx += active_len;
    if !app.queue.queued.is_empty() { idx += 1; } // "Queued (N)" header
    idx + (app.queue.cursor - active_len)
}
```

- [ ] **Step 3: Stub the other panes**

Create `src/tui/ui/catalog.rs`:

```rust
use crate::tui::app::App;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Paragraph};

pub fn render(f: &mut Frame, _app: &App, area: Rect) {
    f.render_widget(
        Paragraph::new("Catalog (Task 12)").block(Block::default().title("Catalog").borders(Borders::ALL)),
        area,
    );
}
```

Create `src/tui/ui/updates.rs`:

```rust
use crate::tui::app::App;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Paragraph};

pub fn render(f: &mut Frame, _app: &App, area: Rect) {
    f.render_widget(
        Paragraph::new("Updates (Task 13)").block(Block::default().title("Updates").borders(Borders::ALL)),
        area,
    );
}
```

Create `src/tui/ui/modal.rs`:

```rust
use crate::tui::app::{App, Modal};
use ratatui::Frame;

pub fn render(_f: &mut Frame, _app: &App, _modal: &Modal) {
    // Real renderers added in Task 14.
}
```

- [ ] **Step 4: Build & smoke**

Run: `cargo build --quiet && ./target/debug/comfyui-dl tui`
Expected: tabs row, banner, queue pane border with empty-state message, footer with keybinds. Press `2` / `3` to switch tabs.

- [ ] **Step 5: Commit**

```bash
git add src/tui/ui/
git commit -m "feat(tui): render Queue pane, tabs, banner, footer"
```

---

### Task 12: Catalog pane

**Goal:** Render the catalog list with live filter, lazy load on first entry.

**Files:**
- Modify: `src/tui/ui/catalog.rs`

- [ ] **Step 1: Replace stub**

```rust
use crate::ipc::protocol::EnrichedModel;
use crate::tui::app::{App, ConnState};
use crate::tui::format::{format_bytes, trim_path};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let dimmed = matches!(app.connection, ConnState::Reconnecting { .. });

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    let search_style = if app.catalog.search_focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let search = Paragraph::new(format!("Search: {}", app.catalog.filter))
        .style(search_style)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(search, layout[0]);

    let filtered: Vec<&EnrichedModel> = app.catalog.models.iter()
        .filter(|m| matches_filter(m, &app.catalog.filter))
        .collect();

    if filtered.is_empty() {
        let msg = if app.catalog.loaded { "No models match filter." } else { "Loading…" };
        let p = Paragraph::new(msg)
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title("Catalog"));
        f.render_widget(p, layout[1]);
        return;
    }

    let items: Vec<ListItem> = filtered.iter()
        .map(|m| catalog_item(m, dimmed))
        .collect();

    let cursor = app.catalog.cursor.min(filtered.len().saturating_sub(1));
    let mut state = ListState::default();
    state.select(Some(cursor));

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Catalog"))
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, layout[1], &mut state);
}

fn matches_filter(m: &EnrichedModel, filter: &str) -> bool {
    if filter.is_empty() { return true; }
    let q = filter.to_lowercase();
    let in_name  = m.model_name.as_deref().map(|s| s.to_lowercase().contains(&q)).unwrap_or(false);
    let in_ver   = m.version_name.as_deref().map(|s| s.to_lowercase().contains(&q)).unwrap_or(false);
    let in_path  = m.dest_path.as_deref().map(|s| s.to_lowercase().contains(&q)).unwrap_or(false);
    in_name || in_ver || in_path
}

fn catalog_item(m: &EnrichedModel, dimmed: bool) -> ListItem<'static> {
    let title = match (m.model_name.as_deref(), m.version_name.as_deref()) {
        (Some(n), Some(v)) => format!("{n} — {v}"),
        (Some(n), None) => n.to_string(),
        _ => m.url.clone(),
    };
    let mt = m.model_type.as_deref().unwrap_or("?");
    let size = m.file_size.map(format_bytes).unwrap_or_else(|| "?".into());
    let path = m.dest_path.as_deref().map(trim_path).unwrap_or_else(|| "?".into());
    let line_text = format!("  {title}    [{mt}]   {size}   {path}");
    let style = if dimmed { Style::default().fg(Color::DarkGray) } else { Style::default() };
    ListItem::new(Line::from(Span::styled(line_text, style)))
}
```

- [ ] **Step 2: Build & smoke**

Run: `cargo build --quiet && ./target/debug/comfyui-dl tui`
Expected: switching to Catalog tab shows "Loading…" then a list of catalogued models. `/` enters search, typing filters live, Esc exits search.

- [ ] **Step 3: Commit**

```bash
git add src/tui/ui/catalog.rs
git commit -m "feat(tui): catalog pane with live search filter"
```

---

### Task 13: Updates pane

**Goal:** Render available updates, lazy load on first entry, refresh via `R`.

**Files:**
- Modify: `src/tui/ui/updates.rs`

- [ ] **Step 1: Replace stub**

```rust
use crate::tui::app::{App, ConnState, UpdateInfo};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let dimmed = matches!(app.connection, ConnState::Reconnecting { .. });

    if !app.updates.loaded {
        let p = Paragraph::new("Loading…")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title("Updates"));
        f.render_widget(p, area);
        return;
    }
    if app.updates.items.is_empty() {
        let p = Paragraph::new("All models are up to date.")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title("Updates"));
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app.updates.items.iter().map(|u| update_item(u, dimmed)).collect();
    let mut state = ListState::default();
    state.select(Some(app.updates.cursor.min(app.updates.items.len() - 1)));

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Updates"))
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, area, &mut state);
}

fn update_item(u: &UpdateInfo, dimmed: bool) -> ListItem<'static> {
    let mt = u.model_type.as_deref().unwrap_or("?");
    let path_tail = u.dest_path.as_deref()
        .and_then(|p| p.rsplit('/').next())
        .unwrap_or("?");
    let text = format!(
        "  {path_tail} — v{} → v{} ({})    [{mt}]",
        u.version_id, u.available_version_id, u.available_version_name,
    );
    let style = if dimmed { Style::default().fg(Color::DarkGray) } else { Style::default() };
    ListItem::new(Line::from(Span::styled(text, style)))
}
```

- [ ] **Step 2: Verify the daemon's `ListUpdates` shape matches `UpdateInfo`**

The handler returns `cat.list_updates_available()` which gives `Vec<DownloadJob>`. `DownloadJob` has fields: `id`, `model_id: Option<u64>`, `version_id: Option<u64>`, `available_version_id: Option<u64>`, `available_version_name: Option<String>`, `model_type`, `dest_path`. The `UpdateInfo` deserializer expects `model_id: u64` and `version_id: u64` non-optional.

Two options:
- (a) Change `UpdateInfo` to take `Option`s — simplest.
- (b) Change the daemon to filter out rows where `model_id` / `version_id` / `available_version_id` are null (they shouldn't be — `list_updates_available` joins on a flagged update).

Pick (a). Edit `src/tui/app.rs:UpdateInfo`:

```rust
#[derive(Clone, Debug, serde::Deserialize)]
pub struct UpdateInfo {
    pub id: uuid::Uuid,
    pub model_id: Option<u64>,
    pub version_id: Option<u64>,
    pub available_version_id: Option<u64>,
    #[serde(default)]
    pub available_version_name: Option<String>,
    pub model_type: Option<String>,
    pub dest_path: Option<String>,
}
```

And update `handle_updates_key` to use the optionals:

```rust
fn handle_updates_key(&mut self, key: KeyEvent) -> Option<AppAction> {
    let selected = self.updates.items.get(self.updates.cursor)?;
    match key.code {
        KeyCode::Enter => {
            let mid = selected.model_id?;
            let vid = selected.available_version_id?;
            Some(AppAction::DownloadVersion { model_id: mid, version_id: vid })
        }
        KeyCode::Char('R') => Some(AppAction::CheckUpdates),
        _ => None,
    }
}
```

And the renderer:
```rust
let mid = u.model_id.map(|n| n.to_string()).unwrap_or_else(|| "?".into());
let vid_now = u.version_id.map(|n| n.to_string()).unwrap_or_else(|| "?".into());
let vid_new = u.available_version_id.map(|n| n.to_string()).unwrap_or_else(|| "?".into());
let vname = u.available_version_name.as_deref().unwrap_or("?");
// then:
let text = format!("  {path_tail} — v{vid_now} → v{vid_new} ({vname})    [{mt}]   model_id={mid}");
```

- [ ] **Step 3: Build & smoke**

Run: `cargo build --quiet && ./target/debug/comfyui-dl tui`

Expected: Updates tab shows "Loading…" then either "All models are up to date." or a list of pending updates. `R` triggers a fresh check.

- [ ] **Step 4: Commit**

```bash
git add src/tui/ui/updates.rs src/tui/app.rs
git commit -m "feat(tui): updates pane with lazy load and check trigger"
```

---

### Task 14: Modals (Confirm, Help, Error, AddDownload)

**Goal:** Implement the modal renderer that overlays a centered box on top of the panes.

**Files:**
- Modify: `src/tui/ui/modal.rs`

- [ ] **Step 1: Implement `modal::render`**

```rust
use crate::tui::app::{App, AddDownloadField, Modal};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

pub fn render(f: &mut Frame, _app: &App, modal: &Modal) {
    let area = f.area();
    match modal {
        Modal::Confirm { prompt, .. } => confirm(f, area, prompt),
        Modal::Error { message } => error(f, area, message),
        Modal::Help => help(f, area),
        Modal::AddDownload { url, model_type, focus } => add_download(f, area, url, model_type, *focus),
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length((area.height.saturating_sub(h)) / 2), Constraint::Length(h), Constraint::Min(0)])
        .split(area);
    let h_split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length((area.width.saturating_sub(w)) / 2), Constraint::Length(w), Constraint::Min(0)])
        .split(v[1]);
    h_split[1]
}

fn confirm(f: &mut Frame, area: Rect, prompt: &str) {
    let r = centered(area, 60, 5);
    f.render_widget(Clear, r);
    let body = Paragraph::new(vec![
        Line::from(prompt.to_string()),
        Line::from(""),
        Line::from(Span::styled("[Y]es / [N]o", Style::default().add_modifier(Modifier::BOLD))),
    ])
    .alignment(Alignment::Center)
    .block(Block::default().borders(Borders::ALL).title("Confirm"));
    f.render_widget(body, r);
}

fn error(f: &mut Frame, area: Rect, message: &str) {
    let r = centered(area, 70, 7);
    f.render_widget(Clear, r);
    let body = Paragraph::new(message.to_string())
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::Red))
        .block(Block::default().borders(Borders::ALL).title("Error — press any key"));
    f.render_widget(body, r);
}

fn help(f: &mut Frame, area: Rect) {
    let r = centered(area, 50, 14);
    f.render_widget(Clear, r);
    let lines = vec![
        Line::from("Global"),
        Line::from("  1/2/3 or Tab/Shift-Tab   switch tabs"),
        Line::from("  ?                        toggle help"),
        Line::from("  q / Esc                  close modal / quit"),
        Line::from("  r                        refresh current pane"),
        Line::from(""),
        Line::from("Queue"),
        Line::from("  a c d                    add / cancel / delete"),
        Line::from(""),
        Line::from("Catalog"),
        Line::from("  /  D  R                  search / delete / redownload"),
        Line::from(""),
        Line::from("Updates"),
        Line::from("  Enter R                  download / check now"),
    ];
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Help")),
        r,
    );
}

fn add_download(f: &mut Frame, area: Rect, url: &str, model_type: &str, focus: AddDownloadField) {
    let r = centered(area, 70, 9);
    f.render_widget(Clear, r);
    let url_style = field_style(focus == AddDownloadField::Url);
    let mt_style = field_style(focus == AddDownloadField::ModelType);
    let submit_style = field_style(focus == AddDownloadField::Submit);
    let lines = vec![
        Line::from(Span::styled(format!("URL:        {url}"), url_style)),
        Line::from(""),
        Line::from(Span::styled(format!("Model type: {model_type}  (optional)"), mt_style)),
        Line::from(""),
        Line::from(Span::styled("[ Submit ]", submit_style)),
        Line::from(""),
        Line::from("Tab to cycle  •  Enter on Submit  •  Esc to cancel"),
    ];
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Add download")),
        r,
    );
}

fn field_style(focused: bool) -> Style {
    if focused { Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD) }
    else { Style::default() }
}
```

- [ ] **Step 2: Build & smoke**

Run: `cargo build --quiet && ./target/debug/comfyui-dl tui`

Verify:
- `?` opens Help; any key dismisses.
- On a queued/active job, `c` opens a Confirm; `y` triggers cancel; `n` cancels.
- `a` opens AddDownload; Tab cycles fields; typing fills them; Tab to Submit + Enter submits.
- Inducing an action error (e.g. AddDownload with empty URL) opens an Error modal.

- [ ] **Step 3: Commit**

```bash
git add src/tui/ui/modal.rs
git commit -m "feat(tui): confirm/help/error/add-download modals"
```

---

### Task 15: Final polish — manual smoke and docs

**Goal:** Run the spec's manual smoke checklist and fix any rough edges. No new code unless smoke uncovers something.

**Files:**
- Modify: `README.md` if existing CLI usage docs reference `tui` (search first)

- [ ] **Step 1: Run the smoke checklist** from `docs/plans/2026-05-06-tui-design.md` "Testing strategy → Manual smoke checklist". For each item, confirm pass/fail.

- [ ] **Step 2: Add `tui` to README quick-start (if README has a CLI command list)**

Run: `rg -n "comfyui-dl" README.md | head -20`
If there's a usage block, add a one-liner:

```
comfyui-dl tui                      # interactive terminal UI
```

- [ ] **Step 3: Final clippy / test sweep**

Run: `cargo clippy --quiet -- -D warnings`
Run: `cargo test --quiet`
Expected: clean, all green.

- [ ] **Step 4: Commit polish (if any changes)**

```bash
git add -A
git commit -m "docs: mention comfyui-dl tui in README" # if README changed
```

---

## Spec coverage check (writer self-review)

| Spec section | Task(s) |
|---|---|
| Surface scope (Queue + Catalog + Updates) | 11, 12, 13 |
| Refresh strategy (push-based snapshots) | 6, 10 |
| Binary structure (`comfyui-dl tui`) | 8 |
| Snapshot payload (live + dirty pulses) | 3, 6 |
| Layout (three tabs) | 11 |
| ID display rules (no IDs, names + versions) | 1, 11, 12 |
| Path trimming | 8 (helper), 11/12/13 (callers) |
| Connection model (long-lived + ephemeral) | 6, 7, 10 |
| State management (pure reducer) | 9 |
| IPC `Subscribe` + `Frame` | 4 |
| `IpcServer` streaming branch | 5 |
| `IpcSubscriber` client | 7 |
| Daemon broadcast bus | 2 |
| Snapshot builder | 3 |
| `ActiveJob` / `QueuedJob` types | 3 |
| `version_name` schema/sidecar | 1 |
| Coalescing window (10 ms) | 6 |
| Lagged broadcast recovery | 6 |
| Reconnect with backoff | 10 |
| Disconnect banner + dimmed pane | 11, 12, 13 |
| TTY check + panic-safe restore | 10 |
| Modals: confirm / error / help / add | 14 |
| Catalog `R` (per-model redownload IPC) | 4 |
| Reducer unit tests | 9 |
| `IpcSubscribe` integration test | 6 |
| Path trim unit tests | 8 |
| Manual smoke checklist | 15 |

No spec items missing.
