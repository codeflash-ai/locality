SHELL := /usr/bin/env bash

CARGO ?= cargo
NPM ?= npm

EXE_SUFFIX :=
ifeq ($(OS),Windows_NT)
EXE_SUFFIX := .exe
endif

LOC_DEBUG := target/debug/loc$(EXE_SUFFIX)
LOC_RELEASE := target/release/loc$(EXE_SUFFIX)
LOCALITY_FILE_PROVIDER_TARGET ?= notion-main

ifeq ($(OS),Windows_NT)
CLEAN_START_PLAN_CMD := & '.\scripts\clean-start.ps1'
CLEAN_START_CMD := & '.\scripts\clean-start.ps1' -Yes
clean-start-plan clean-start: SHELL := powershell.exe
clean-start-plan clean-start: .SHELLFLAGS := -NoProfile -ExecutionPolicy Bypass -Command
else
CLEAN_START_PLAN_CMD := scripts/clean-start.sh
CLEAN_START_CMD := scripts/clean-start.sh --yes
endif

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

.PHONY: build-crates
build-crates: stop-locality-dev-runtimes ## Build all non-desktop Rust workspace crates.
	$(CARGO) build --workspace --exclude locality-desktop

.PHONY: build-rust
build-rust: ## Build all Rust workspace packages.
	$(CARGO) build --workspace

.PHONY: stop-locality-dev-runtimes
stop-locality-dev-runtimes: ## Stop local daemon/provider processes that can lock Windows build outputs.
ifeq ($(OS),Windows_NT)
	@if [ -x "$(LOC_DEBUG)" ]; then loc="$(LOC_DEBUG)"; elif [ -x "$(LOC_RELEASE)" ]; then loc="$(LOC_RELEASE)"; else loc=""; fi; \
	if [ -n "$$loc" ]; then \
		"$$loc" file-provider stop "$(LOCALITY_FILE_PROVIDER_TARGET)" --json >/dev/null 2>&1 || true; \
		"$$loc" daemon stop >/dev/null 2>&1 || true; \
	fi
	@pwsh -NoProfile -ExecutionPolicy Bypass -Command '$$root = (Resolve-Path ".").Path; Get-Process locality-cloud-files,localityd -ErrorAction SilentlyContinue | Where-Object { $$_.Path -and $$_.Path.StartsWith($$root, [System.StringComparison]::OrdinalIgnoreCase) } | Stop-Process -Force'
else
	@true
endif

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

.PHONY: dev-restart
dev-restart: build-desktop prepare-desktop-dev-sidecars ## Build UI/debug sidecars, restart the dev daemon, and launch the Tauri dev app.
	$(LOC_DEBUG) daemon restart
	$(DESKTOP_NPM) run tauri -- dev

.PHONY: clean-start-plan
clean-start-plan: ## Print the local Locality clean-start reset actions without deleting anything.
	$(CLEAN_START_PLAN_CMD)

.PHONY: clean-start
clean-start: ## Stop Locality and remove local app/state/mounts/credentials for fresh manual testing.
	$(CLEAN_START_CMD)

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

.PHONY: test-linux-publish-config
test-linux-publish-config: ## Validate Linux package publish configuration.
	tests/linux_publish_config.sh
	tests/linux_publish_validation.sh

.PHONY: test-macos-publish-config
test-macos-publish-config: ## Validate macOS package publish configuration.
	tests/macos_publish_config.sh

.PHONY: test-windows-publish-config
test-windows-publish-config: ## Validate Windows package publish configuration.
	tests/windows_publish_config.sh

.PHONY: test-release-asset-names
test-release-asset-names: ## Validate GitHub Release asset naming configuration.
	tests/release_asset_names.sh

.PHONY: test-release-notes
test-release-notes: ## Validate LLM-generated GitHub Release notes plumbing.
	tests/release_notes.sh

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

.PHONY: docs-dev
docs-dev: ## Start the Mintlify docs preview from an isolated docs workspace.
	scripts/mintlify-docs.sh dev

.PHONY: docs-validate
docs-validate: ## Validate the Mintlify docs build.
	scripts/mintlify-docs.sh validate

.PHONY: docs-broken-links
docs-broken-links: ## Check Mintlify docs links from an isolated docs workspace.
	scripts/mintlify-docs.sh broken-links

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
