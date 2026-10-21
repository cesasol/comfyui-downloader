# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Credentials in the system keyring** — the CivitAI API key and the HuggingFace token are stored in the freedesktop Secret Service (`org.freedesktop.secrets`) instead of plaintext `config.toml`;
  `comfyui-dl set-key` writes there and gained `--service civitai|huggingface`. Existing plaintext values are migrated into the keyring on daemon startup and removed from the file, and remain a
  fallback when no Secret Service is reachable

### Removed

- **Tauri desktop app** — the Svelte GUI, the `src-tauri` crate, the desktop entry and its icon, and the AppImage build are gone; the daemon is driven by `comfyui-dl` only
- **Streaming subscription IPC** — the `subscribe` request, its `Frame` snapshot stream, and the daemon event bus that fed the planned terminal UI; `get_status` still reports queue and progress on
  demand

### Fixed

- **Keyring race at login** — the daemon opened the Secret Service once and, if it lost the race against gnome-keyring-daemon claiming `org.freedesktop.secrets`, ran without credentials; it now waits
  up to 30 s for the service to appear and then asks the keyring to unlock — startup blocks until that prompt is answered or dismissed — and stops retrying as soon as the user dismisses the prompt
  instead of re-raising it
- **Credentials appearing after startup** — a credential stored while the daemon is running (for example with `comfyui-dl set-key`, which no longer suggests a restart) is now picked up by the next
  request that needs it; the resolved credentials are shared with the HTTP clients and re-read from the keyring at most once a minute
- **Startup scan skipped forever** — the scan bailed out when the daemon had no API key yet (keyring not ready, or none configured), so files already on disk never had their artifacts backfilled; it
  now waits for the key and runs once one is configured

## [0.1.0] - 2026-03-26

### Added

- **Download queue** — enqueue model downloads; the daemon processes them with configurable concurrency
- **Download resume** — resumes interrupted downloads using HTTP range requests when the server supports it
- **Metadata sidecars** — writes a `.metadata.json` file alongside each model containing SHA-256 hash, CivitAI API response, base model, and preview path
- **Preview images** — downloads and saves the CivitAI preview image next to each model file
- **Startup scanner** — on daemon start, scans the models directory for existing files missing metadata or preview images and fetches them via SHA-256 hash lookup; registers discovered models in the
  catalog
- **Duplicate detection** — skips the download if the target file already exists on disk
- **Update notifications** — periodically polls CivitAI for newer versions of tracked models and flags them in the database; updates are never auto-downloaded
- **Smart model routing** — automatically places checkpoint models in the correct ComfyUI subdirectory by inspecting the safetensors file header for bundled VAE/CLIP components; GGUF checkpoints are
  always routed to `diffusion_models/`
- **Early access filtering** — skips EarlyAccess model versions by default (configurable)
- **Checksum verification** — validates SHA-256 hashes reported by CivitAI after each download
- **Retry logic** — handles CivitAI rate-limit responses (HTTP 429) with exponential backoff
- **Disk space guard** — checks available disk space before starting a download
- **Desktop notifications** — emits libnotify notifications on download completion, errors, and available updates; progress notifications every 10%
- **Model management** — list downloaded models, delete models (removes files and catalog entry), and relocate misplaced files during update checks
- **IPC interface** — Unix domain socket with a simple JSON protocol for daemon ↔ CLI communication
- **CLI client** — `comfyui-dl` command covering all daemon interactions (add, status, list, cancel, delete, check-updates, updates, download-version, set-key)
- **SystemD integration** — ships a `.service` unit file for `systemctl --user`
- **Tauri desktop app** — Svelte-based model gallery viewer with sidebar filtering by model type and base model

[Unreleased]: https://gitlab.com/cesasol/comfyui-downloader/compare/v0.1.0...HEAD
[0.1.0]: https://gitlab.com/cesasol/comfyui-downloader/-/releases/tag/v0.1.0
