SHELL := /usr/bin/env bash

CARGO ?= cargo
NPM ?= npm

DESKTOP_DIR := apps/desktop
DESKTOP_NPM := $(NPM) --prefix $(DESKTOP_DIR)
DESKTOP_NODE_MODULES_STAMP := $(DESKTOP_DIR)/node_modules/.package-lock.json

.DEFAULT_GOAL := help

.PHONY: help
help: ## Show available targets.
	@awk 'BEGIN {FS = ":.*##"; printf "Usage: make <target>\n\nTargets:\n"} /^[a-zA-Z0-9_.-]+:.*##/ {printf "  %-22s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

.PHONY: setup
setup: $(DESKTOP_NODE_MODULES_STAMP) ## Install desktop npm dependencies.

$(DESKTOP_NODE_MODULES_STAMP): $(DESKTOP_DIR)/package-lock.json $(DESKTOP_DIR)/package.json
	$(DESKTOP_NPM) ci

.PHONY: build-all
build-all: build-release build-tauri ## Build all deliverables in release mode.

.PHONY: build
build: build-rust build-desktop ## Build the Rust workspace and desktop frontend.

.PHONY: build-rust
build-rust: ## Build all Rust workspace packages.
	$(CARGO) build --workspace

.PHONY: build-release
build-release: build-desktop ## Build all Rust workspace packages in release mode.
	$(CARGO) build --workspace --release

.PHONY: build-desktop
build-desktop: $(DESKTOP_NODE_MODULES_STAMP) ## Build the desktop frontend assets.
	$(DESKTOP_NPM) run build

.PHONY: build-tauri
build-tauri: ## Build the packaged Tauri desktop app.
	$(DESKTOP_NPM) run build:mac

.PHONY: build-mas
build-mas: setup ## Build a local Mac App Store-channel .app bundle.
	$(DESKTOP_NPM) run build:mas

.PHONY: publish
publish: setup ## Build, sign, notarize, staple, and validate a macOS DMG.
	scripts/publish-macos.sh

.PHONY: publish-mas
publish-mas: setup ## Build, sign, package, and optionally upload a Mac App Store build.
	scripts/publish-mas.sh

.PHONY: publish-linux
publish-linux: setup ## Build and validate Linux .deb and .rpm packages.
	scripts/publish-linux.sh

.PHONY: publish-windows
publish-windows: setup ## Build, sign, and validate a Windows NSIS package.
	pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/publish-windows.ps1

.PHONY: build-tauri-windows
build-tauri-windows: setup ## Build the Windows Tauri installer.
	$(DESKTOP_NPM) run build:windows

.PHONY: render-homebrew-cask
render-homebrew-cask: ## Render a Homebrew cask from published macOS DMG artifacts.
	scripts/render-homebrew-cask.sh

.PHONY: render-updater-manifest
render-updater-manifest: ## Render a Tauri updater manifest from updater artifacts.
	scripts/render-tauri-updater-manifest.sh

.PHONY: render-linux-repositories
render-linux-repositories: ## Render APT and RPM repository metadata from Linux package artifacts.
	scripts/render-linux-repositories.sh

.PHONY: bump-version
bump-version: ## Bump release version; pass VERSION=0.1.1.
	@test -n "$(VERSION)" || (echo "Usage: make bump-version VERSION=0.1.1" >&2; exit 2)
	node scripts/bump-version.mjs "$(VERSION)"

.PHONY: audit-mas-readiness
audit-mas-readiness: ## Run static checks for Mac App Store release readiness.
	scripts/audit-mas-readiness.sh

.PHONY: prepare-macos-file-provider
prepare-macos-file-provider: ## Stage the macOS File Provider extension for Tauri packaging.
	$(DESKTOP_DIR)/scripts/prepare-macos-file-provider.sh

.PHONY: clean-start-plan
clean-start-plan: ## Print the local AFS clean-start reset actions without deleting anything.
	scripts/clean-start.sh

.PHONY: clean-start
clean-start: ## Stop AFS and remove local app/state/mounts/credentials for fresh manual testing.
	scripts/clean-start.sh --yes

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

.PHONY: test-linux-publish-config
test-linux-publish-config: ## Validate Linux package publish configuration.
	tests/linux_publish_config.sh
	tests/linux_publish_validation.sh

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
