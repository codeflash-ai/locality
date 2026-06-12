SHELL := /usr/bin/env bash

CARGO ?= cargo
NPM ?= npm

DESKTOP_DIR := apps/desktop
DESKTOP_NPM := $(NPM) --prefix $(DESKTOP_DIR)

.DEFAULT_GOAL := help

.PHONY: help
help: ## Show available targets.
	@awk 'BEGIN {FS = ":.*##"; printf "Usage: make <target>\n\nTargets:\n"} /^[a-zA-Z0-9_.-]+:.*##/ {printf "  %-22s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

.PHONY: setup
setup: ## Install desktop npm dependencies.
	$(DESKTOP_NPM) ci

.PHONY: build
build: build-rust build-desktop ## Build the Rust workspace and desktop frontend.

.PHONY: build-rust
build-rust: ## Build all Rust workspace packages.
	$(CARGO) build --workspace

.PHONY: build-release
build-release: ## Build all Rust workspace packages in release mode.
	$(CARGO) build --workspace --release

.PHONY: build-desktop
build-desktop: ## Build the desktop frontend assets.
	$(DESKTOP_NPM) run build

.PHONY: build-tauri
build-tauri: ## Build the packaged Tauri desktop app.
	$(DESKTOP_NPM) run tauri -- build

.PHONY: check
check: check-rust check-desktop ## Run Rust checks and desktop type/build checks.

.PHONY: check-rust
check-rust: ## Check all Rust workspace packages.
	$(CARGO) check --workspace

.PHONY: check-desktop
check-desktop: ## Run desktop TypeScript and Vite build checks.
	$(DESKTOP_NPM) run build

.PHONY: test
test: test-rust ## Run the default test suite.

.PHONY: test-rust
test-rust: ## Run all Rust workspace tests.
	$(CARGO) test --workspace

.PHONY: test-linux-fuse
test-linux-fuse: ## Run the optional Linux FUSE smoke test when enabled by env vars.
	tests/linux_fuse_smoke.sh

.PHONY: fmt
fmt: ## Format Rust code.
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Check Rust formatting.
	$(CARGO) fmt --all -- --check

.PHONY: lint
lint: ## Run Rust formatting checks and clippy.
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: ci
ci: fmt-check test ## Run the same checks as GitHub Actions.

.PHONY: dev-desktop
dev-desktop: ## Start the Vite desktop frontend dev server.
	$(DESKTOP_NPM) run dev

.PHONY: preview-desktop
preview-desktop: ## Preview the built desktop frontend.
	$(DESKTOP_NPM) run preview

.PHONY: dev-tauri
dev-tauri: ## Start the Tauri desktop app in development mode.
	$(DESKTOP_NPM) run tauri -- dev

.PHONY: run-cli
run-cli: ## Run the afs CLI; pass args with ARGS='status --json'.
	$(CARGO) run -p afs-cli -- $(ARGS)

.PHONY: run-daemon
run-daemon: ## Run the afsd daemon.
	$(CARGO) run -p afsd

.PHONY: clean
clean: ## Remove Rust and desktop build outputs.
	$(CARGO) clean
	rm -rf $(DESKTOP_DIR)/dist

.PHONY: clean-desktop
clean-desktop: ## Remove desktop build outputs.
	rm -rf $(DESKTOP_DIR)/dist
