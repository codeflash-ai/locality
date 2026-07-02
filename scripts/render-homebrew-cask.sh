#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DMG_DIR="${HOMEBREW_DMG_DIR:-${ROOT}/target/release/bundle/dmg}"
OUTPUT="${HOMEBREW_CASK_OUTPUT:-${ROOT}/target/release/homebrew/Casks/loc.rb}"
VERSION="${HOMEBREW_VERSION:-}"
RELEASE_TAG="${HOMEBREW_RELEASE_TAG:-}"
BASE_URL="${HOMEBREW_BASE_URL:-}"

log() {
  printf 'homebrew-cask: %s\n' "$*"
}

fail() {
  printf 'homebrew-cask: error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

latest_notarized_dmg_for_arch() {
  local arch="$1"
  find "${DMG_DIR}" -maxdepth 1 -type f -name "*-notarized-*${arch}.dmg" | sort | tail -n 1
}

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
    return
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
    return
  fi
  fail "missing required command: shasum or sha256sum"
}

version_from_tauri_config() {
  sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    "${ROOT}/apps/desktop/src-tauri/tauri.conf.json" | head -n 1
}

artifact_url() {
  local env_value="$1"
  local path="$2"
  if [[ -n "${env_value}" ]]; then
    printf '%s\n' "${env_value}"
    return 0
  fi
  [[ -n "${BASE_URL}" ]] || fail "set HOMEBREW_BASE_URL or HOMEBREW_RELEASE_TAG"
  printf '%s/%s\n' "${BASE_URL%/}" "$(basename "${path}")"
}

write_cask() {
  local dmg="$1"
  local url sha
  url="$(artifact_url "${HOMEBREW_DMG_URL:-}" "${dmg}")"
  sha="$(sha256_file "${dmg}")"

  cat >"${OUTPUT}" <<EOF
cask "loc" do
  version "${VERSION}"
  sha256 "${sha}"

  url "${url}",
      verified: "github.com/codeflash-ai/locality/"
  name "Locality"
  desc "Mount workspaces as local files for agents"
  homepage "https://github.com/codeflash-ai/locality"

  auto_updates true
  depends_on arch: :arm64
  depends_on macos: :sonoma

  app "Locality.app"
  binary "#{appdir}/Locality.app/Contents/MacOS/loc"

  uninstall script: {
    executable: "#{appdir}/Locality.app/Contents/MacOS/Locality",
    args: ["--prepare-uninstall"],
  }

  zap trash: [
    "~/.loc",
    "~/Library/Application Support/ai.codeflash.locality",
    "~/Library/Caches/ai.codeflash.locality",
    "~/Library/Preferences/ai.codeflash.locality.plist",
  ]
end
EOF
}

main() {
  require_command sed

  VERSION="${VERSION:-$(version_from_tauri_config)}"
  [[ -n "${VERSION}" ]] || fail "set HOMEBREW_VERSION or define bundle.version in tauri.conf.json"
  if [[ -z "${BASE_URL}" && -n "${RELEASE_TAG}" ]]; then
    BASE_URL="https://github.com/codeflash-ai/locality/releases/download/${RELEASE_TAG}"
  fi

  local dmg
  dmg="${HOMEBREW_DMG:-${HOMEBREW_ARM_DMG:-$(latest_notarized_dmg_for_arch aarch64)}}"
  if [[ -z "${dmg}" ]]; then
    dmg="${HOMEBREW_DMG:-${HOMEBREW_ARM_DMG:-$(latest_notarized_dmg_for_arch arm64)}}"
  fi
  [[ -n "${dmg}" && -f "${dmg}" ]] || fail "need a notarized Apple Silicon DMG; set HOMEBREW_DMG or HOMEBREW_ARM_DMG"

  mkdir -p "$(dirname "${OUTPUT}")"
  write_cask "${dmg}"

  log "wrote ${OUTPUT}"
}

main "$@"
