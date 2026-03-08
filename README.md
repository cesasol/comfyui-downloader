# comfyui-downloader

A Rust daemon that downloads, catalogs, and updates AI models from CivitAI into a directory structure compatible with ComfyUI.

## Overview

`comfyui-downloader` runs as a SystemD user service on GNU/Linux. It exposes a Unix socket IPC interface so a companion CLI tool can enqueue downloads, query status, and trigger update checks — all without requiring root privileges.

## Features

- **Download queue** — enqueue model downloads; the daemon processes them with configurable concurrency (default: 1)
- **Metadata sidecars** — writes a `.metadata.json` file alongside each downloaded model containing the SHA-256 hash, CivitAI API response, base model, preview path, and more
- **Preview images** — downloads and saves the CivitAI preview image (`model.preview.jpg/webp`) next to each model file
- **Startup scanner** — on daemon start, scans the models directory for existing files missing metadata or preview images and fetches them from CivitAI using SHA-256 hash lookup
- **Duplicate detection** — skips the download if the target file already exists on disk
- **Update checks** — periodically polls CivitAI for newer versions of every tracked model
- **Early access filtering** — skips EarlyAccess model versions by default (configurable)
- **Checksum verification** — validates SHA-256 hashes reported by CivitAI after each download
- **Retry logic** — handles CivitAI rate-limit responses (HTTP 429) with exponential backoff
- **Disk space guard** — checks available disk space before starting a download
- **Desktop notifications** — emits libnotify notifications on download completion, errors, and available updates; progress notifications every 10%
- **IPC interface** — Unix domain socket with a simple JSON protocol for daemon ↔ CLI communication
- **CLI client** — `comfyui-dl` command to add downloads, check status, trigger update checks, cancel jobs, and configure the API key
- **SystemD integration** — ships a `.service` unit file for `systemctl --user`

## Planned Features

- ZFS snapshot integration before and after bulk downloads
- ComfyUI execution status shown as desktop notifications
- Manage ComfyUI itself as a SystemD sub-daemon
- Execute saved workflow templates with parameter patching

## Architecture

```
comfyui-downloader/
├── src/
│   ├── main.rs           # Daemon binary entry point
│   ├── cli_main.rs       # CLI binary entry point
│   ├── config.rs         # Config loading/saving (XDG paths, TOML)
│   ├── daemon/
│   │   ├── mod.rs        # Daemon lifecycle (init, signal handling, shutdown)
│   │   ├── queue.rs      # Async download queue (tokio, semaphore-bounded)
│   │   ├── downloader.rs # HTTP streaming, checksum, metadata & preview writing
│   │   ├── scanner.rs    # Startup scanner: hash-lookup for existing model files
│   │   ├── updater.rs    # Periodic update checker
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
│       └── mod.rs        # CLI argument parsing (clap)
├── systemd/
│   └── comfyui-downloader.service
├── PKGBUILD              # Arch Linux / AUR package
├── Cargo.toml
└── README.md
```

## Directory Layout (ComfyUI models)

Models are saved under a configurable root (default: `$XDG_DATA_HOME/comfyui/models/`) using the path `{type}/{baseModel}/{filename}`:

```
models/
├── checkpoints/          # Full-precision checkpoints
│   └── SDXL 1.0/
│       └── model.safetensors
├── diffusion_models/     # Pruned Flux checkpoints (GGUF / fp8 / etc.)
│   └── Flux.1 D/
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

Model type and base model are inferred from the CivitAI API response. Flux checkpoints with `metadata.size == "pruned"` are routed to `diffusion_models/` instead of `checkpoints/`.

## Configuration

Configuration is read from `$XDG_CONFIG_HOME/comfyui-downloader/config.toml` (default: `~/.config/comfyui-downloader/config.toml`). The file is created with defaults on first daemon startup.

```toml
[civitai]
api_key = ""              # CivitAI API key (required)

[paths]
models_dir = "~/.local/share/comfyui/models"

[daemon]
update_interval_hours = 24
max_concurrent_downloads = 1
socket_path = "/run/user/$UID/comfyui-downloader.sock"
skip_early_access = true  # Skip EarlyAccess model versions when resolving latest
```

The API key can also be set without editing the file manually:

```sh
comfyui-dl set-key <your-api-key>
```

## IPC Protocol

Communication over the Unix socket uses newline-delimited JSON:

| Command | Payload | Description |
|---|---|---|
| `AddDownload` | `{ url, model_type? }` | Enqueue a CivitAI model URL |
| `ListQueue` | — | Return current queue state |
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
cargo build --release
cp target/release/comfyui-downloader ~/.local/bin/
cp target/release/comfyui-dl ~/.local/bin/
cp systemd/comfyui-downloader.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now comfyui-downloader
```

## CLI Usage

```sh
# Set your CivitAI API key (writes to config file, no daemon needed)
comfyui-dl set-key <your-api-key>

# Add a model by CivitAI URL
comfyui-dl add https://civitai.com/models/12345
comfyui-dl add https://civitai.com/models/12345?modelVersionId=67890

# Show daemon status, active downloads, and free disk space
comfyui-dl status

# Trigger an immediate update check
comfyui-dl check-updates

# Cancel a queued or active download by job ID
comfyui-dl cancel <uuid>
```

## License

GPL-3.0-only — see [LICENSE](LICENSE).
