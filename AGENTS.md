# AGENTS.md — Coding Agent Guide

## Build & Development Commands

```sh
cargo check                          # fast type/borrow check (no codegen) — run first
cargo build                          # debug build
cargo build --release                # release build
cargo clippy -- -D warnings          # lint; all warnings are errors — must be clean
cargo test                           # run all tests
cargo test <test_name>               # run a single test by substring match, e.g.:
cargo test test_parse_model_page_url # run one specific test
cargo test catalog::tests            # run all tests in a module path
RUST_LOG=debug cargo run --bin comfyui-downloader   # daemon with debug logging
RUST_LOG=debug cargo run --bin comfyui-dl -- status # CLI with debug logging
```

**Workflow**: `cargo check` → `cargo clippy -- -D warnings` → `cargo test`. Fix all clippy warnings before considering work done. There is no `rustfmt.toml`; use default `rustfmt` settings (`cargo fmt`).

---

## Project Structure

Two binaries share the same library:
- `src/main.rs` — daemon entry point, calls `daemon::run()`
- `src/cli_main.rs` — CLI entry point, calls `cli::run()`
- `src/lib.rs` — library root, re-exports modules
- `src/config.rs` — config loading/saving via XDG paths

**Critical rule**: The CLI never accesses the SQLite database directly. All operations go through the IPC Unix socket.

---

## Code Style

### Formatting
- Default `rustfmt` (no config file). Run `cargo fmt` before committing.
- 4-space indentation. No trailing whitespace.

### Imports
- Group: `crate::` imports first, then external crates, then `std`.
- No blank lines required between groups (convention in this codebase is loose ordering).
- Prefer explicit paths over glob imports except in `#[cfg(test)]` where `use super::*` is standard.

```rust
// Typical pattern — crate first, then external, then std
use crate::catalog::{Catalog, JobStatus};
use crate::config::Config;
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;
```

### Naming
- `snake_case`: functions, variables, modules, fields
- `PascalCase`: types, structs, enums, traits
- `SCREAMING_SNAKE_CASE`: constants (`const`, `static`)
- Short names for cloned `Arc`s inside `tokio::spawn` closures: `cfg`, `cat`, `civ`, `act`, `prog`, `wake`

### Types
- `Uuid` (v4) for all job/entity IDs
- `DateTime<Utc>` (chrono) for all timestamps; stored as RFC 3339 strings in SQLite
- `PathBuf` for owned paths, `&Path` for borrowed path arguments
- `Option<String>` for optional text fields
- `serde_json::Value` for ad-hoc/dynamic JSON responses in IPC handlers

### Structs and Enums
Derive order convention: `#[derive(Debug, Clone, Serialize, Deserialize)]`

```rust
// Internal types use snake_case JSON keys
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus { Queued, Downloading, Done, Failed, Cancelled }

// CivitAI API response types use camelCase to match the API
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelVersion { pub id: u64, pub base_model: Option<String>, ... }

// IPC protocol uses externally-tagged enums
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "payload", rename_all = "snake_case")]
pub enum Request { AddDownload { url: String, model_type: Option<String> }, ... }
```

---

## Error Handling

- **All fallible functions** return `anyhow::Result<T>`. Import as `use anyhow::{Context, Result};`.
- Enrich errors with `.context("static message")` or `.with_context(|| format!("dynamic {}", val))`.
- Early-return errors with `bail!("reason")`.
- Log full error chains with `{e:#}` (anyhow multi-cause format).
- Use `unwrap_or_default()` only for non-critical fallbacks (e.g., display strings).
- Never use `.unwrap()` or `.expect()` in production code paths (tests are fine).

```rust
// Good
let conn = Connection::open(path)
    .with_context(|| format!("opening catalog at {}", path.display()))?;

// Good — log full chain
error!("Job {job_id} failed: {e:#}");

// Bad — swallows context
let conn = Connection::open(path)?;
```

---

## Async Patterns

- Runtime: `tokio` with `#[tokio::main]` / `#[tokio::test]`.
- Use `tokio::sync::Mutex` (not `std::sync::Mutex`) for any mutex held across `.await` points.
- Clone all `Arc`s **before** the `async move` closure or `tokio::spawn`:

```rust
let cfg = config.clone();
let cat = catalog.clone();
tokio::spawn(async move {
    queue::run(cfg, cat).await;
});
```

- Use `CancellationToken` (from `tokio_util::sync`) for cooperative task cancellation.
- Shared mutable state pattern: `Arc<Mutex<T>>` with type aliases at module level:

```rust
pub type ActiveTasks = Arc<Mutex<HashMap<Uuid, CancellationToken>>>;
pub type ProgressMap  = Arc<Mutex<HashMap<Uuid, DownloadProgress>>>;
```

---

## Logging

Use `tracing` macros (`info!`, `warn!`, `error!`). Logging is controlled by `RUST_LOG` at runtime.

```rust
info!("Starting download job {}", job.id);
warn!("Rate limited; retrying in {}s", delay.as_secs());
error!("Job {job_id} failed: {e:#}");
```

---

## Testing

- Tests live in an inline `#[cfg(test)]` module at the **bottom** of each source file.
- Always `use super::*;` to import the module under test.
- Async tests use `#[tokio::test]`.
- Use SQLite in-memory databases for catalog tests:

```rust
let catalog = Catalog::open(std::path::Path::new(":memory:")).unwrap();
```

- Name tests `test_<what_it_tests>` (snake_case, descriptive).
- Each test covers one behaviour; no multi-concern tests.
- Run a single test: `cargo test test_parse_model_page_url`

---

## Key Design Constraints

1. **CLI ↔ Daemon isolation**: CLI sends a single JSON request over the Unix socket and prints the response. It never reads the SQLite DB.
2. **Tokio Mutex everywhere**: The `Catalog` mutex is `tokio::sync::Mutex` because it is held across `.await` points in IPC handlers.
3. **SQLite is bundled**: `rusqlite` is compiled with `features = ["bundled"]` — no system SQLite required.
4. **XDG paths**: Always use `xdg_config_home()` / `xdg_data_home()` from `config.rs` rather than hardcoding `~/.config` or `~/.local/share`.
5. **API key required**: The daemon errors at request time if `civitai.api_key` is not set. The startup scanner skips silently.
6. **No `as any` / suppression**: Never suppress type errors or lint warnings with `#[allow(...)]` without justification in a comment.
