#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WINDOWS_BUNDLE_SCRIPT="${ROOT}/apps/desktop/scripts/prepare-windows-bundle.ps1"
NSIS_HOOKS="${ROOT}/apps/desktop/src-tauri/windows/locality-sidecars.nsh"
MOUNT_LOGO_ICO="${ROOT}/apps/desktop/src-tauri/icons/locality-mount-logo.ico"
MAKEFILE="${ROOT}/Makefile"

fail() {
  printf 'windows publish config test: %s\n' "$*" >&2
  exit 1
}

[[ -f "${MOUNT_LOGO_ICO}" ]] \
  || fail "mount root ICO logo asset is missing"
grep -q '^test-windows-publish-config:' "${MAKEFILE}" \
  || fail "Makefile is missing test-windows-publish-config target"
grep -F -q 'locality-mount-logo.ico' "${WINDOWS_BUNDLE_SCRIPT}" \
  || fail "Windows bundle prep must stage the mount root logo ICO"
grep -F -q 'File /oname=locality-mount-logo.ico' "${NSIS_HOOKS}" \
  || fail "NSIS install hook must install the mount root logo ICO"
grep -F -q 'Delete "$INSTDIR\locality-mount-logo.ico"' "${NSIS_HOOKS}" \
  || fail "NSIS uninstall hook must remove the mount root logo ICO"
