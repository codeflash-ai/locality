#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DESKTOP_DIR="${ROOT}/apps/desktop"
DMG_DIR="${ROOT}/target/release/bundle/dmg"
PRODUCT_NAME="${PUBLISH_PRODUCT_NAME:-AFS}"
CHANNEL="${PUBLISH_CHANNEL:-beta}"
DATE_STAMP="${PUBLISH_DATE:-$(date +%Y%m%d)}"
NOTARY_PROFILE="${APPLE_NOTARY_KEYCHAIN_PROFILE:-${NOTARY_KEYCHAIN_PROFILE:-afs-notary}}"

log() {
  printf 'publish: %s\n' "$*"
}

fail() {
  printf 'publish: error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

json_escape() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '%s' "${value}"
}

detect_signing_identity() {
  if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
    printf '%s\n' "${APPLE_SIGNING_IDENTITY}"
    return 0
  fi

  local identities
  identities="$(
    security find-identity -v -p codesigning 2>/dev/null \
      | sed -n 's/.*"\(Developer ID Application: .*([^)]*)\)".*/\1/p'
  )"
  local count
  count="$(printf '%s\n' "${identities}" | sed '/^$/d' | wc -l | tr -d ' ')"
  if [[ "${count}" == "1" ]]; then
    printf '%s\n' "${identities}"
    return 0
  fi

  fail "set APPLE_SIGNING_IDENTITY to the Developer ID Application certificate to use for signing"
}

notary_args() {
  if xcrun notarytool history --keychain-profile "${NOTARY_PROFILE}" >/dev/null 2>&1; then
    printf '%s\0%s\0' "--keychain-profile" "${NOTARY_PROFILE}"
    return 0
  fi

  if [[ -n "${APPLE_ID:-}" && -n "${APPLE_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]]; then
    printf '%s\0%s\0%s\0%s\0%s\0%s\0' \
      "--apple-id" "${APPLE_ID}" \
      "--password" "${APPLE_PASSWORD}" \
      "--team-id" "${APPLE_TEAM_ID}"
    return 0
  fi

  fail "notary credentials unavailable; create keychain profile '${NOTARY_PROFILE}' or set APPLE_ID, APPLE_PASSWORD, and APPLE_TEAM_ID"
}

assert_clean_tree() {
  if [[ "${PUBLISH_ALLOW_DIRTY:-0}" == "1" ]]; then
    return 0
  fi
  if [[ -n "$(git -C "${ROOT}" status --porcelain)" ]]; then
    fail "working tree has uncommitted changes; commit them first or set PUBLISH_ALLOW_DIRTY=1"
  fi
}

latest_tauri_dmg() {
  find "${DMG_DIR}" -maxdepth 1 -type f -name "${PRODUCT_NAME}_*.dmg" | sort | tail -n 1
}

verify_signed_app_in_dmg() (
  local dmg="$1"
  local expected_build="$2"
  local tmpdir mountpoint app
  tmpdir="$(mktemp -d)"
  mountpoint="${tmpdir}/mount"
  mkdir -p "${mountpoint}"

  cleanup() {
    hdiutil detach "${mountpoint}" -quiet >/dev/null 2>&1 || true
    rm -rf "${tmpdir}"
  }
  trap cleanup EXIT

  hdiutil attach "${dmg}" -readonly -noverify -noautoopen -mountpoint "${mountpoint}" -quiet
  app="${mountpoint}/${PRODUCT_NAME}.app"
  codesign --verify --deep --strict --verbose=2 "${app}"
  codesign -dv --verbose=4 "${app}" 2>&1 | grep -q "Developer ID Application"
  codesign -dv --verbose=4 "${app}/Contents/PlugIns/AgentFSFileProvider.appex" 2>&1 \
    | grep -q "Developer ID Application"
  grep -a -F -q "${expected_build}" "${app}/Contents/MacOS/afsd"
)

validate_notarized_dmg() {
  local dmg="$1"
  xcrun stapler validate "${dmg}"
  spctl --assess --type open --context context:primary-signature --verbose "${dmg}"
  hdiutil verify "${dmg}"

  (
    local tmpdir mountpoint app
    tmpdir="$(mktemp -d)"
    mountpoint="${tmpdir}/mount"
    mkdir -p "${mountpoint}"

    cleanup() {
      hdiutil detach "${mountpoint}" -quiet >/dev/null 2>&1 || true
      rm -rf "${tmpdir}"
    }
    trap cleanup EXIT

    hdiutil attach "${dmg}" -readonly -noverify -noautoopen -mountpoint "${mountpoint}" -quiet
    app="${mountpoint}/${PRODUCT_NAME}.app"
    codesign --verify --deep --strict --verbose=2 "${app}"
    spctl --assess --type execute --verbose "${app}"
  )
}

main() {
  [[ "$(uname -s)" == "Darwin" ]] || fail "macOS publishing must run on macOS"
  require_command git
  require_command npm
  require_command cargo
  require_command xcrun
  require_command hdiutil
  require_command codesign
  require_command spctl
  require_command security
  require_command strings

  assert_clean_tree

  local signing_identity commit_short commit_full config_json dmg arch output_name final_dmg sha
  signing_identity="$(detect_signing_identity)"
  commit_short="$(git -C "${ROOT}" rev-parse --short=7 HEAD)"
  commit_full="$(git -C "${ROOT}" rev-parse --short=12 HEAD)"
  config_json="$(printf '{"bundle":{"macOS":{"signingIdentity":"%s"}}}' "$(json_escape "${signing_identity}")")"

  local -a submit_args=()
  while IFS= read -r -d '' arg; do
    submit_args+=("${arg}")
  done < <(notary_args)

  log "commit ${commit_full}"
  log "signing identity: ${signing_identity}"
  log "notary profile: ${NOTARY_PROFILE}"

  mkdir -p "${DMG_DIR}"
  rm -f "${DMG_DIR}/${PRODUCT_NAME}_"*.dmg
  rm -rf "${ROOT}/target/release/bundle/macos/${PRODUCT_NAME}.app"

  log "building signed Tauri DMG"
  APPLE_SIGNING_IDENTITY="${signing_identity}" \
    npm --prefix "${DESKTOP_DIR}" run tauri -- build --bundles dmg --config "${config_json}"

  dmg="$(latest_tauri_dmg)"
  [[ -n "${dmg}" && -f "${dmg}" ]] || fail "Tauri did not produce a ${PRODUCT_NAME}_*.dmg artifact"

  log "applying installer disk icon"
  APPLE_SIGNING_IDENTITY="${signing_identity}" \
    bash "${DESKTOP_DIR}/scripts/postprocess-dmg-volume-icon.sh" "${dmg}"

  log "verifying Developer ID signatures"
  verify_signed_app_in_dmg "${dmg}" "${commit_full}"

  log "submitting for notarization"
  xcrun notarytool submit "${dmg}" --wait "${submit_args[@]}"

  log "stapling notarization ticket"
  xcrun stapler staple "${dmg}"

  arch="$(basename "${dmg}" .dmg)"
  arch="${arch##*_}"
  output_name="${PUBLISH_DMG_NAME:-${PRODUCT_NAME}-${CHANNEL}-${DATE_STAMP}-${commit_short}-notarized-${arch}.dmg}"
  final_dmg="${DMG_DIR}/${output_name}"
  cp "${dmg}" "${final_dmg}"

  log "validating notarized DMG"
  validate_notarized_dmg "${final_dmg}"

  sha="$(shasum -a 256 "${final_dmg}" | awk '{print $1}')"
  printf '\nPublished DMG: %s\n' "${final_dmg}"
  printf 'SHA256: %s\n' "${sha}"
}

main "$@"
