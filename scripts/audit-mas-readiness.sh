#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAURI_CONF="${ROOT}/apps/desktop/src-tauri/tauri.conf.json"
FRONTEND_APP="${ROOT}/apps/desktop/src/App.tsx"
RUST_MAIN="${ROOT}/apps/desktop/src-tauri/src/main.rs"
RUST_BUILD="${ROOT}/apps/desktop/src-tauri/build.rs"
PUBLISH_SCRIPT="${ROOT}/scripts/publish-mas.sh"
MAKEFILE="${ROOT}/Makefile"
HOST_ENTITLEMENTS="${ROOT}/platform/macos/LocalityFileProvider/App/Locality.entitlements"
EXTENSION_ENTITLEMENTS="${ROOT}/platform/macos/LocalityFileProvider/App/LocalityFileProvider.entitlements"
HOST_PLIST="${ROOT}/platform/macos/LocalityFileProvider/App/Locality.Info.plist"
EXTENSION_PLIST="${ROOT}/platform/macos/LocalityFileProvider/App/LocalityFileProvider.Info.plist"
CTL_PLIST="${ROOT}/platform/macos/LocalityFileProvider/App/LocalityFileProviderCtl.Info.plist"

fail() {
  printf 'mas-readiness: error: %s\n' "$*" >&2
  exit 1
}

log() {
  printf 'mas-readiness: %s\n' "$*"
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

json_value() {
  jq -r "$1" "$2"
}

require_plist_key() {
  local plist="$1"
  local key="$2"
  grep -q "<key>${key}</key>" "${plist}" || fail "${plist} is missing ${key}"
}

require_plist_string() {
  local plist="$1"
  local value="$2"
  grep -q "<string>${value}</string>" "${plist}" || fail "${plist} is missing string ${value}"
}

require_command jq

[[ -x "${PUBLISH_SCRIPT}" ]] || fail "missing executable Mac App Store publish script"
grep -q '^publish-mas:' "${MAKEFILE}" || fail "Makefile is missing publish-mas target"

[[ "$(json_value '.bundle.targets | index("app") != null' "${TAURI_CONF}")" == "true" ]] \
  || fail "Tauri bundle targets must include app for Mac App Store packaging"
[[ "$(json_value '.bundle.macOS.minimumSystemVersion' "${TAURI_CONF}")" == "14.0" ]] \
  || fail "Mac App Store packaging should match the current macOS 14.0 minimum"
[[ "$(json_value '.identifier' "${TAURI_CONF}")" == "ai.codeflash.locality" ]] \
  || fail "Tauri identifier must match the App Store app bundle ID"

grep -q 'VITE_LOCALITY_DISTRIBUTION_CHANNEL' "${FRONTEND_APP}" \
  || fail "frontend must gate App Store update behavior on VITE_LOCALITY_DISTRIBUTION_CHANNEL"
grep -q 'appStoreDistribution' "${FRONTEND_APP}" \
  || fail "frontend must disable self-update UI for App Store builds"
grep -q 'LOCALITY_DISTRIBUTION_CHANNEL' "${RUST_BUILD}" \
  || fail "Rust build script must embed LOCALITY_DISTRIBUTION_CHANNEL"
grep -q 'app_store_distribution' "${RUST_MAIN}" \
  || fail "desktop backend must gate App Store-specific behavior"

for plist in "${HOST_PLIST}" "${EXTENSION_PLIST}" "${CTL_PLIST}"; do
  require_plist_key "${plist}" "LSMinimumSystemVersion"
  require_plist_string "${plist}" "14.0"
done

for entitlements in "${HOST_ENTITLEMENTS}" "${EXTENSION_ENTITLEMENTS}"; do
  require_plist_key "${entitlements}" "com.apple.security.app-sandbox"
  require_plist_key "${entitlements}" "com.apple.security.application-groups"
  require_plist_string "${entitlements}" "group.ai.codeflash.locality"
  require_plist_key "${entitlements}" "com.apple.security.network.client"
done

log "static Mac App Store readiness checks passed"
log "remaining external inputs: App Store distribution certificates, provisioning profiles, App Store Connect app record, and TestFlight/App Review metadata"
