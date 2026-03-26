# comfyui-downloader Makefile
# Usage: make [target]
# Targets: build, build-gui, test, lint, fmt, fmt-check, check, install, uninstall, update, clean

PREFIX ?= /usr/local
BINDIR  = $(DESTDIR)$(PREFIX)/bin
SYSTEMD = $(DESTDIR)/usr/lib/systemd/user
APPDIR  = $(DESTDIR)$(PREFIX)/share/applications
ICONDIR = $(DESTDIR)$(PREFIX)/share/icons/hicolor/128x128/apps

.PHONY: build build-gui test lint fmt fmt-check check clean install uninstall update

build:
	cargo build --release -p comfyui-downloader

build-gui:
	pnpm --dir gui install --frozen-lockfile
	cargo tauri build

test:
	cargo test -p comfyui-downloader

lint:
	cargo clippy -p comfyui-downloader -- -D warnings

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

check:
	cargo check -p comfyui-downloader

clean:
	cargo clean

install: build
	install -Dm755 target/release/comfyui-downloader $(BINDIR)/comfyui-downloader
	install -Dm755 target/release/comfyui-dl         $(BINDIR)/comfyui-dl
	install -Dm644 systemd/comfyui-downloader.service $(SYSTEMD)/comfyui-downloader.service
	install -Dm644 comfyui-downloader.desktop         $(APPDIR)/comfyui-downloader.desktop
	install -Dm644 src-tauri/icons/128x128.png        $(ICONDIR)/comfyui-downloader.png

uninstall:
	rm -f $(BINDIR)/comfyui-downloader
	rm -f $(BINDIR)/comfyui-dl
	rm -f $(SYSTEMD)/comfyui-downloader.service
	rm -f $(APPDIR)/comfyui-downloader.desktop
	rm -f $(ICONDIR)/comfyui-downloader.png

update:
	git pull
	$(MAKE) build
