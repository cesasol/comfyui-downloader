# comfyui-downloader

A Rust daemon that downloads, catalogs, and manages AI models from CivitAI and HuggingFace into a directory structure compatible with ComfyUI.

## Overview

`comfyui-downloader` runs as a SystemD user service on GNU/Linux. It exposes a Unix socket IPC interface so a companion CLI tool can enqueue downloads, manage models, review available updates, and
configure the daemon — all without requiring root privileges.

## Features

- **Download queue** — enqueue model downloads; the daemon processes them with configurable concurrency (default: 1)
- **HuggingFace downloads** — enqueue any HuggingFace file URL; the subdirectory is derived from the path inside the repo, and the LFS SHA-256 is verified after download (a token is only needed for
  gated repos)
- **ComfyUI template picker** — `comfyui-dl templates` browses the official ComfyUI workflow templates, filters them by generation type, task, or model family, and queues one, many, or all of them
  with their complete model dependency set
- **VRAM feasibility tiers** — detects the local GPU and classifies every template as fitting in VRAM, needing CPU offload for the text encoders and VAE, or unable to run at all (hidden by default)
- **Download resume** — resumes interrupted downloads using HTTP range requests when the server supports it
- **Metadata sidecars** — writes a `.metadata.json` file alongside each downloaded model containing the SHA-256 hash, CivitAI API response, base model, preview path, and more
- **Preview images** — downloads and saves the CivitAI preview image (`model.preview.jpg/webp`) next to each model file
- **Startup scanner** — on daemon start, scans the models directory for existing files missing metadata or preview images and fetches them from CivitAI using SHA-256 hash lookup; registers discovered
  models in the catalog for update tracking
- **Duplicate detection** — skips the download if the target file already exists on disk
- **Update notifications** — periodically polls CivitAI for newer versions of tracked models (once per model every 24 hours) and flags them in the database; updates are never auto-downloaded, giving
  you full control over which versions to install
- **Smart model routing** — automatically places checkpoint models in the correct ComfyUI subdirectory by inspecting the safetensors file header for bundled VAE/CLIP components; GGUF checkpoints are
  always routed to `diffusion_models/`
- **Early access filtering** — skips EarlyAccess model versions by default (configurable)
- **Checksum verification** — validates SHA-256 hashes reported by CivitAI after each download
- **Retry logic** — handles CivitAI rate-limit responses (HTTP 429) with exponential backoff
- **Disk space guard** — checks available disk space before starting a download
- **Desktop notifications** — emits libnotify notifications on download completion, errors, and available updates; progress notifications every 10%
- **Model management** — list downloaded models, delete models (removes files and catalog entry), and relocate misplaced files during update checks
- **IPC interface** — Unix domain socket with a simple JSON protocol for daemon ↔ CLI communication
- **CLI client** — `comfyui-dl` command for all daemon interactions
- **SystemD integration** — ships a `.service` unit file for `systemctl --user`

## Quickstart — Template Workflow

Get a working ComfyUI workflow downloaded and ready to use in about five minutes.

### 1. Install and start the daemon

```sh
# Arch Linux (AUR)
makepkg -si

# Or manual build
cargo build --release -p comfyui-downloader
cp target/release/comfyui-downloader ~/.local/bin/
cp target/release/comfyui-dl ~/.local/bin/
cp systemd/comfyui-downloader-user.service ~/.config/systemd/user/comfyui-downloader.service
systemctl --user daemon-reload
systemctl --user enable --now comfyui-downloader
```

### 2. Set your CivitAI API key

The daemon needs a CivitAI key to resolve model URLs. You only need a free account.

```sh
comfyui-dl set-key <your-api-key>
```

### 3. Browse available templates

The template picker fetches the official ComfyUI workflow catalog, resolves every model dependency, and filters by what your GPU can run:

```sh
# List all templates that fit on this GPU
comfyui-dl templates

# Filter by type
comfyui-dl templates --type image
comfyui-dl templates --type video

# Free-text search
comfyui-dl templates "upscale"
```

Templates are tagged with a VRAM tier: **fits in VRAM**, **needs CPU offload**, or **will not run** (hidden by default). Pass `--comfortable-only` to show only templates that run entirely on the GPU.

### 4. Queue a template

The interactive picker starts with **nothing selected**. Move with the arrow keys, toggle a row with **space** or **x**, select every visible match with **a**, clear all selections with **backspace**,
confirm with **Enter**, and cancel with **Esc**:

```sh
# Interactive — picks interactively
comfyui-dl templates "flux"

# Non-interactive — queues everything that matches without prompting
comfyui-dl templates --type image --yes

# Queue a specific template by name
comfyui-dl templates --name "Flux.1 Dev" --yes
```

Queuing a template always downloads its **complete dependency set**: the diffusion model (or checkpoint), text encoders, VAE, LoRAs, and any helper models — each routed to the correct ComfyUI
`models/` subdirectory. Files shared between templates are queued only once.

### 5. Wait for downloads

```sh
comfyui-dl status
```

Active downloads show progress; completed jobs appear in the catalog. The daemon handles retries, resume, and checksum verification automatically.

### 6. Load the workflow in ComfyUI

Once downloads finish, the model files are in your ComfyUI `models/` directory (default: `~/.local/share/comfyui/models/`). The workflow itself ships with ComfyUI — this tool only fetches the weights
it needs. Launch ComfyUI, open **Workflow → Browse Templates**, and pick the template you queued: its nodes now resolve to the downloaded files automatically, with no manual path configuration.

### Full example

```sh
# One-shot: list, pick, download everything for image generation
comfyui-dl set-key your-api-key
systemctl --user start comfyui-downloader
comfyui-dl templates --type image --yes
comfyui-dl status          # watch progress
# ... models land in models/ — open ComfyUI and go
```

## Architecture

```text
comfyui-downloader/
├── src/
│   ├── main.rs           # Daemon binary entry point
│   ├── cli_main.rs       # CLI binary entry point
│   ├── lib.rs            # Library root, re-exports modules
│   ├── config.rs         # Config loading/saving (XDG paths, TOML)
│   ├── safetensor.rs     # Safetensors header parser (VAE/CLIP detection)
│   ├── daemon/
│   │   ├── mod.rs        # Daemon lifecycle, IPC request handler
│   │   ├── queue.rs      # Async download queue (tokio, semaphore-bounded)
│   │   ├── downloader.rs # HTTP streaming, checksum, metadata & preview writing
│   │   ├── scanner.rs    # Startup scanner: hash-lookup for existing model files
│   │   ├── updater.rs    # Periodic update checker (notify-only, no auto-download)
│   │   └── notifier.rs   # libnotify desktop notifications
│   ├── ipc/
│   │   ├── mod.rs        # Re-exports
│   │   ├── protocol.rs   # JSON request/response types
│   │   ├── server.rs     # Unix socket server (daemon side)
│   │   └── client.rs     # Unix socket client (CLI side)
│   ├── civitai/
│   │   ├── mod.rs        # CivitAI API client (retry on 429)
│   │   └── types.rs      # API response types, ModelType → subdir mapping
│   ├── catalog/
│   │   ├── mod.rs        # Model catalog (SQLite via rusqlite)
│   │   └── schema.rs     # DB schema and migrations
│   └── cli/
│       └── mod.rs        # CLI argument parsing and output formatting (clap)
├── systemd/
│   ├── comfyui-downloader.service
│   └── comfyui-downloader-user.service
├── PKGBUILD              # Arch Linux / AUR package
├── Cargo.toml
└── README.md
```

## Directory Layout (ComfyUI models)

Models are saved under a configurable root (default: `$XDG_DATA_HOME/comfyui/models/`) using the path `{type}/{baseModel}/{filename}`:

```text
models/
├── checkpoints/          # Full checkpoints (bundling VAE + CLIP + UNet)
│   └── SDXL 1.0/
│       └── model.safetensors
├── diffusion_models/     # Diffusion-only models (no VAE/CLIP)
│   └── Flux.1 D/
│       ├── model.safetensors
│       ├── model.gguf
│       ├── model.metadata.json
│       └── model.preview.webp
├── loras/
├── vae/
├── controlnet/
├── embeddings/
├── upscale_models/
└── other/                # Fallback for unrecognised model types
```

Model type is inferred from the CivitAI API response. For checkpoint safetensors files, the daemon reads the file header to detect bundled components:

- **VAE present** (`first_stage_model.*` tensors) and/or **CLIP present** (`cond_stage_model.*`, `conditioner.embedders.*` tensors) → `checkpoints/`
- **Neither VAE nor CLIP** → `diffusion_models/`
- **GGUF checkpoints** → always `diffusion_models/` (GGUF never bundles VAE/CLIP)

## Configuration

Configuration is read from `$XDG_CONFIG_HOME/comfyui-downloader/config.toml` (default: `~/.config/comfyui-downloader/config.toml`). The file is created with defaults on first daemon startup.

```toml
[paths]
models_dir = "~/.local/share/comfyui/models"

[daemon]
update_interval_hours = 24
max_concurrent_downloads = 1
socket_path = "/run/user/$UID/comfyui-downloader.sock"
skip_early_access = true  # Skip EarlyAccess model versions when resolving latest

[gpu]
vram_bytes = 0            # Optional override of the detected VRAM capacity
```

### Credentials

API credentials are **not** stored in `config.toml`. They live in the session
keyring behind the freedesktop Secret Service D-Bus interface
(`org.freedesktop.secrets`, implemented by gnome-keyring, KWallet and others),
and are read by the daemon at startup:

```sh
comfyui-dl set-key <your-civitai-api-key>
comfyui-dl set-key --service huggingface <your-hf-token>   # gated repos only
```

Both commands work without a running daemon. A `civitai.api_key` or
`huggingface.token` left in `config.toml` by an older version still works, but
the daemon moves it into the keyring on the next start and removes it from the
file. If no Secret Service is reachable (for example on a headless box without
a keyring daemon), the plaintext fields remain as a fallback.

## CLI Usage

```sh
# Store your CivitAI API key in the system keyring (no daemon needed)
comfyui-dl set-key <your-api-key>

# Store a HuggingFace token for gated repos
comfyui-dl set-key --service huggingface <your-hf-token>

# Add a model by CivitAI URL
comfyui-dl add https://civitai.com/models/12345
comfyui-dl add https://civitai.com/models/12345?modelVersionId=67890

# Add a model by HuggingFace file URL (resolve/ or blob/ links both work).
# The target subdirectory is taken from the path inside the repo
# (split_files/vae/ae.safetensors -> vae/), or from --model-type.
comfyui-dl add https://huggingface.co/Comfy-Org/z_image_turbo/resolve/main/split_files/vae/ae.safetensors
comfyui-dl add https://huggingface.co/org/repo/resolve/main/model.safetensors --model-type diffusion_models

# Show daemon status, active downloads, and free disk space
comfyui-dl status

# List downloaded models in the catalog
comfyui-dl list

# Check for available updates (flags models, does not auto-download)
comfyui-dl check-updates

# View models with available updates
comfyui-dl updates

# Download a specific version of a model
comfyui-dl download-version <model_id> <version_id>

# Cancel a queued or active download by job ID
comfyui-dl cancel <uuid>

# Delete a model by job ID (removes files and catalog entry)
comfyui-dl delete <uuid>
```

### ComfyUI Template Picker

`comfyui-dl templates` fetches the ComfyUI default workflow-template catalog
(`Comfy-Org/workflow_templates`), resolves the model files each template needs
from HuggingFace, and lets you pick one, many, or all of them. Everything shown
is selected by default — press Enter to queue the lot.

Selecting a template always queues its **complete dependency set**: diffusion
model (or checkpoint), text encoders, VAE, LoRAs and any helper models, each
routed to its ComfyUI subdirectory. Files shared between templates are queued
once.

Each template is judged against the detected GPU:

| Tier | Meaning |
|---|---|
| fits in VRAM | All weights plus the activation working set fit on the card |
| needs CPU offload | Fits only with the text encoders and VAE executed on the CPU |
| will not run | The sampler weights alone exceed VRAM — hidden unless `--include-unrunnable` |

```sh
# Everything that can run on this GPU (interactive multi-select; nothing preselected)
comfyui-dl templates

# Filter by generation type: "all video models"
comfyui-dl templates --type video

# Filter by task tag: "all image edit models"
comfyui-dl templates --task "image edit"

# Filter by model family: "all Z-Image-Turbo"
comfyui-dl templates --model z-image-turbo

# Free text, combined filters, and only what fits without offloading
comfyui-dl templates "upscale" --type image --comfortable-only

# Show what cannot run, include cloud-API templates, refresh the cached catalog
comfyui-dl templates --include-unrunnable --include-api --refresh

# Non-interactive: queue every match; --json prints the resolved listing instead
comfyui-dl templates --type audio --yes
comfyui-dl templates --model wan2.2 --json
```

The catalog is cached under `$XDG_CACHE_HOME/comfyui-downloader/templates`
(index for 6 hours, workflows for a week); `--refresh` bypasses it.

### Update Workflow

The daemon periodically checks CivitAI for newer versions of tracked models (rate-limited to once per model every 24 hours). When an update is found, it is flagged in the database and a desktop
notification is sent — but **no automatic download occurs**. This is intentional: CivitAI model "versions" often represent quantizations, different base models, or unrelated variants rather than true
updates.

To review and install updates:

```sh
# See what's available
comfyui-dl updates

# Output:
#   dreamWeaver_fluxDevV2.safetensors  [diffusion_models]
#     version 5550002 → 5550003 (Flux Dev V3)
#     comfyui-dl download-version 990001 5550003

# Explicitly install an update
comfyui-dl download-version 990001 5550003
```

## IPC Protocol

Communication over the Unix socket uses newline-delimited JSON:

| Command | Payload | Description |
|---|---|---|
| `AddDownload` | `{ url, model_type? }` | Enqueue a CivitAI or HuggingFace model URL |
| `AddDownloads` | `{ items: [{ url, model_type? }] }` | Enqueue several files at once (template picker) |
| `ListTemplates` | `{ filter, refresh, include_unrunnable }` | ComfyUI templates with model bundles and VRAM verdicts |
| `ListQueue` | — | Return current queue state |
| `ListModels` | — | Return downloaded models from the catalog |
| `ListModelsEnriched` | — | Return models enriched with sidecar metadata |
| `ListUpdates` | — | Return models with available updates flagged |
| `DownloadVersion` | `{ model_id, version_id }` | Enqueue a specific model version for download |
| `DeleteModel` | `{ id }` | Delete a model by job ID (files + catalog entry) |
| `CheckUpdates` | — | Trigger an immediate update scan |
| `GetStatus` | — | Daemon health, active download progress, free disk space |
| `Cancel` | `{ id }` | Cancel a queued or active download |

## Tech Stack

| Concern | Crate |
|---|---|
| Async runtime | `tokio` |
| HTTP client | `reqwest` |
| CLI parsing | `clap` |
| Serialisation | `serde` / `serde_json` |
| Database | `rusqlite` (SQLite, bundled) |
| Desktop notifications | `notify-rust` |
| Config | `toml` |
| Logging | `tracing` / `tracing-subscriber` |
| Checksum | `sha2` / `hex` |
| Disk space | `libc` (`statvfs`) |
| Job IDs | `uuid` |
| Timestamps | `chrono` |

## Requirements

- GNU/Linux with SystemD (user session)
- `libnotify` (usually pre-installed on desktop distros)
- A CivitAI API key (required for all downloads and metadata lookups)

## Installation

### Arch Linux (AUR)

A `PKGBUILD` is included. To build and install:

```sh
makepkg -si
```

Or use an AUR helper once the package is published.

### Manual

```sh
cargo build --release -p comfyui-downloader
cp target/release/comfyui-downloader ~/.local/bin/
cp target/release/comfyui-dl ~/.local/bin/
cp systemd/comfyui-downloader-user.service ~/.config/systemd/user/comfyui-downloader.service
systemctl --user daemon-reload
systemctl --user enable --now comfyui-downloader
```

## License

GPL-3.0-only — see [LICENSE](LICENSE).
