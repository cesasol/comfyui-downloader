# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working in this repository.

## Build & development commands

```sh
cargo check                          # fast type/borrow check (no codegen)
cargo build                          # debug build
cargo build --release                # release build
cargo test                           # run all tests
cargo test <test_name>               # run a single test by name
cargo clippy -- -D warnings          # lint (treat warnings as errors)
RUST_LOG=debug cargo run --bin comfyui-downloader   # run daemon with debug logging
RUST_LOG=debug cargo run --bin comfyui-dl -- status # run CLI
```

Logging is controlled by `RUST_LOG` (e.g. `debug`, `info`, `comfyui_downloader=trace`).

## Architecture overview

Two binaries share the same library code:

- **`comfyui-downloader`** (`src/main.rs`) — long-running daemon; calls `daemon::run()`
- **`comfyui-dl`** (`src/cli_main.rs`) — one-shot CLI; connects to the daemon socket, sends a JSON command, prints the response, exits

### Daemon startup sequence (`src/daemon/mod.rs`)
1. Load `Config` from `~/.config/comfyui-downloader/config.toml`
2. Open SQLite catalog at `~/.local/share/comfyui-downloader/catalog.db`
3. Spawn `queue::run` task — processes pending jobs from the catalog
4. Spawn `updater::run` task — polls CivitAI for model updates on a configurable interval
5. Bind Unix socket and call `IpcServer::serve` — handles CLI requests until shutdown

### IPC layer (`src/ipc/`)
- `protocol.rs` — `Request` enum (tagged JSON: `{"cmd":"...", "payload":{...}}`) and `Response` enum (`{"status":"ok"/"err", "data":...}`)
- `server.rs` — `IpcServer` accepts connections, deserialises a `Request`, invokes the handler closure, serialises the `Response`
- `client.rs` — `IpcClient` used by the CLI binary to send a single request and read back the response

### Catalog (`src/catalog/`)
SQLite database accessed through `rusqlite`. `Catalog` wraps a `Connection` behind `Arc<Mutex<Catalog>>` shared across async tasks. Jobs have a `JobStatus` enum (`Queued`, `Running`, `Done`, `Failed`, `Cancelled`). Schema migrations live in `schema.rs`.

### Download pipeline (`src/daemon/`)
- `queue.rs` — polls catalog for `Queued` jobs, respects `max_concurrent_downloads`, calls the downloader
- `downloader.rs` — streams bytes from CivitAI via `reqwest`, verifies SHA-256 checksum, checks disk space via `libc::statvfs`, supports resume via HTTP Range
- `updater.rs` — compares catalog model versions against CivitAI API; enqueues new versions if found
- `notifier.rs` — wraps `notify-rust` for desktop notifications on completion/error/update

### CivitAI client (`src/civitai/`)
`CivitaiClient` uses `reqwest` with automatic retry on HTTP 429 (exponential backoff). Response types are in `types.rs`.

## Key design constraints

- The daemon and CLI are separate processes; the CLI **never** accesses the DB directly — all operations go through the IPC socket.
- The `Catalog` mutex is `tokio::sync::Mutex` because it is held across `.await` points in async handlers.
- Disk space is checked with `libc::statvfs` (no extra crate).
- `rusqlite` is compiled with the `bundled` feature — no system SQLite dependency.
