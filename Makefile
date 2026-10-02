# Convenience wrapper around cargo. Binaries install to ~/.cargo/bin.

UNIT_DIR := $(HOME)/.config/systemd/user

.PHONY: all build test lint install uninstall install-service uninstall-service

all: build

build:
	cargo build --release

test:
	cargo test --workspace

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

install:
	cargo install --locked --path crates/av-server
	cargo install --locked --path crates/av-viz

uninstall:
	cargo uninstall av-server av-viz

# Linux (systemd) only; see contrib/launchd for macOS.
install-service: install
	mkdir -p $(UNIT_DIR)
	cp contrib/systemd/av-server.service $(UNIT_DIR)/
	systemctl --user daemon-reload
	systemctl --user enable --now av-server.service

uninstall-service:
	-systemctl --user disable --now av-server.service
	rm -f $(UNIT_DIR)/av-server.service
	systemctl --user daemon-reload
