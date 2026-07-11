#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WINDOWS_BUNDLE_SCRIPT="${ROOT}/apps/desktop/scripts/prepare-windows-bundle.ps1"
WINDOWS_PUBLISH_SCRIPT="${ROOT}/scripts/publish-windows.ps1"
WINDOWS_WORKFLOW="${ROOT}/.github/workflows/release-windows.yml"
NSIS_HOOKS="${ROOT}/apps/desktop/src-tauri/windows/locality-sidecars.nsh"
MOUNT_LOGO_ICO="${ROOT}/apps/desktop/src-tauri/icons/locality-mount-logo.ico"
MAKEFILE="${ROOT}/Makefile"
PACKAGE_JSON="${ROOT}/apps/desktop/package.json"

fail() {
  printf 'windows publish config test: %s\n' "$*" >&2
  exit 1
}

[[ -f "${MOUNT_LOGO_ICO}" ]] \
  || fail "mount root ICO logo asset is missing"
grep -q '^test-windows-publish-config:' "${MAKEFILE}" \
  || fail "Makefile is missing test-windows-publish-config target"
grep -q '^build-tauri-windows-arm64:' "${MAKEFILE}" \
  || fail "Makefile is missing Windows ARM64 Tauri build target"
grep -q '^publish-windows-arm64:' "${MAKEFILE}" \
  || fail "Makefile is missing Windows ARM64 publish target"
grep -F -q '"build:windows-arm64": "tauri build --bundles nsis --target aarch64-pc-windows-msvc"' "${PACKAGE_JSON}" \
  || fail "desktop package.json must expose a Windows ARM64 Tauri build script"
grep -F -q 'locality-mount-logo.ico' "${WINDOWS_BUNDLE_SCRIPT}" \
  || fail "Windows bundle prep must stage the mount root logo ICO"
grep -F -q 'LOCALITY_WINDOWS_TARGET' "${WINDOWS_BUNDLE_SCRIPT}" \
  || fail "Windows bundle prep must accept an explicit target triple"
grep -F -q 'TAURI_ENV_TARGET_TRIPLE' "${WINDOWS_BUNDLE_SCRIPT}" \
  || fail "Windows bundle prep must infer Tauri target triples for direct npm builds"
grep -F -q 'TAURI_ENV_ARCH' "${WINDOWS_BUNDLE_SCRIPT}" \
  || fail "Windows bundle prep must infer Tauri ARM64 architecture for direct npm builds"
grep -F -q -- '--target", $TargetTriple' "${WINDOWS_BUNDLE_SCRIPT}" \
  || fail "Windows bundle prep must cross-compile sidecars for the explicit target"
grep -F -q 'target\$TargetTriple\release' "${WINDOWS_BUNDLE_SCRIPT}" \
  || fail "Windows bundle prep must copy sidecars from the explicit target release directory"
grep -F -q 'LOCALITY_WINDOWS_TARGET' "${WINDOWS_PUBLISH_SCRIPT}" \
  || fail "Windows publish script must accept an explicit target triple"
grep -F -q -- '--target", $targetTriple' "${WINDOWS_PUBLISH_SCRIPT}" \
  || fail "Windows publish script must pass the explicit target to Tauri"
grep -F -q 'target\$targetTriple\release\bundle' "${WINDOWS_PUBLISH_SCRIPT}" \
  || fail "Windows publish script must read target-specific Tauri bundle output"
grep -F -q 'aarch64-pc-windows-msvc' "${WINDOWS_PUBLISH_SCRIPT}" \
  || fail "Windows publish script must map the Windows ARM64 Rust target"
grep -F -q 'rustup target add aarch64-pc-windows-msvc' "${WINDOWS_WORKFLOW}" \
  || fail "Windows release workflow must install the Windows ARM64 Rust target"
grep -F -q 'LOCALITY_WINDOWS_TARGET: aarch64-pc-windows-msvc' "${WINDOWS_WORKFLOW}" \
  || fail "Windows release workflow must build the ARM64 sidecars and installer with the ARM64 target"
grep -F -q 'Locality_Windows_ARM64_v$env:APP_VERSION.exe' "${WINDOWS_WORKFLOW}" \
  || fail "Windows release workflow must publish a versioned ARM64 installer asset"
grep -F -q 'UPDATER_WINDOWS_AARCH64_ARTIFACT: target/release/bundle/windows/Locality_Windows_ARM64_v${{ env.APP_VERSION }}.exe' "${WINDOWS_WORKFLOW}" \
  || fail "Windows release workflow must include ARM64 in the updater manifest"
grep -F -q 'File /oname=locality-mount-logo.ico' "${NSIS_HOOKS}" \
  || fail "NSIS install hook must install the mount root logo ICO"
grep -F -q 'Delete "$INSTDIR\locality-mount-logo.ico"' "${NSIS_HOOKS}" \
  || fail "NSIS uninstall hook must remove the mount root logo ICO"
