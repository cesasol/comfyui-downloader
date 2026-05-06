# TUI Design — Queue & Catalog Management

**Date:** 2026-05-06
**Scope:** Add a `comfyui-dl tui` subcommand providing a full-screen terminal UI for managing the download queue, browsing the catalog, and applying available updates.

## Goals

- Live view of active and queued downloads with progress and ETA.
- Cancel, retry, and delete downloads from the keyboard.
- Browse and filter the catalogue of downloaded models; delete or re-queue from there.
- Trigger and apply CivitAI version updates without dropping back to the CLI.
- Reuse the existing daemon and IPC layer; no second process, no second binary.

## Non-goals (v1)

- Reordering the queue / setting priority.
- In-terminal image previews of model thumbnails.
- Custom theming.
- Multiplexed request/response on a single socket connection.

## Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Surface scope | Full management (Queue + Catalog + Updates) | Single place to operate the daemon; catalog/updates already exposed via IPC |
| Refresh strategy | Daemon-pushed snapshots over a long-lived connection | Smoother progress than polling; matches the user's preference for an event-driven model |
| Binary | `comfyui-dl tui` subcommand | One binary to install/document; ratatui deps acceptable in the CLI |
| Snapshot payload | Live data only (`active`, `queued`, `free_bytes`) plus `catalog_dirty` / `updates_dirty` pulses | Avoids paying the full catalogue cost every tick while still auto-refreshing on change |
| Snapshot semantics | Full snapshot per push (no diffs) | Simpler client; payload is small (live data only) |
| Layout | Three top-level tabs | Familiar `htop`/`btop` feel; cheap to extend |
| ID display | Hidden in TUI; cursor selection only | Names + version names communicate identity; UUIDs are noise |
| Connection model | Two connections per session: long-lived `Subscribe` + short-lived per-action | Reuses the existing one-shot client pattern; no protocol multiplexing needed |
| State management | Pure reducer + dispatched actions over mpsc | Testable without I/O |

---

## Architecture overview

```
┌─ comfyui-downloader (daemon) ───────────────────────────┐
│  queue / downloader / updater  ─pushes events→  ┐       │
│                                                  │      │
│  Catalog (SQLite, Mutex)                         ▼      │
│                                       broadcast::Sender │
│                                            <Event>      │
│  IpcServer (Unix socket)                        │       │
│   ├─ ephemeral conn → Request/Response (today)         │
│   └─ subscribe conn ───receiver.recv()──→ Snapshot ────┼──▶
└────────────────────────────────────────────────────────┘   │
                                                             │
┌─ comfyui-dl tui (TUI client) ───────────────────────────┐  │
│  main loop:                                             │  │
│   ├─ tokio task A: read Snapshot frames ──→ mpsc ───┐   │◀─┘
│   ├─ tokio task B: crossterm event stream ──→ mpsc ─┤   │
│   └─ render loop: select(rx) → update App → draw    │   │
│                                                      │   │
│  Actions (cancel/add/delete) → fresh IpcClient conn ─→──┼──▶ daemon
└─────────────────────────────────────────────────────────┘
```

- The TUI is a tokio task inside the existing `comfyui-dl` binary, gated by the `Tui` subcommand.
- Two connections per session: one persistent stream, plus short-lived ones for each action.
- The daemon's broadcast channel is the single source of truth; queue/downloader/updater push events into it; the subscribe handler converts events to snapshot frames.

---

## IPC protocol changes (`src/ipc/protocol.rs`)

```rust
pub enum Request {
    // ...existing variants...
    Subscribe,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Subscribed,
    Snapshot {
        active: Vec<ActiveJob>,
        queued: Vec<QueuedJob>,
        free_bytes: u64,
        catalog_dirty: bool,
        updates_dirty: bool,
        seq: u64,
    },
    Error { message: String },
}

pub struct ActiveJob {
    pub id: Uuid,
    pub model_name: Option<String>,
    pub version_name: Option<String>,
    pub model_type: Option<String>,
    pub bytes_received: u64,
    pub total_bytes: Option<u64>,
    pub dest_path: Option<String>,
    pub started_at: DateTime<Utc>,
    pub download_reason: Option<String>,
}

pub struct QueuedJob {
    pub id: Uuid,
    pub url: String,
    pub model_name: Option<String>,
    pub version_name: Option<String>,
    pub model_type: Option<String>,
    pub download_reason: Option<String>,
}
```

- `IpcServer` recognises `Subscribe` and does not close the connection — it enters a streaming loop until the client disconnects or a shutdown signal fires.
- A new `IpcSubscriber` type yields `Frame`s as a `Stream<Item = Result<Frame>>`.
- The existing `Request`/`Response` flow is unchanged for all other variants.
- `ActiveJob` and `QueuedJob` replace the ad-hoc `serde_json::Value` used today in `Request::GetStatus`. The CLI's `print_status` is updated to consume the typed form. The daemon's `Subscribe` handler reuses the same constructor as `GetStatus`, so both paths share the snapshot-building code.

`EnrichedModel` gains a `version_name: Option<String>` field. The catalog schema gains a `version_name TEXT` column (migration in `src/catalog/schema.rs`); `Catalog::enrich_model` is extended to write it, and `daemon::downloader` populates it from the CivitAI metadata fetch that already happens.

---

## Daemon-side broadcast mechanism

```rust
// src/daemon/events.rs (new)
#[derive(Clone, Debug)]
pub enum Event {
    ProgressTick,
    QueueChanged,
    CatalogChanged,
    UpdatesChanged,
}

pub type EventBus = tokio::sync::broadcast::Sender<Event>;
```

- `EventBus` is constructed in `daemon::run` and cloned into every emitter:
  - `queue::run` — emits `QueueChanged` on transitions; emits `ProgressTick` from a 250 ms interval gated on "any active jobs".
  - `downloader::download_to_file` — emits `QueueChanged` on completion / failure (progress is covered by the tick).
  - `updater::run` — emits `UpdatesChanged` after each poll cycle.
  - IPC handlers that mutate the catalog (`AddDownload`, `DeleteModel`, `RedownloadMissing`, `DownloadVersion`, `Cancel`) — emit `CatalogChanged` on success.
- `IpcServer` accepts a `Subscribe` request and:
  1. Calls `bus.subscribe()` to get a `Receiver<Event>`.
  2. Sends `Frame::Subscribed`, then an immediate `Frame::Snapshot`.
  3. Loops: `recv()` an event → coalesce events that arrive within a 10 ms window → rebuild snapshot → send `Frame::Snapshot`. The `catalog_dirty` / `updates_dirty` flags are set per-subscriber based on which event types fired since the last snapshot, then reset.
- Backpressure: on `RecvError::Lagged`, send one fresh full snapshot and continue. Channel capacity is 256 events.
- Catalog write methods (`insert_model`, `delete_model`, `enrich_model`, `set_status`, `requeue_done`, etc.) take `&EventBus` and emit `CatalogChanged` on success. Because the parameter is required by the type signature, the compiler enforces emission at every call site — there's no way to mutate the catalogue without also notifying subscribers.

---

## TUI client structure (`src/tui/`)

```
src/tui/
├── mod.rs           pub async fn run(config: Config) -> Result<()>
├── app.rs           App state, event reducer
├── ipc.rs           snapshot stream task + action helpers
├── input.rs         crossterm key → AppEvent translation
└── ui/
    ├── mod.rs       top-level layout (tabs + status bar)
    ├── queue.rs     Queue pane render
    ├── catalog.rs   Catalog pane render
    ├── updates.rs   Updates pane render
    └── modal.rs     confirm / text-input / help / error modals
```

### State

```rust
pub struct App {
    pub tab: Tab,                    // Queue | Catalog | Updates
    pub queue: QueuePane,            // active+queued from latest snapshot, selection cursor
    pub catalog: CatalogPane,        // models + search filter + cursor
    pub updates: UpdatesPane,        // updates list + cursor
    pub modal: Option<Modal>,        // Confirm | AddDownload | Help | Error
    pub connection: ConnState,       // Connected | Reconnecting { attempt, next_at }
    pub last_seq: Option<u64>,
    pub free_bytes: u64,
    pub should_quit: bool,
}
```

### Event loop

```rust
let (tx, mut rx) = mpsc::channel::<AppEvent>(64);

spawn(ipc::snapshot_task(socket_path.clone(), tx.clone()));
spawn(input::input_task(tx.clone()));

let mut app = App::default();
let mut term = ratatui::init();
loop {
    if let Some(ev) = rx.recv().await {
        let action = app.handle(ev);            // pure reducer
        if let Some(a) = action {
            spawn(ipc::dispatch_action(socket_path.clone(), a, tx.clone()));
        }
        term.draw(|f| ui::draw(f, &app))?;
        if app.should_quit { break; }
    }
}
```

### Actions and events

```rust
enum AppAction {
    Cancel(Uuid),
    Delete(Uuid),
    Retry(Uuid),
    Add { url: String, model_type: Option<String> },
    DownloadVersion { model_id: u64, version_id: u64 },
    CheckUpdates,
    RefetchCatalog,
    RefetchUpdates,
}

enum AppEvent {
    Snapshot(SnapshotFrame),
    StreamError(String),
    StreamReconnecting { attempt: u32 },
    Key(KeyEvent),
    Tick,                            // 1 Hz; drives reconnect timer + UI animations
    CatalogLoaded(Vec<EnrichedModel>),
    UpdatesLoaded(Vec<UpdateInfo>),
    ActionResult { action_id: ActionId, outcome: Result<(), String> },
}
```

- The reducer (`App::handle`) never blocks; it produces an `AppAction` which `dispatch_action` performs over a fresh `IpcClient` connection. The result returns as `AppEvent::ActionResult` and surfaces errors via `Modal::Error`.
- When a snapshot arrives with `catalog_dirty: true`, the reducer schedules `AppAction::RefetchCatalog`. Same for updates. Initial load happens on first pane entry. Manual `r` triggers an unconditional refetch.
- ID resolution is unnecessary inside the TUI — full UUIDs are always available from the snapshot or the enriched list.

---

## Pane behavior & key bindings

### Global

- `1` / `2` / `3` or `Tab` / `Shift-Tab` — switch tabs
- `?` — help overlay (lists keys for the current tab)
- `q` / `Esc` — close modal; if no modal, quit
- `Q` — force quit
- `r` — refresh current pane (catalog / updates re-fetch; no-op on Queue, which is already live)

### Queue pane

```
┌─ Queue ─ Catalog ─ Updates ─────────────────────────────┐
│ Active                                                   │
│ ▶ Pony Realism — v15 better_hands         [LORA]        │
│   [████████████████░░░░░░░░░░░░] 53%  712 MiB / 1.3 GiB │
│   ETA 2m 14s  (5.4 MiB/s)                               │
│   → SDXL/pony_realism_v15.safetensors                   │
│                                                          │
│ Queued (3)                                              │
│   Smooth Anime — v3 stable                [LORA]        │
│   Epic Realism — v5 sd1.5_finetune        [CKPT]        │
│   https://civitai.com/models/12345        [LORA]        │
└──────────────────────────────────────────────────────────┘
 a:add  c:cancel  r:retry  d:delete  Enter:details  ?:help
```

- Single scrollable list with an "Active" / "Queued" separator.
- `j` / `k` or `↑` / `↓` — move cursor
- `a` — open `AddDownload` modal (URL input + optional model type)
- `c` — confirm-then-cancel selected job
- `r` — re-queue if selected job is failed/cancelled (otherwise no-op)
- `d` — delete selected job from catalog (only valid for terminal-state jobs)
- `Enter` — details overlay (paths, sha256, error if failed)

### Catalog pane

```
│ ▶ Pony Realism — v12 better_hands  [LORA]  2.1 GiB  SDXL/pony_realism_v12.safetensors
│   Smooth Anime — v3 stable         [LORA]  1.8 GiB  SD1.5/smooth_anime_v3.safetensors
```

- `/` — focus search bar; live filter on `model_name`, `version_name`, and `dest_path`
- `Enter` — details overlay (model_id, version_id, base_model, sha256, file_size, preview path)
- `D` (capital) — confirm-then-delete model + file
- `R` — re-queue if file is missing on disk

### Updates pane

```
│ ▶ Pony Realism — v12 → v15 (better_hands)        [LORA]
│   Epic Realism — v3 → v5  (sd1.5_finetune)       [CKPT]
```

- `Enter` — enqueue `DownloadVersion { model_id, version_id }`
- `R` — fire `CheckUpdates`; pane refreshes via `updates_dirty`

### Display rules

- IDs are not displayed; selection is by cursor.
- Path display shows the last two components of `dest_path` (e.g. `~/comfyui/models/loras/SDXL/pony_realism_v15.safetensors` → `SDXL/pony_realism_v15.safetensors`). The model-type subdir is dropped because the `[LORA]/[CKPT]` tag already conveys it.
- Job rows show `<model_name> — <version_name>` when both are present, falling back to URL when metadata isn't yet populated.

### Modals

- **Confirm** — inline `[Y/n]` at the bottom of the screen, blocks input until answered.
- **AddDownload** — centered modal; `Tab` cycles URL → model_type → submit; `Esc` cancels.
- **Error** — shows a failed `ActionResult` message; dismissed with any key.
- **Help** — `?`-toggled overlay listing keys for the current tab.

---

## Error handling and edge cases

- **Snapshot stream disconnect.** `ipc::snapshot_task` reconnects with exponential backoff (250 ms → 500 ms → 1 s → 2 s → cap 5 s), emitting `StreamReconnecting { attempt }`. A top-bar banner shows `● Disconnected — reconnecting (attempt N)…` while in `Reconnecting` state. On reconnect the daemon immediately sends a fresh snapshot. During disconnection, the Queue pane is dimmed so stale data isn't mistaken for live.
- **Action failures.** Dispatch tasks always return `AppEvent::ActionResult`; failures open `Modal::Error`. No silent swallowing.
- **Daemon never started.** If the initial `Subscribe` connect fails, the TUI starts in `Reconnecting` mode rather than crashing.
- **Lagged broadcast receiver.** Server side handles `RecvError::Lagged` by sending one fresh snapshot.
- **Terminal resize.** ratatui handles redraw; cursors are clamped on render.
- **Panic safety.** `ratatui::init()` installs a panic hook that restores the terminal.
- **Concurrent actions.** Each action carries an opaque `ActionId`; results are matched by ID, so order doesn't matter.
- **Empty states.** Each pane has a centered empty-state message ("No downloads. Press `a` to add one.", "Catalog is empty.", "All models are up to date.").
- **TTY detection.** The `Tui` subcommand checks `std::io::stdout().is_terminal()`; if false, it exits with an error.

---

## Testing strategy

- **Pure-reducer unit tests** for `App::handle`: tab switching, cursor bounds, modal transitions, snapshot application, dirty-flag follow-up actions, disconnect/reconnect transitions, action-result handling.
- **Helper unit tests** for path trimming and catalog search filter.
- **IPC integration tests** with a real `IpcServer`, in-memory catalog, and `EventBus`: confirm `Subscribe` produces the expected snapshot sequence in response to mutations, that `catalog_dirty` / `updates_dirty` flags toggle correctly, and that `Lagged` recovery sends a fresh snapshot.
- **No render-layer tests.** Visuals are validated by hand; reducer/state tests cover correctness.

### Manual smoke checklist

- Add a download via TUI → appears in Queue pane.
- Cancel active download → confirm prompt → job moves to terminal state, removed from active.
- Kill daemon while TUI running → banner appears, reconnects when daemon restarts.
- Catalog search filters live as you type.
- Trigger update check → updates pane refreshes.
- Resize terminal small → no panic, layout adapts.
- Pipe TUI stdout → error message, no escape-code vomit.

---

## Dependencies

- `ratatui` — TUI rendering.
- `crossterm` — terminal backend for ratatui. Added as a direct dep so the TUI can drive raw mode and the event stream.
- `tokio-stream` — to expose `IpcSubscriber` as a `Stream<Item = Result<Frame>>`.

No new runtime services. SQLite, reqwest, tokio, etc. are unchanged.

---

## Open follow-ups (out of scope for v1)

- Queue priority / reorder — would add a `priority INT` column and a `SetPriority` IPC variant.
- Image previews using kitty / iTerm graphics protocols.
- Diff view of CivitAI changelog between versions on the Updates pane.
- Multi-selection in Catalog for batch delete.
