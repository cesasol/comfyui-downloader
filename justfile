# comfyui-downloader justfile
# Usage: just [recipe]
# Recipes: build, build-gui, test, lint, fmt, fmt-check, check, install, uninstall, update, clean

# Default recipe — shows available recipes
[private]
default:
    @just --list

# Installation prefix (override with `just PREFIX=/usr`)
PREFIX  := env_var_or_default("PREFIX", "/usr/local")
BINDIR  := env_var_or_default("DESTDIR", "") + PREFIX + "/bin"
SYSTEMD := env_var_or_default("DESTDIR", "") + "/usr/lib/systemd/user"
APPDIR  := env_var_or_default("DESTDIR", "") + PREFIX + "/share/applications"
ICONDIR := env_var_or_default("DESTDIR", "") + PREFIX + "/share/icons/hicolor/128x128/apps"

# Build the daemon and CLI binaries
build:
    cargo build --release -p comfyui-downloader

# Build the GUI (Tauri) without bundling
build-gui:
    pnpm --dir gui install --frozen-lockfile
    cargo tauri build --no-bundle

# Run tests
test:
    cargo test -p comfyui-downloader

# Run clippy lints
lint:
    cargo clippy -p comfyui-downloader -- -D warnings

# Format source code
fmt:
    cargo fmt

# Check formatting without modifying files
fmt-check:
    cargo fmt --check

# Fast type/borrow check (no codegen)
check:
    cargo check -p comfyui-downloader

# Clean build artifacts
clean:
    cargo clean

# Install binaries, systemd service, desktop entry, and icon
install: build
    install -Dm755 target/release/comfyui-downloader {{BINDIR}}/comfyui-downloader
    install -Dm755 target/release/comfyui-dl         {{BINDIR}}/comfyui-dl
    install -Dm644 systemd/comfyui-downloader.service {{SYSTEMD}}/comfyui-downloader.service
    install -Dm644 comfyui-downloader.desktop         {{APPDIR}}/comfyui-downloader.desktop
    install -Dm644 src-tauri/icons/128x128.png        {{ICONDIR}}/comfyui-downloader.png

# Uninstall binaries, systemd service, desktop entry, and icon
uninstall:
    rm -f {{BINDIR}}/comfyui-downloader
    rm -f {{BINDIR}}/comfyui-dl
    rm -f {{SYSTEMD}}/comfyui-downloader.service
    rm -f {{APPDIR}}/comfyui-downloader.desktop
    rm -f {{ICONDIR}}/comfyui-downloader.png

# Pull latest changes and rebuild
update:
    git pull
    just build
