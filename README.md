# comfyui-downloader

A Rust daemon that downloads, catalogs, and updates AI models from CivitAI into a directory structure compatible with ComfyUI.

## Overview

`comfyui-downloader` runs as a SystemD user service on GNU/Linux. It exposes a Unix socket IPC interface so a companion CLI tool can enqueue downloads, query status, and trigger update checks — all without requiring root privileges.

## Features

- **Download queue** — enqueue model downloads; the daemon processes them sequentially with configurable concurrency
- **Update checks** — periodically polls CivitAI for newer versions of every tracked model
- **Checksum verification** — validates SHA-256 hashes reported by CivitAI after each download
- **Retry logic** — handles CivitAI rate-limit responses (HTTP 429) and transient network failures with exponential backoff
- **Disk space guard** — checks available disk space before starting a download
- **Desktop notifications** — emits libnotify notifications on download completion, errors, and available updates
- **IPC interface** — Unix domain socket with a simple JSON protocol for daemon ↔ CLI communication
- **CLI client** — `comfyui-dl` command to add downloads, list queue status, trigger update checks, and more
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
│   ├── main.rs           # Binary entry point; starts daemon or delegates to CLI
│   ├── daemon/
│   │   ├── mod.rs        # Daemon lifecycle (init, signal handling, shutdown)
│   │   ├── queue.rs      # Async download queue (tokio)
│   │   ├── downloader.rs # HTTP download logic, resume support, checksum
│   │   ├── updater.rs    # Periodic update checker
│   │   └── notifier.rs   # libnotify desktop notifications
│   ├── ipc/
│   │   ├── mod.rs        # Unix socket server/client
│   │   └── protocol.rs   # JSON request/response types
│   ├── civitai/
│   │   ├── mod.rs        # CivitAI API client
│   │   └── types.rs      # API response types
│   ├── catalog/
│   │   ├── mod.rs        # Model catalog (SQLite via rusqlite)
│   │   └── schema.rs     # DB schema and migrations
│   └── cli/
│       └── mod.rs        # CLI argument parsing (clap)
├── systemd/
│   └── comfyui-downloader.service
├── Cargo.toml
└── README.md
```

## Directory Layout (ComfyUI models)

The daemon organises downloaded models under a configurable root (default: `~/.local/share/comfyui/models/`):

```
models/
├── checkpoints/
├── loras/
├── vae/
├── controlnet/
├── embeddings/
└── upscale_models/
```

Model type is inferred from the CivitAI metadata and mapped to the appropriate subdirectory.

## Configuration

Configuration is read from `~/.config/comfyui-downloader/config.toml`:

```toml
[civitai]
api_key = ""          # CivitAI API key (required for private models)

[paths]
models_dir = "~/.local/share/comfyui/models"

[daemon]
update_interval_hours = 24
max_concurrent_downloads = 2
socket_path = "/run/user/$UID/comfyui-downloader.sock"
```

## IPC Protocol

Communication over the Unix socket uses newline-delimited JSON:

| Command | Payload | Description |
|---|---|---|
| `AddDownload` | `{ url, model_type? }` | Enqueue a CivitAI model URL |
| `ListQueue` | — | Return current queue state |
| `CheckUpdates` | — | Trigger an immediate update scan |
| `GetStatus` | — | Daemon health and active download progress |
| `Cancel` | `{ id }` | Cancel a queued or active download |

## Tech Stack

| Concern | Crate |
|---|---|
| Async runtime | `tokio` |
| HTTP client | `reqwest` |
| CLI parsing | `clap` |
| Serialisation | `serde` / `serde_json` |
| Database | `rusqlite` (SQLite) |
| Desktop notifications | `notify-rust` |
| Config | `toml` |
| Logging | `tracing` / `tracing-subscriber` |

## Requirements

- GNU/Linux with SystemD (user session)
- `libnotify` (usually pre-installed on desktop distros)
- A CivitAI account/API key for authenticated downloads

## Installation

```sh
cargo build --release
cp target/release/comfyui-downloader ~/.local/bin/
cp systemd/comfyui-downloader.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now comfyui-downloader
```

## CLI Usage

```sh
# Add a model by CivitAI URL
comfyui-dl add https://civitai.com/models/12345

# Show queue and active downloads
comfyui-dl status

# Trigger update check immediately
comfyui-dl check-updates

# Cancel a download
comfyui-dl cancel <id>
```
