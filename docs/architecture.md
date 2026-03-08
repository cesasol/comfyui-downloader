# Architecture

`comfyui-downloader` is a GNU/Linux user-space daemon that downloads AI models from CivitAI into a ComfyUI-compatible directory layout. It is built as two separate binaries that communicate through a Unix domain socket.

---

## Binaries

| Binary | Entry point | Role |
|---|---|---|
| `comfyui-downloader` | `src/main.rs` | Long-running daemon |
| `comfyui-dl` | `src/cli_main.rs` | One-shot CLI client |

The CLI binary does **not** share in-process state with the daemon. It connects to the socket, sends a single JSON request, prints the response, and exits. All business logic lives in the daemon.

---

## Daemon startup sequence

`daemon::run()` (`src/daemon/mod.rs`):

1. Load `Config` from TOML (`src/config.rs`).
2. Open SQLite catalog at `~/.local/share/comfyui-downloader/catalog.db` and run schema migrations.
3. Construct a `CivitaiClient` wrapping a `reqwest::Client`.
4. Spawn **queue task** (`daemon::queue::run`) — processes pending download jobs.
5. Spawn **updater task** (`daemon::updater::run`) — periodically checks for newer model versions.
6. Bind the Unix socket and enter the IPC accept loop.

The three shared resources (`Config`, `Catalog`, `CivitaiClient`) are wrapped in `Arc` and cloned into each task. `Catalog` additionally uses `tokio::sync::Mutex` because the lock is held across `.await` points.

---

## IPC layer (`src/ipc/`)

Communication between the CLI and daemon uses a Unix domain socket at `/run/user/<UID>/comfyui-downloader.sock`. The wire format is **newline-delimited JSON** — one JSON object per line in each direction.

### Protocol (`protocol.rs`)

```
// Request (CLI → daemon)
{"cmd": "<variant>", "payload": {...}}

// Response (daemon → CLI)
{"status": "ok" | "err", "data": ...}
```

`Request` is a Serde-tagged enum with `snake_case` variant names:

| `cmd` | Payload fields | Description |
|---|---|---|
| `add_download` | `url`, `model_type?` | Enqueue a CivitAI URL |
| `list_queue` | — | Return all jobs from the catalog |
| `check_updates` | — | Trigger an immediate update scan |
| `get_status` | — | Daemon health ping |
| `cancel` | `id` (UUID) | Set a job to `cancelled` |

### Server (`server.rs`)

`IpcServer::serve` accepts connections in a `tokio::spawn` loop. Each connection gets its own task that reads lines, deserialises each into a `Request`, calls the handler closure, serialises the `Response`, and writes it back. Stale socket files are deleted on bind.

### Client (`client.rs`)

`IpcClient::send` writes one request line and reads one response line. The connection is closed when the `IpcClient` is dropped.

---

## Catalog (`src/catalog/`)

A SQLite database accessed through `rusqlite` (compiled with the `bundled` feature — no system SQLite dependency required).

### Schema (`schema.rs`)

```sql
CREATE TABLE jobs (
    id          TEXT PRIMARY KEY,   -- UUID v4 as string
    url         TEXT NOT NULL,
    model_id    INTEGER,            -- CivitAI model ID (resolved later)
    version_id  INTEGER,            -- CivitAI version ID (resolved later)
    model_type  TEXT,               -- subdirectory name, e.g. "loras"
    dest_path   TEXT,               -- absolute path after download
    status      TEXT NOT NULL,      -- see JobStatus below
    created_at  TEXT NOT NULL,      -- RFC 3339
    updated_at  TEXT NOT NULL,
    error       TEXT
);
```

WAL mode is enabled; an index on `status` speeds up `next_queued()`.

### `JobStatus` lifecycle

```
Queued → Downloading → Verifying → Done
                   ↘ Failed
        (any state) → Cancelled
```

### Key catalog methods

| Method | SQL | Used by |
|---|---|---|
| `enqueue` | `INSERT` | IPC `add_download` handler |
| `next_queued` | `SELECT … WHERE status='queued' ORDER BY created_at ASC LIMIT 1` | queue task |
| `set_status` | `UPDATE jobs SET status=…` | queue task, IPC `cancel` handler |
| `list_jobs` | `SELECT … ORDER BY created_at DESC` | IPC `list_queue` handler |

---

## Download pipeline (`src/daemon/`)

### Queue task (`queue.rs`)

Polls `catalog.next_queued()` in a tight loop (sleeping 5 s when idle). For each job:

1. Sets status → `Downloading`.
2. Calls `downloader::download`.
3. On success: sets status → `Done`, fires `notify_success`.
4. On failure: sets status → `Failed` with error text, fires `notify_error`.

Currently processes one job at a time (concurrency enforcement is a TODO).

### Downloader (`downloader.rs`)

1. Resolves the destination directory: `models_dir / model_type` (falls back to `other`).
2. Checks available disk space via `libc::statvfs` — aborts if < 1 GiB free.
3. Sends a `GET` request with optional `Bearer` auth; streams bytes to a `.tmp` file while computing a running SHA-256 hash.
4. On stream completion, logs the digest and atomically renames `.tmp` → final path.

Filename is derived from the `Content-Disposition` response header (`filename=` parameter) or the last URL path segment.

> **Note:** Checksum comparison against the CivitAI-reported hash is implemented as a TODO — the hash is computed but not yet validated.

### Updater task (`updater.rs`)

Sleeps for `update_interval_hours` between runs. On each run, iterates all `Done` jobs that have a `version_id`, compares against the CivitAI API to find newer versions, and would enqueue them.

> **Note:** The comparison logic is a stub — the structure is in place but API calls are not yet wired up.

### Notifier (`notifier.rs`)

Thin wrapper around `notify-rust`. Three notification types:

| Function | Icon | Trigger |
|---|---|---|
| `notify_success` | `dialog-information` | Download complete |
| `notify_error` | `dialog-error` | Download or update failure |
| `notify_update_available` | `software-update-available` | New model version found |

---

## CivitAI client (`src/civitai/`)

`CivitaiClient` wraps a `reqwest::Client` (30 s timeout). All JSON requests go through `get_json`, which retries on HTTP 429 with exponential backoff (delay = 2^attempt seconds, capped at 2^6 = 64 s).

### API endpoints used

| Method | URL | Returns |
|---|---|---|
| `get_model(id)` | `/api/v1/models/{id}` | `ModelInfo` (name, type, versions list) |
| `get_model_version(id)` | `/api/v1/model-versions/{id}` | `ModelVersion` (download URL, file hashes) |

### CivitAI model type → ComfyUI directory mapping (`types.rs`)

| `ModelType` | `models_subdir()` |
|---|---|
| `Checkpoint` | `checkpoints` |
| `LORA` / `LoCon` | `loras` |
| `Controlnet` | `controlnet` |
| `Vae` | `vae` |
| `Embedding` (TextualInversion) | `embeddings` |
| `Upscaler` | `upscale_models` |
| anything else | `other` |

---

## Configuration (`src/config.rs`)

Read from `~/.config/comfyui-downloader/config.toml`; falls back to built-in defaults if the file is absent. All three sections (`[civitai]`, `[paths]`, `[daemon]`) are optional.

| Key | Default |
|---|---|
| `civitai.api_key` | `None` |
| `paths.models_dir` | `~/.local/share/comfyui/models` |
| `daemon.update_interval_hours` | `24` |
| `daemon.max_concurrent_downloads` | `2` |
| `daemon.socket_path` | `/run/user/<UID>/comfyui-downloader.sock` |

---

## SystemD integration

The unit file (`systemd/comfyui-downloader.service`) is a **user service** (`WantedBy=default.target`). It starts after `network-online.target`, sets `RUST_LOG=info`, and restarts on failure with a 5 s back-off. The binary is expected at `~/.local/bin/comfyui-downloader`.
