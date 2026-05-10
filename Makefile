SHELL := /bin/bash

.PHONY: help build release check fmt fmt-check clippy test test-full gates \
		wizard wizard-dry-run run run-tray proxy health daemon-status dashboard \
		status-file-init clean-runtime

CONFIG ?= $(HOME)/.codex/mcp-mux.toml
SERVICE ?= general-memory
SOCKET ?= /tmp/$(SERVICE).sock
CMD ?= npx
CMD_ARGS ?= @modelcontextprotocol/server-memory
STATUS_FILE ?= $(HOME)/.rust-mux/status/$(SERVICE).json
LOG_LEVEL ?= info

help:
	@echo "rust-mux targets"
	@echo "  build             - cargo build"
	@echo "  release           - cargo build --release"
	@echo "  check             - cargo check --all-targets --all-features"
	@echo "  fmt               - cargo fmt --all"
	@echo "  fmt-check         - cargo fmt -- --check"
	@echo "  clippy            - cargo clippy --all-targets --all-features -- -D warnings"
	@echo "  test              - cargo test --all-targets --all-features"
	@echo "  test-full         - test + opcjonalny pakiet testów transportu (ignored)"
	@echo "  gates             - fmt-check + clippy + test"
	@echo ""
	@echo "Wizard / runtime"
	@echo "  wizard            - uruchom interaktywny wizard (zapisuje config)"
	@echo "  wizard-dry-run    - uruchom wizard bez zapisu"
	@echo "  run               - uruchom mux dla SERVICE z CONFIG"
	@echo "  run-tray          - uruchom mux z tray + STATUS_FILE"
	@echo "  proxy             - uruchom rust-mux-proxy dla SOCKET"
	@echo "  health            - health check dla SERVICE z CONFIG"
	@echo "  daemon-status     - status wszystkich usług z daemona"
	@echo "  dashboard         - uruchom tray dashboard (feature: tray)"
	@echo "  status-file-init  - utwórz katalog dla STATUS_FILE"
	@echo "  clean-runtime     - usuń SOCKET i STATUS_FILE"
	@echo ""
	@echo "Zmienne (override np. 'make run SERVICE=brave-search'):"
	@echo "  CONFIG=$(CONFIG)"
	@echo "  SERVICE=$(SERVICE)"
	@echo "  SOCKET=$(SOCKET)"
	@echo "  STATUS_FILE=$(STATUS_FILE)"
	@echo "  CMD=$(CMD)"
	@echo "  CMD_ARGS=$(CMD_ARGS)"
	@echo "  LOG_LEVEL=$(LOG_LEVEL)"

build:
	cargo build

release:
	cargo build --release

check:
	cargo check --all-targets --all-features

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt -- --check

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --all-targets --all-features

test-full:
	cargo test --all-targets --all-features
	cargo test --all-targets --all-features -- --ignored

gates: fmt-check clippy test

wizard:
	cargo run --bin rust-mux -- wizard --config "$(CONFIG)" --service "$(SERVICE)"

wizard-dry-run:
	cargo run --bin rust-mux -- wizard --config "$(CONFIG)" --service "$(SERVICE)" --dry-run

run:
	cargo run --bin rust-mux -- --config "$(CONFIG)" --service "$(SERVICE)" --log-level "$(LOG_LEVEL)"

run-tray: status-file-init
	cargo run --bin rust-mux -- --config "$(CONFIG)" --service "$(SERVICE)" --tray --status-file "$(STATUS_FILE)" --log-level "$(LOG_LEVEL)"

proxy:
	cargo run --bin rust-mux-proxy -- --socket "$(SOCKET)"

health:
	cargo run --bin rust-mux -- health --config "$(CONFIG)" --service "$(SERVICE)"

daemon-status:
	rust-mux daemon-status --config "$(CONFIG)"

dashboard:
	rust-mux dashboard --config "$(CONFIG)"

status-file-init:
	mkdir -p "$(dir $(STATUS_FILE))"

clean-runtime:
	rm -f "$(SOCKET)" "$(STATUS_FILE)"
