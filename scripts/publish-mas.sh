#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DESKTOP_DIR="${ROOT}/apps/desktop"
APP_DIR="${ROOT}/target/release/bundle/macos"
MAS_OUT_DIR="${ROOT}/target/release/bundle/mas"
PRODUCT_NAME="${PUBLISH_PRODUCT_NAME:-AFS}"
CHANNEL="${PUBLISH_CHANNEL:-app-store}"
DATE_STAMP="${PUBLISH_DATE:-$(date +%Y%m%d)}"
APP_BUNDLE_ID="${MAS_APP_BUNDLE_ID:-ai.codeflash.afs}"
FILE_PROVIDER_BUNDLE_ID="${MAS_FILE_PROVIDER_BUNDLE_ID:-ai.codeflash.afs.AgentFS.FileProvider}"
HOST_ENTITLEMENTS="${ROOT}/platform/macos/AgentFSFileProvider/App/AgentFS.entitlements"
FILE_PROVIDER_ENTITLEMENTS="${ROOT}/platform/macos/AgentFSFileProvider/App/AgentFSFileProvider.entitlements"
TEMP_SECRET_DIR=""

log() {
  printf 'publish-mas: %s\n' "$*"
}

fail() {
  printf 'publish-mas: error: %s\n' "$*" >&2
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

cleanup_secrets() {
  if [[ -n "${TEMP_SECRET_DIR}" ]]; then
    rm -rf "${TEMP_SECRET_DIR}"
  fi
}

assert_clean_tree() {
  if [[ "${PUBLISH_ALLOW_DIRTY:-0}" == "1" ]]; then
    return 0
  fi
  if [[ -n "$(git -C "${ROOT}" status --porcelain)" ]]; then
    fail "working tree has uncommitted changes; commit them first or set PUBLISH_ALLOW_DIRTY=1"
  fi
}

assert_arm64_host() {
  if [[ "${PUBLISH_ALLOW_INTEL:-0}" == "1" ]]; then
    return 0
  fi
  case "$(uname -m)" in
    arm64|aarch64) ;;
    *) fail "Mac App Store publishing is Apple Silicon-only; set PUBLISH_ALLOW_INTEL=1 for a local unsupported Intel build" ;;
  esac
}

detect_identity() {
  local env_name="$1"
  local identity_regex="$2"
  local label="$3"
  local env_value="${!env_name:-}"
  if [[ -n "${env_value}" ]]; then
    printf '%s\n' "${env_value}"
    return 0
  fi

  local identities count
  identities="$(
    security find-identity -v -p codesigning 2>/dev/null \
      | sed -n 's/.*"\([^"]*\)".*/\1/p' \
      | grep -E "^(${identity_regex}): " || true
  )"
  count="$(printf '%s\n' "${identities}" | sed '/^$/d' | wc -l | tr -d ' ')"
  if [[ "${count}" == "1" ]]; then
    printf '%s\n' "${identities}"
    return 0
  fi

  fail "set ${env_name} to the ${label} signing identity"
}

profile_path() {
  local path_env="$1"
  local base64_env="$2"
  local output_name="$3"
  local path_value="${!path_env:-}"
  local base64_value="${!base64_env:-}"

  if [[ -n "${path_value}" ]]; then
    [[ -f "${path_value}" ]] || fail "${path_env} points to a missing file: ${path_value}"
    printf '%s\n' "${path_value}"
    return 0
  fi

  [[ -n "${base64_value}" ]] || fail "set ${path_env} or ${base64_env}"
  if [[ -z "${TEMP_SECRET_DIR}" ]]; then
    TEMP_SECRET_DIR="$(mktemp -d)"
  fi
  local output="${TEMP_SECRET_DIR}/${output_name}"
  printf '%s' "${base64_value}" | base64 --decode > "${output}" 2>/dev/null \
    || printf '%s' "${base64_value}" | base64 -D > "${output}"
  printf '%s\n' "${output}"
}

app_store_api_private_key_path() {
  if [[ -n "${APP_STORE_CONNECT_API_PRIVATE_KEY_PATH:-}" ]]; then
    [[ -f "${APP_STORE_CONNECT_API_PRIVATE_KEY_PATH}" ]] \
      || fail "APP_STORE_CONNECT_API_PRIVATE_KEY_PATH points to a missing file"
    printf '%s\n' "${APP_STORE_CONNECT_API_PRIVATE_KEY_PATH}"
    return 0
  fi

  [[ -n "${APP_STORE_CONNECT_API_PRIVATE_KEY:-}" ]] \
    || fail "set APP_STORE_CONNECT_API_PRIVATE_KEY or APP_STORE_CONNECT_API_PRIVATE_KEY_PATH"
  [[ -n "${APP_STORE_CONNECT_API_KEY_ID:-}" ]] \
    || fail "set APP_STORE_CONNECT_API_KEY_ID"
  if [[ -z "${TEMP_SECRET_DIR}" ]]; then
    TEMP_SECRET_DIR="$(mktemp -d)"
  fi
  local output="${TEMP_SECRET_DIR}/AuthKey_${APP_STORE_CONNECT_API_KEY_ID}.p8"
  printf '%s\n' "${APP_STORE_CONNECT_API_PRIVATE_KEY}" > "${output}"
  chmod 600 "${output}"
  printf '%s\n' "${output}"
}

altool_auth_args() {
  if [[ -n "${APP_STORE_CONNECT_API_KEY_ID:-}" || -n "${APP_STORE_CONNECT_API_ISSUER_ID:-}" ]]; then
    [[ -n "${APP_STORE_CONNECT_API_KEY_ID:-}" ]] || fail "set APP_STORE_CONNECT_API_KEY_ID"
    [[ -n "${APP_STORE_CONNECT_API_ISSUER_ID:-}" ]] || fail "set APP_STORE_CONNECT_API_ISSUER_ID"
    local key_path
    key_path="$(app_store_api_private_key_path)"
    printf '%s\0%s\0%s\0%s\0%s\0%s\0' \
      "--api-key" "${APP_STORE_CONNECT_API_KEY_ID}" \
      "--api-issuer" "${APP_STORE_CONNECT_API_ISSUER_ID}" \
      "--p8-file-path" "${key_path}"
    return 0
  fi

  if [[ -n "${APP_STORE_CONNECT_USERNAME:-}" && -n "${APP_STORE_CONNECT_PASSWORD:-}" ]]; then
    printf '%s\0%s\0%s\0%s\0' \
      "--username" "${APP_STORE_CONNECT_USERNAME}" \
      "--password" "${APP_STORE_CONNECT_PASSWORD}"
    return 0
  fi

  fail "set App Store Connect API key values or APP_STORE_CONNECT_USERNAME and APP_STORE_CONNECT_PASSWORD"
}

build_config_json() {
  local signing_identity="$1"
  printf '{"bundle":{"macOS":{"signingIdentity":"%s"}}}' "$(json_escape "${signing_identity}")"
}

profile_application_identifier() {
  local profile="$1"
  local tmp
  tmp="$(mktemp)"
  security cms -D -i "${profile}" > "${tmp}"
  plutil -extract Entitlements.application-identifier raw -o - "${tmp}" 2>/dev/null || true
  rm -f "${tmp}"
}

assert_profile_matches() {
  local profile="$1"
  local bundle_id="$2"
  local label="$3"
  local app_identifier
  app_identifier="$(profile_application_identifier "${profile}")"
  [[ -n "${app_identifier}" ]] \
    || fail "could not read application-identifier from ${label} provisioning profile"
  [[ "${app_identifier}" == *".${bundle_id}" ]] \
    || fail "${label} provisioning profile is for ${app_identifier}, expected ${bundle_id}"
}

sign_with_entitlements() {
  local identity="$1"
  local entitlements="$2"
  local path="$3"
  [[ -e "${path}" ]] || fail "missing code path to sign: ${path}"
  codesign --force --sign "${identity}" --entitlements "${entitlements}" "${path}"
}

assert_entitled() {
  local path="$1"
  local entitlement="$2"
  codesign -d --entitlements :- "${path}" 2>/dev/null | grep -q "<key>${entitlement}</key>" \
    || fail "${path} is missing entitlement ${entitlement}"
}

resign_app_store_bundle() {
  local app="$1"
  local signing_identity="$2"
  local app_profile="$3"
  local file_provider_profile="$4"
  local appex="${app}/Contents/PlugIns/AgentFSFileProvider.appex"

  [[ -d "${app}" ]] || fail "missing app bundle: ${app}"
  [[ -d "${appex}" ]] || fail "missing File Provider extension: ${appex}"

  assert_profile_matches "${app_profile}" "${APP_BUNDLE_ID}" "app"
  assert_profile_matches "${file_provider_profile}" "${FILE_PROVIDER_BUNDLE_ID}" "File Provider"

  cp "${app_profile}" "${app}/Contents/embedded.provisionprofile"
  cp "${file_provider_profile}" "${appex}/Contents/embedded.provisionprofile"

  sign_with_entitlements "${signing_identity}" "${HOST_ENTITLEMENTS}" "${app}/Contents/MacOS/agentfs-file-providerctl"
  sign_with_entitlements "${signing_identity}" "${HOST_ENTITLEMENTS}" "${app}/Contents/MacOS/afs"
  sign_with_entitlements "${signing_identity}" "${HOST_ENTITLEMENTS}" "${app}/Contents/MacOS/afsd"
  sign_with_entitlements "${signing_identity}" "${FILE_PROVIDER_ENTITLEMENTS}" "${appex}/Contents/MacOS/AgentFSFileProvider"
  sign_with_entitlements "${signing_identity}" "${FILE_PROVIDER_ENTITLEMENTS}" "${appex}"
  sign_with_entitlements "${signing_identity}" "${HOST_ENTITLEMENTS}" "${app}/Contents/MacOS/${PRODUCT_NAME}"
  sign_with_entitlements "${signing_identity}" "${HOST_ENTITLEMENTS}" "${app}"

  codesign --verify --deep --strict --verbose=2 "${app}"
  assert_entitled "${app}" "com.apple.security.app-sandbox"
  assert_entitled "${appex}" "com.apple.security.app-sandbox"
}

run_altool() {
  local action="$1"
  local pkg="$2"
  local -a auth_args=()
  while IFS= read -r -d '' arg; do
    auth_args+=("${arg}")
  done < <(altool_auth_args)

  case "${action}" in
    validate) xcrun altool --validate-app "${pkg}" "${auth_args[@]}" ;;
    upload) xcrun altool --upload-package "${pkg}" "${auth_args[@]}" ;;
    *) fail "unknown altool action: ${action}" ;;
  esac
}

main() {
  trap cleanup_secrets EXIT
  [[ "$(uname -s)" == "Darwin" ]] || fail "Mac App Store publishing must run on macOS"
  assert_arm64_host
  require_command git
  require_command npm
  require_command cargo
  require_command xcrun
  require_command codesign
  require_command security
  require_command plutil
  require_command productbuild
  require_command pkgutil
  require_command base64
  require_command shasum

  assert_clean_tree

  local app_signing_identity installer_identity app_profile file_provider_profile
  local commit_short commit_full config_json app pkg output_name sha arch
  app_signing_identity="$(
    detect_identity \
      MAS_APP_SIGNING_IDENTITY \
      '3rd Party Mac Developer Application|Apple Distribution' \
      'Mac App Store application'
  )"
  installer_identity="$(
    detect_identity \
      MAS_INSTALLER_SIGNING_IDENTITY \
      '3rd Party Mac Developer Installer' \
      'Mac App Store installer'
  )"
  app_profile="$(
    profile_path \
      MAS_APP_PROVISIONING_PROFILE \
      MAS_APP_PROVISIONING_PROFILE_BASE64 \
      AFS_AppStore.provisionprofile
  )"
  file_provider_profile="$(
    profile_path \
      MAS_FILE_PROVIDER_PROVISIONING_PROFILE \
      MAS_FILE_PROVIDER_PROVISIONING_PROFILE_BASE64 \
      AFS_FileProvider_AppStore.provisionprofile
  )"

  commit_short="$(git -C "${ROOT}" rev-parse --short=7 HEAD)"
  commit_full="$(git -C "${ROOT}" rev-parse --short=12 HEAD)"
  config_json="$(build_config_json "${app_signing_identity}")"
  arch="$(uname -m)"

  log "commit ${commit_full}"
  log "app signing identity: ${app_signing_identity}"
  log "installer signing identity: ${installer_identity}"

  rm -rf "${APP_DIR}/${PRODUCT_NAME}.app" "${MAS_OUT_DIR}"
  mkdir -p "${APP_DIR}" "${MAS_OUT_DIR}"

  log "building Mac App Store-channel app bundle"
  VITE_AFS_DISTRIBUTION_CHANNEL=mas \
  AFS_DISTRIBUTION_CHANNEL=mas \
  APPLE_SIGNING_IDENTITY="${app_signing_identity}" \
    npm --prefix "${DESKTOP_DIR}" run tauri -- build --bundles app --config "${config_json}"

  app="${APP_DIR}/${PRODUCT_NAME}.app"
  log "embedding provisioning profiles and re-signing bundle"
  resign_app_store_bundle "${app}" "${app_signing_identity}" "${app_profile}" "${file_provider_profile}"

  output_name="${MAS_PKG_NAME:-${PRODUCT_NAME}-${CHANNEL}-${DATE_STAMP}-${commit_short}-${arch}.pkg}"
  pkg="${MAS_OUT_DIR}/${output_name}"

  log "building signed App Store package"
  productbuild --component "${app}" /Applications --sign "${installer_identity}" "${pkg}"
  pkgutil --check-signature "${pkg}" >/dev/null

  sha="$(shasum -a 256 "${pkg}" | awk '{print $1}')"
  printf '%s %s\n' "${sha}" "${pkg}" > "${pkg}.sha256"

  if [[ "${MAS_VALIDATE_WITH_APPLE:-0}" == "1" || "${MAS_UPLOAD:-0}" == "1" ]]; then
    log "validating package with App Store Connect"
    run_altool validate "${pkg}"
  fi

  if [[ "${MAS_UPLOAD:-0}" == "1" ]]; then
    log "uploading package to App Store Connect"
    run_altool upload "${pkg}"
  fi

  printf '\nPublished Mac App Store package:\n'
  printf '  %s\n' "${pkg}"
  printf '  %s.sha256\n' "${pkg}"
  printf 'SHA256: %s\n' "${sha}"
}

main "$@"
