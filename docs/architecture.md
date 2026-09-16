# Architecture

`comfyui-downloader` is a GNU/Linux user-space daemon that downloads AI models from CivitAI into a ComfyUI-compatible directory layout. It is built as two separate binaries that communicate through a
Unix domain socket.

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

1. Load `Config` from TOML (`src/config.rs`); immediately re-save it to sync any new fields added since last run.
2. Open SQLite catalog at `$XDG_DATA_HOME/comfyui-downloader/catalog.db` and run schema migrations.
3. Construct a `CivitaiClient` wrapping a `reqwest::Client`.
4. Spawn **scanner task** (`daemon::scanner::run`) — walks existing model files and backfills missing metadata/preview images.
5. Spawn **queue task** (`daemon::queue::run`) — processes pending download jobs.
6. Spawn **updater task** (`daemon::updater::run`) — periodically checks for newer model versions.
7. Bind the Unix socket and enter the IPC accept loop.

The three shared resources (`Config`, `Catalog`, `CivitaiClient`) are wrapped in `Arc` and cloned into each task. `Catalog` additionally uses `tokio::sync::Mutex` because the lock is held across
`.await` points.

---

## IPC layer (`src/ipc/`)

Communication between the CLI and daemon uses a Unix domain socket at `/run/user/<UID>/comfyui-downloader.sock`. The wire format is **newline-delimited JSON** — one JSON object per line in each
direction.

### Protocol (`protocol.rs`)

```text
// Request (CLI → daemon)
{"cmd": "<variant>", "payload": {...}}

// Response (daemon → CLI)
{"status": "ok" | "err", "data": ...}
```

`Request` is a Serde-tagged enum with `snake_case` variant names:

| `cmd` | Payload fields | Description |
|---|---|---|
| `add_download` | `url`, `model_type?` | Enqueue a CivitAI or HuggingFace URL |
| `add_downloads` | `items[]` of `{url, model_type?}` | Enqueue several files at once (template picker) |
| `list_templates` | `filter`, `refresh`, `include_unrunnable` | ComfyUI template bundles with VRAM verdicts |
| `list_queue` | — | Return all jobs from the catalog |
| `check_updates` | — | Trigger an immediate update scan |
| `get_status` | — | Queue length, active progress, free disk bytes |
| `cancel` | `id` (UUID) | Cancel a queued or active download |

### Server (`server.rs`)

`IpcServer::serve` accepts connections in a `tokio::spawn` loop. Each connection gets its own task that reads lines until EOF, deserialises each into a `Request`, calls the handler closure, serialises
the `Response`, and writes it back — one connection therefore carries as many exchanges as the client sends, which the CLI depends on (`add` resolves version info first, `delete`/`cancel` resolve an
ID prefix, the template picker lists before queueing). Stale socket files are deleted on bind.

---

## Template catalog (`src/templates/`)

The ComfyUI default workflow templates are published in `Comfy-Org/workflow_templates`. `TemplateCatalog` fetches `templates/index.json` (categories → templates with title, tags, model families and
total size) and each matching template's workflow JSON, caching both under `$XDG_CACHE_HOME/comfyui-downloader/templates` (index 6 h, workflows 7 days).

Model files are extracted from two sources and merged, deduplicated by URL:

1. `nodes[].properties.models` — `{name, url, directory}`, authoritative for the target subdirectory.
2. The `MarkdownNote` "Model links" section — `**role**` headings followed by `- [file](url)` links, used by templates that predate the node field. Role headings are normalised (`"Diffusion model"` →
   `diffusion_models`), and non-HuggingFace links (input assets, documentation) are discarded.

File sizes and checksums come from one HuggingFace tree request per `(repo, revision, directory)` group, covering every file the templates reference there.

### VRAM feasibility (`src/vram.rs`, `src/gpu.rs`)

`gpu::detect_gpus` reads `mem_info_vram_total` for each `/sys/class/drm/card*` device (falling back to `nvidia-smi`), and `config.gpu.vram_bytes` overrides the result. A bundle's weights are split
into GPU-resident roles (checkpoints, diffusion models, LoRAs, ControlNets) and offloadable roles (text encoders, VAE, CLIP vision, upscalers), then classified:

| Tier | Condition |
|---|---|
| `comfortable` | all weights × overhead + activation headroom + driver reserve ≤ VRAM |
| `cpu_offload` | only the GPU-resident half fits under the same budget |
| `wont_run` | even the GPU-resident half does not fit |

Activation headroom depends on the generation type (video reserves more than image, audio and LLM the least).

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
    model_id    INTEGER,            -- CivitAI model ID (resolved from URL)
    version_id  INTEGER,            -- CivitAI version ID (resolved from URL)
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

```text
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

A `Semaphore` with `max_concurrent_downloads` permits bounds concurrency. The loop acquires a permit before polling `catalog.next_queued()` (sleeping 5 s when idle). For each job:

1. Sets status → `Downloading`.
2. Creates a `CancellationToken` and registers it in `ActiveTasks`.
3. Spawns a task that calls `downloader::download`.
4. On success: persists `dest_path` and resolved `model_type`, sets status → `Done`, fires `notify_success`.
5. On cancellation: sets status → `Cancelled`.
6. On failure: sets status → `Failed` with error text, fires `notify_error`.

The semaphore permit is held by the spawned task and released automatically when it finishes, allowing the next job to start.

### Downloader (`downloader.rs`)

**Source routing**: A job whose URL parses as a HuggingFace file reference (`src/huggingface`) is resolved against the HuggingFace tree API instead of CivitAI: the download URL is deterministic, the
expected SHA-256 comes from the LFS object ID, and the target subdirectory comes from the job's `model_type` or from the file's directory inside the repo (`split_files/vae/ae.safetensors` → `vae`).
HuggingFace downloads need no CivitAI key; the optional `huggingface.token` is sent only when configured, and a 401/403 reports the repo as possibly gated.

**API resolution** (`resolve_version`): For CivitAI jobs, the downloader calls the CivitAI API to obtain the authoritative download URL, expected SHA-256 hash, model type subdirectory, base model
name, and preview image URL. Three resolution paths:

- Both `model_id` + `version_id` known → parallel `get_model` + `get_model_version` calls.
- Only `version_id` → single `get_model_version` call.
- Only `model_id` → `get_model` then `get_model_version` for the latest non-EarlyAccess version.
- Neither → falls back to the stored URL with no hash verification.

**Download path**: `models_dir / model_type_subdir / base_model / filename`

- `model_type_subdir` is derived from the CivitAI `ModelType` (see mapping table below).
- `base_model` is the CivitAI base model string (e.g. `"Flux.1 D"`), sanitised for filesystem use.
- Spaces are preserved in directory names.

**Download steps**:

1. Check if the target file already exists — skip the entire download if so.
2. Check available disk space via `libc::statvfs` — abort if < 1 GiB free.
3. Stream bytes from the download URL (Bearer auth) to a `.tmp` file, computing a running SHA-256 hash.
4. On stream completion, compare the computed hash against the CivitAI-reported value; delete the `.tmp` file and bail on mismatch.
5. Atomically rename `.tmp` → final path.
6. Write a `.metadata.json` sidecar with file info, SHA-256, base model, and the full CivitAI API response.
7. Download the preview image to `filename.preview.{ext}` alongside the model file.

Progress notifications are updated every 10% via `notify_download_start` / `update_download_progress` / `close_download_notification`.

### Scanner (`scanner.rs`)

Runs once at daemon startup. Walks every known model subdirectory (`checkpoints`, `diffusion_models`, `loras`, `vae`, `controlnet`, `embeddings`, `upscale_models`, `other`) looking for model files
(`.safetensors`, `.gguf`, `.pt`, `.pth`, `.bin`, `.ckpt`) that are missing a `.metadata.json` or `.preview.*` sidecar. For each such file:

1. Computes the SHA-256 hash of the file (blocking task pool).
2. Calls `CivitaiClient::get_model_version_by_hash` to identify the model.
3. Writes missing metadata and/or preview image via `downloader::save_artifacts`.

The scanner skips silently if no API key is configured.

### Updater task (`updater.rs`)

Runs immediately, then sleeps for `update_interval_hours` between runs (can be woken early by the `CheckUpdates` IPC command via a `tokio::sync::Notify`).

On each run, iterates all `Done` jobs that have both a `model_id` and `version_id`. One representative job per `model_id` is checked. For each:

1. Calls `civitai.get_model(model_id)` to fetch the current version list.
2. Compares the latest version ID against the stored `version_id` (newer = larger integer, as CivitAI assigns monotonically increasing IDs).
3. If a newer version exists: calls `catalog.enqueue_version_update`, fires `notify_update_available`.

### Notifier (`notifier.rs`)

Thin wrapper around `notify-rust`:

| Function | Icon | Trigger |
|---|---|---|
| `notify_success` | `dialog-information` | Download complete |
| `notify_error` | `dialog-error` | Download or update failure |
| `notify_update_available` | `software-update-available` | New model version found |
| `notify_download_start` | `document-save` | Download begins (persistent, returns notification ID) |
| `update_download_progress` | `document-save` | Every 10% progress (replaces notification by ID) |
| `close_download_notification` | — | Download finished or cancelled |

---

## CivitAI client (`src/civitai/`)

`CivitaiClient` wraps a `reqwest::Client` (30 s timeout). All JSON requests go through `get_json`, which retries on HTTP 429 with exponential backoff (delay = 2^attempt seconds, capped at 2^6 = 64 s).

### API endpoints used

| Method | URL | Returns |
|---|---|---|
| `get_model(id)` | `/api/v1/models/{id}` | `ModelInfo` (name, type, versions list) |
| `get_model_version(id)` | `/api/v1/model-versions/{id}` | `ModelVersion` (download URL, file hashes, images) |
| `get_model_version_by_hash(sha256)` | `/api/v1/model-versions/by-hash/{sha256}` | `ModelVersion` (used by startup scanner) |

### CivitAI model type → ComfyUI directory mapping (`types.rs`)

`ModelType::models_subdir_for_file` applies extra routing logic for checkpoints:

| `ModelType` | Condition | `models_subdir` |
|---|---|---|
| `Checkpoint` | Flux base model + `metadata.size == "pruned"` | `diffusion_models` |
| `Checkpoint` | all other cases | `checkpoints` |
| `LORA` / `LoCon` | — | `loras` |
| `Controlnet` | — | `controlnet` |
| `Vae` | — | `vae` |
| `Embedding` (TextualInversion) | — | `embeddings` |
| `Upscaler` | — | `upscale_models` |
| anything else | — | `other` |

---

## Configuration (`src/config.rs`)

Read from `$XDG_CONFIG_HOME/comfyui-downloader/config.toml` (default: `~/.config/comfyui-downloader/config.toml`). Falls back to built-in defaults if the file is absent; the daemon re-saves the file
on startup to persist any newly added default fields.

| Key | Default |
|---|---|
| `paths.models_dir` | `$XDG_DATA_HOME/comfyui/models` |
| `daemon.update_interval_hours` | `24` |
| `daemon.max_concurrent_downloads` | `1` |
| `daemon.socket_path` | `/run/user/<UID>/comfyui-downloader.sock` |
| `daemon.skip_early_access` | `true` |

XDG base directories are read from `$XDG_CONFIG_HOME` / `$XDG_DATA_HOME`, falling back to `$HOME/.config` / `$HOME/.local/share`.

The deprecated `civitai.api_key` and `huggingface.token` fields still deserialise, but they are not where credentials belong (see below).

---

## Credentials (`src/secrets.rs`)

Secrets are kept in the freedesktop Secret Service over D-Bus (`org.freedesktop.secrets`: gnome-keyring, KWallet, ...) rather than in `config.toml`. `Store::open` negotiates one encrypted session and
is reused for every credential; items are addressed by the attributes `application=comfyui-downloader` and `credential=civitai-api-key` / `huggingface-token`, so labels are cosmetic.

`Config::resolve_credentials` runs once at daemon startup and fills `Config::secrets`, a `#[serde(skip)]` field that therefore can never be written back to disk. Resolution order per credential:

1. keyring item, if present
2. otherwise the plaintext config field, which is written into the keyring and stripped from `config.toml`
3. if the Secret Service is unreachable, the plaintext field is used as-is and a warning is logged

Runtime code never touches the config fields directly; it calls `Config::civitai_api_key()` / `Config::huggingface_token()`, which prefer the resolved secret and treat a blank value as unset.
`comfyui-dl set-key [--service civitai|huggingface] <VALUE>` writes to the keyring without a daemon connection and clears any plaintext leftover.

---

## SystemD integration

The packaged unit file (`systemd/comfyui-downloader.service`) is a **user service** (`WantedBy=default.target`). It starts after `network-online.target`, sets `RUST_LOG=info`, and restarts on failure
with a 5 s back-off. The packaged binary must be on `$PATH` because this unit uses a bare `ExecStart=comfyui-downloader`. Per-user installs use `systemd/comfyui-downloader-user.service`, installed as
`comfyui-downloader.service`, with `ExecStart=%h/.local/bin/comfyui-downloader` so the unit does not depend on systemd's user-service PATH.
