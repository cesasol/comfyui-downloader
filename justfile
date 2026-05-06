# comfyui-downloader justfile
# Usage: just [recipe]
# Recipes: build, build-gui, test, lint, fmt, fmt-check, check, install, install-user, uninstall, uninstall-user, update, clean

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

# Per-user install paths (XDG; no sudo required)
USER_DATA    := env_var_or_default("XDG_DATA_HOME", env_var("HOME") + "/.local/share")
USER_CONFIG  := env_var_or_default("XDG_CONFIG_HOME", env_var("HOME") + "/.config")
USER_BINDIR  := env_var("HOME") + "/.local/bin"
USER_SYSTEMD := USER_CONFIG + "/systemd/user"
USER_APPDIR  := USER_DATA + "/applications"
USER_ICONDIR := USER_DATA + "/icons/hicolor/128x128/apps"

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

# Install into the current user's $HOME (no sudo). Ensure ~/.local/bin is on PATH.
install-user: build
    install -Dm755 target/release/comfyui-downloader {{USER_BINDIR}}/comfyui-downloader
    install -Dm755 target/release/comfyui-dl         {{USER_BINDIR}}/comfyui-dl
    install -Dm644 systemd/comfyui-downloader.service {{USER_SYSTEMD}}/comfyui-downloader.service
    install -Dm644 comfyui-downloader.desktop         {{USER_APPDIR}}/comfyui-downloader.desktop
    install -Dm644 src-tauri/icons/128x128.png        {{USER_ICONDIR}}/comfyui-downloader.png
    @echo "Installed to {{USER_BINDIR}}. Reload the user units with:"
    @echo "  systemctl --user daemon-reload"
    @echo "  systemctl --user enable --now comfyui-downloader.service"

# Uninstall the per-user install
uninstall-user:
    rm -f {{USER_BINDIR}}/comfyui-downloader
    rm -f {{USER_BINDIR}}/comfyui-dl
    rm -f {{USER_SYSTEMD}}/comfyui-downloader.service
    rm -f {{USER_APPDIR}}/comfyui-downloader.desktop
    rm -f {{USER_ICONDIR}}/comfyui-downloader.png

# Pull latest changes and rebuild
update:
    git pull
    just build
