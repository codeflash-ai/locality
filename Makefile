SHELL := /usr/bin/env bash

CARGO ?= cargo
NPM ?= npm

DESKTOP_DIR := apps/desktop
DESKTOP_NPM := $(NPM) --prefix $(DESKTOP_DIR)
DESKTOP_NODE_MODULES_STAMP := $(DESKTOP_DIR)/node_modules/.package-lock.json
OAUTH_SERVICE_DIR := apps/oauth-service
OAUTH_SERVICE_NPM := $(NPM) --prefix $(OAUTH_SERVICE_DIR)
OAUTH_SERVICE_NODE_MODULES_STAMP := $(OAUTH_SERVICE_DIR)/node_modules/.package-lock.json

.DEFAULT_GOAL := help

.PHONY: help
help: ## Show available targets.
	@awk 'BEGIN {FS = ":.*##"; printf "Usage: make <target>\n\nTargets:\n"} /^[a-zA-Z0-9_.-]+:.*##/ {printf "  %-22s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

.PHONY: setup
setup: $(DESKTOP_NODE_MODULES_STAMP) $(OAUTH_SERVICE_NODE_MODULES_STAMP) ## Install app npm dependencies.

$(DESKTOP_NODE_MODULES_STAMP): $(DESKTOP_DIR)/package-lock.json $(DESKTOP_DIR)/package.json
	$(DESKTOP_NPM) ci

$(OAUTH_SERVICE_NODE_MODULES_STAMP): $(OAUTH_SERVICE_DIR)/package-lock.json $(OAUTH_SERVICE_DIR)/package.json
	$(OAUTH_SERVICE_NPM) ci

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

.PHONY: publish-unnotarized
publish-unnotarized: setup ## Build, sign, and validate a macOS DMG without notarization.
	PUBLISH_SKIP_NOTARIZATION=1 scripts/publish-macos.sh

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

.PHONY: install-macos-file-provider
install-macos-file-provider: ## Install/register the local macOS File Provider development bundle.
	platform/macos/LocalityFileProvider/scripts/install-dev-bundle.sh

.PHONY: prepare-desktop-dev-sidecars
prepare-desktop-dev-sidecars: ## Build debug desktop sidecars used by Tauri dev.
	$(DESKTOP_NPM) run dev:prepare

.PHONY: clean-start-plan
clean-start-plan: ## Print the local Locality clean-start reset actions without deleting anything.
	scripts/clean-start.sh

.PHONY: clean-start
clean-start: ## Stop Locality and remove local app/state/mounts/credentials for fresh manual testing.
	scripts/clean-start.sh --yes

.PHONY: check
check: check-rust check-desktop check-oauth-service ## Run Rust and app checks.

.PHONY: check-rust
check-rust: ## Check all Rust workspace packages.
	$(CARGO) check --workspace

.PHONY: check-desktop
check-desktop: ## Run desktop TypeScript and Vite build checks.
	$(DESKTOP_NPM) run build

.PHONY: check-oauth-service
check-oauth-service: $(OAUTH_SERVICE_NODE_MODULES_STAMP) ## Run OAuth service typecheck and tests.
	$(OAUTH_SERVICE_NPM) run check

.PHONY: audit-oauth-service
audit-oauth-service: $(OAUTH_SERVICE_NODE_MODULES_STAMP) ## Audit OAuth service npm dependencies.
	$(OAUTH_SERVICE_NPM) audit

.PHONY: test
test: test-rust ## Run the default test suite.

.PHONY: test-rust
test-rust: ## Run all Rust workspace tests.
	$(CARGO) test --workspace

.PHONY: test-linux-fuse
test-linux-fuse: ## Run the optional Linux FUSE smoke test when enabled by env vars.
	tests/linux_fuse_smoke.sh

.PHONY: test-simulation
test-simulation: ## Run deterministic randomized sync simulation smoke tests.
	$(CARGO) test -p locality-core --test simulation_harness
	$(CARGO) test -p localityd --test simulation -- --test-threads=1

.PHONY: test-simulation-nightly
test-simulation-nightly: ## Run ignored heavy randomized sync simulation tests.
	LOCALITY_SIMULATION_PROFILE=nightly $(CARGO) test -p localityd --test simulation -- --ignored --test-threads=1

.PHONY: test-simulation-live-notion
test-simulation-live-notion: ## Run live Notion reliability e2e against scratch content.
	$(CARGO) test -p loc-cli --test e2e_push_workflow live_seeded_reliability_sequence_push_drift_conflict_converges_notion -- --ignored --exact --test-threads=1
	$(CARGO) test -p loc-cli --test e2e_push_workflow live_multi_seed_reliability_sequences_converge_notion -- --ignored --exact --test-threads=1
	$(CARGO) test -p loc-cli --test e2e_push_workflow live_stress_repeated_push_reopen_status_noop_converges_notion -- --ignored --exact --test-threads=1
	$(CARGO) test -p loc-cli --test e2e_push_workflow live_stress_repeated_drift_conflict_recovery_converges_notion -- --ignored --exact --test-threads=1
	$(CARGO) test -p loc-cli --test e2e_push_workflow live_page_directory_create_then_move_pushes_under_final_parent -- --ignored --exact --test-threads=1
	$(CARGO) test -p loc-cli --test e2e_push_workflow live_validation_failure_blocks_before_journal_and_remote_write -- --ignored --exact --test-threads=1
	$(CARGO) test -p loc-cli --test e2e_push_workflow live_sqlite_restart_preserves_reconciled_journal_and_clean_status -- --ignored --exact --test-threads=1
	$(CARGO) test -p loc-cli --test e2e_push_workflow live_remote_fast_forward_updates_clean_file_and_preserves_pending_file -- --ignored --exact --test-threads=1

.PHONY: test-linux-publish-config
test-linux-publish-config: ## Validate Linux package publish configuration.
	tests/linux_publish_config.sh
	tests/linux_publish_validation.sh

.PHONY: test-macos-publish-config
test-macos-publish-config: ## Validate macOS package publish configuration.
	tests/macos_publish_config.sh

.PHONY: test-macos-file-provider-build-config
test-macos-file-provider-build-config: ## Validate macOS File Provider rebuild hooks.
	tests/macos_file_provider_build_config.sh

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
run-cli: ## Run the loc CLI; pass args with ARGS='status --json'.
	$(CARGO) run -p loc-cli -- $(ARGS)

.PHONY: run-daemon
run-daemon: ## Run the localityd daemon.
	$(CARGO) run -p localityd

.PHONY: clean
clean: ## Remove Rust and desktop build outputs.
	$(CARGO) clean
	rm -rf $(DESKTOP_DIR)/dist

.PHONY: clean-desktop
clean-desktop: ## Remove desktop build outputs.
	rm -rf $(DESKTOP_DIR)/dist
