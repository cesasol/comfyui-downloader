# comfyui-downloader justfile
# Usage: just [recipe]
# Recipes: build, test, lint, fmt, fmt-check, check, install, install-user, uninstall, uninstall-user, update, clean

# Default recipe — shows available recipes
[private]
default:
    @just --list

# Installation prefix (override with `just PREFIX=/usr`)
PREFIX := env_var_or_default("PREFIX", "/usr/local")
BINDIR := env_var_or_default("DESTDIR", "") + PREFIX + "/bin"
SYSTEMD := env_var_or_default("DESTDIR", "") + "/usr/lib/systemd/user"

# Per-user install paths (XDG; no sudo required)
USER_DATA := env_var_or_default("XDG_DATA_HOME", env_var("HOME") + "/.local/share")
USER_CONFIG := env_var_or_default("XDG_CONFIG_HOME", env_var("HOME") + "/.config")
USER_BINDIR := env_var("HOME") + "/.local/bin"
USER_SYSTEMD := USER_CONFIG + "/systemd/user"

# Build the daemon and CLI binaries
build:
    cargo build --release --all-features -p comfyui-downloader

# Run tests
test:
    cargo test --all-features -p comfyui-downloader

# Run clippy lints
lint:
    cargo clippy --all-features -p comfyui-downloader -- -D warnings

# Format source code
fmt:
    cargo fmt

# Check formatting without modifying files
fmt-check:
    cargo fmt --check

# Fast type/borrow check (no codegen)
check:
    cargo check --all-features -p comfyui-downloader

# One command, three callers: the developer, the hook, and the pipeline.
ci: fmt-check lint test

# Run the full hook suite across every file (used by CI).
hooks:
    uvx prek run --all-files

# Install git hooks locally.
setup:
    uvx prek install

# Clean build artifacts
clean:
    cargo clean

# Install binaries and the systemd service
install: build
    install -Dm755 target/release/comfyui-downloader {{ BINDIR }}/comfyui-downloader
    install -Dm755 target/release/comfyui-dl         {{ BINDIR }}/comfyui-dl
    install -Dm644 systemd/comfyui-downloader.service {{ SYSTEMD }}/comfyui-downloader.service

# Uninstall binaries and the systemd service
uninstall:
    rm -f {{ BINDIR }}/comfyui-downloader
    rm -f {{ BINDIR }}/comfyui-dl
    rm -f {{ SYSTEMD }}/comfyui-downloader.service

# Install into the current user's $HOME (no sudo). Ensure ~/.local/bin is on PATH.
install-user: build
    install -Dm755 target/release/comfyui-downloader {{ USER_BINDIR }}/comfyui-downloader
    install -Dm755 target/release/comfyui-dl         {{ USER_BINDIR }}/comfyui-dl
    install -Dm644 systemd/comfyui-downloader-user.service {{ USER_SYSTEMD }}/comfyui-downloader.service
    @echo "Installed to {{ USER_BINDIR }}. Reload the user units with:"
    @echo "  systemctl --user daemon-reload"
    @echo "  systemctl --user enable --now comfyui-downloader.service"

# Uninstall the per-user install
uninstall-user:
    rm -f {{ USER_BINDIR }}/comfyui-downloader
    rm -f {{ USER_BINDIR }}/comfyui-dl
    rm -f {{ USER_SYSTEMD }}/comfyui-downloader.service

# Pull latest changes and rebuild
update:
    git pull
    just build
