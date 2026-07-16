#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

DEFAULT_APP_PATH="/Applications/Locality Prompt Test.app"
DEFAULT_SIGNING_IDENTITY="Developer ID Application: CodeFlash Inc (C484HB7Q6S)"
DEFAULT_DISPLAY_NAME="Locality Prompt Test"
DEFAULT_DMG_DIR="${ROOT}/target/release/bundle/dmg"

APP_PATH="${LOCALITY_PROMPT_TEST_APP_PATH:-${DEFAULT_APP_PATH}}"
SOURCE_APP="${LOCALITY_PROMPT_TEST_SOURCE_APP:-}"
DMG="${LOCALITY_PROMPT_TEST_DMG:-}"
DISPLAY_NAME="${LOCALITY_PROMPT_TEST_DISPLAY_NAME:-${DEFAULT_DISPLAY_NAME}}"
SIGNING_IDENTITY="${LOCALITY_PROMPT_TEST_SIGNING_IDENTITY:-${SIGNING_IDENTITY:-${DEFAULT_SIGNING_IDENTITY}}}"
BUNDLE_ID="${LOCALITY_PROMPT_TEST_BUNDLE_ID:-}"
DRY_RUN=0
RESET_DOMAIN="${LOCALITY_PROMPT_TEST_RESET:-1}"
LAUNCH="${LOCALITY_PROMPT_TEST_LAUNCH:-1}"
FORCE_NON_TEST_APP_PATH="${LOCALITY_PROMPT_TEST_FORCE_NON_TEST_APP_PATH:-0}"

HOST_ENTITLEMENTS="${ROOT}/platform/macos/LocalityFileProvider/App/LocalityDeveloperId.entitlements"
FILE_PROVIDER_ENTITLEMENTS="${ROOT}/platform/macos/LocalityFileProvider/App/LocalityFileProvider.entitlements"
MOUNT_POINT=""
TMPDIR=""

usage() {
  cat <<'USAGE'
Usage: scripts/install-macos-prompt-test-app.sh [options]

Build target helper for manual macOS File Provider onboarding tests. Installs a
freshly identified "Locality Prompt Test.app" from a built Locality.app bundle
or DMG, resets any existing test app File Provider domain, re-signs the bundle,
registers its File Provider extension, and launches it.

Options:
  --app-path PATH          Target app path. Defaults to /Applications/Locality Prompt Test.app.
  --source-app PATH        Source Locality.app bundle. Defaults to the built Tauri bundle or DMG.
  --dmg PATH               Source DMG when --source-app is not passed. Defaults to newest built Locality_*.dmg.
  --bundle-id ID           App bundle identifier. Defaults to ai.codeflash.locality.promptfresh<TIMESTAMP>.
  --display-name NAME      App and File Provider display name. Defaults to "Locality Prompt Test".
  --signing-identity NAME  Developer ID signing identity.
  --reset-domain           Reset the existing test app File Provider domain before reinstalling. Default.
  --no-reset-domain        Do not reset the existing test app File Provider domain.
  --launch                 Launch the installed test app. Default.
  --no-launch              Do not launch the installed test app.
  --force-non-test-app-path
                           Allow a target app path that does not look like a prompt-test app.
  --dry-run                Print the install plan without changing the system.
  -h, --help               Show this help.

Environment aliases:
  LOCALITY_PROMPT_TEST_APP_PATH
  LOCALITY_PROMPT_TEST_SOURCE_APP
  LOCALITY_PROMPT_TEST_DMG
  LOCALITY_PROMPT_TEST_BUNDLE_ID
  LOCALITY_PROMPT_TEST_TIMESTAMP
  LOCALITY_PROMPT_TEST_DISPLAY_NAME
  LOCALITY_PROMPT_TEST_SIGNING_IDENTITY
  LOCALITY_PROMPT_TEST_RESET=0|1
  LOCALITY_PROMPT_TEST_LAUNCH=0|1
  LOCALITY_PROMPT_TEST_FORCE_NON_TEST_APP_PATH=0|1
USAGE
}

log() {
  printf '%s\n' "$*"
}

fail() {
  printf 'install-macos-prompt-test-app: %s\n' "$*" >&2
  exit 1
}

print_cmd() {
  printf '+'
  local arg
  for arg in "$@"; do
    printf ' %q' "${arg}"
  done
  printf '\n'
}

run() {
  print_cmd "$@"
  if [[ "${DRY_RUN}" -eq 0 ]]; then
    "$@"
  fi
}

run_may_fail() {
  print_cmd "$@"
  if [[ "${DRY_RUN}" -eq 0 ]]; then
    "$@" >/dev/null 2>&1 || true
  fi
}

prompt_test_timestamp() {
  if [[ -n "${LOCALITY_PROMPT_TEST_TIMESTAMP:-}" ]]; then
    printf '%s\n' "${LOCALITY_PROMPT_TEST_TIMESTAMP}"
    return 0
  fi
  date +%Y%m%d%H%M%S
}

default_bundle_id() {
  printf 'ai.codeflash.locality.promptfresh%s\n' "$(prompt_test_timestamp)"
}

default_dmg() {
  local candidate
  local latest=""
  while IFS= read -r -d '' candidate; do
    if [[ -z "${latest}" || "${candidate}" -nt "${latest}" ]]; then
      latest="${candidate}"
    fi
  done < <(find "${DEFAULT_DMG_DIR}" -maxdepth 1 -type f -name 'Locality_*.dmg' -print0 2>/dev/null)

  [[ -n "${latest}" ]] || return 1
  printf '%s\n' "${latest}"
}

expand_tilde() {
  local path="$1"
  if [[ "${path}" == "~" ]]; then
    printf '%s\n' "${HOME}"
  elif [[ "${path}" == "~/"* ]]; then
    printf '%s/%s\n' "${HOME}" "${path#"~/"}"
  else
    printf '%s\n' "${path}"
  fi
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --app-path)
        APP_PATH="${2:?--app-path requires a value}"
        shift 2
        ;;
      --source-app)
        SOURCE_APP="${2:?--source-app requires a value}"
        shift 2
        ;;
      --dmg)
        DMG="${2:?--dmg requires a value}"
        shift 2
        ;;
      --bundle-id)
        BUNDLE_ID="${2:?--bundle-id requires a value}"
        shift 2
        ;;
      --display-name)
        DISPLAY_NAME="${2:?--display-name requires a value}"
        shift 2
        ;;
      --signing-identity)
        SIGNING_IDENTITY="${2:?--signing-identity requires a value}"
        shift 2
        ;;
      --reset-domain)
        RESET_DOMAIN=1
        shift
        ;;
      --no-reset-domain)
        RESET_DOMAIN=0
        shift
        ;;
      --launch)
        LAUNCH=1
        shift
        ;;
      --no-launch)
        LAUNCH=0
        shift
        ;;
      --force-non-test-app-path)
        FORCE_NON_TEST_APP_PATH=1
        shift
        ;;
      --dry-run)
        DRY_RUN=1
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        fail "unknown option: $1"
        ;;
    esac
  done
}

cleanup() {
  if [[ -n "${MOUNT_POINT}" && -d "${MOUNT_POINT}" && "${DRY_RUN}" -eq 0 ]]; then
    hdiutil detach "${MOUNT_POINT}" -quiet >/dev/null 2>&1 \
      || hdiutil detach "${MOUNT_POINT}" -force -quiet >/dev/null 2>&1 \
      || true
  fi
  if [[ -n "${TMPDIR}" && -d "${TMPDIR}" ]]; then
    rm -rf "${TMPDIR}" >/dev/null 2>&1 || true
  fi
}

require_execute_environment() {
  [[ "${DRY_RUN}" -eq 1 ]] && return 0
  [[ "$(uname -s)" == "Darwin" ]] || fail "this installer only runs on macOS"
  command -v hdiutil >/dev/null 2>&1 || fail "hdiutil is required"
  command -v codesign >/dev/null 2>&1 || fail "codesign is required"
  command -v pluginkit >/dev/null 2>&1 || fail "pluginkit is required"
  [[ -f "${HOST_ENTITLEMENTS}" ]] || fail "missing host entitlements: ${HOST_ENTITLEMENTS}"
  [[ -f "${FILE_PROVIDER_ENTITLEMENTS}" ]] || fail "missing File Provider entitlements: ${FILE_PROVIDER_ENTITLEMENTS}"
}

validate_app_path() {
  [[ -n "${APP_PATH}" ]] || fail "--app-path must not be empty"
  [[ "${APP_PATH}" == *.app ]] || fail "--app-path must point to a .app bundle: ${APP_PATH}"
  [[ "${APP_PATH}" != "/" ]] || fail "--app-path cannot be /"

  if [[ "${FORCE_NON_TEST_APP_PATH}" != "1" ]]; then
    local app_name
    app_name="$(basename "${APP_PATH}")"
    [[ "${app_name}" != "Locality.app" ]] \
      || fail "--app-path points at the production app. Use a prompt-test app path or pass --force-non-test-app-path."
    [[ "${app_name}" == *Prompt* || "${app_name}" == *prompt* || "${app_name}" == *Test* || "${app_name}" == *test* ]] \
      || fail "--app-path must look like a prompt-test app path: ${APP_PATH}. Pass --force-non-test-app-path to override."
  fi
}

validate_source_and_target_paths() {
  [[ "${SOURCE_APP}" != "${APP_PATH}" ]] \
    || fail "--source-app and --app-path must be different paths"

  if [[ -e "${SOURCE_APP}" && -e "${APP_PATH}" ]]; then
    local source_real target_real
    source_real="$(cd "$(dirname "${SOURCE_APP}")" && pwd -P)/$(basename "${SOURCE_APP}")"
    target_real="$(cd "$(dirname "${APP_PATH}")" && pwd -P)/$(basename "${APP_PATH}")"
    [[ "${source_real}" != "${target_real}" ]] \
      || fail "--source-app and --app-path resolve to the same path"
  fi
}

resolve_source_app() {
  SOURCE_APP="$(expand_tilde "${SOURCE_APP}")"
  DMG="$(expand_tilde "${DMG}")"
  APP_PATH="$(expand_tilde "${APP_PATH}")"
  validate_app_path

  if [[ -n "${SOURCE_APP}" ]]; then
    [[ -d "${SOURCE_APP}" ]] || fail "source app does not exist: ${SOURCE_APP}"
    validate_source_and_target_paths
    return 0
  fi

  local bundle_app="${ROOT}/target/release/bundle/macos/Locality.app"
  if [[ -d "${bundle_app}" ]]; then
    SOURCE_APP="${bundle_app}"
    validate_source_and_target_paths
    return 0
  fi

  if [[ -z "${DMG}" ]]; then
    DMG="$(default_dmg)" || fail "missing DMG under ${DEFAULT_DMG_DIR}. Run make build-tauri first or pass --dmg PATH."
    DMG="$(expand_tilde "${DMG}")"
  fi
  [[ -f "${DMG}" ]] || fail "missing DMG: ${DMG}. Run make build-tauri first or pass --dmg PATH."

  TMPDIR="$(mktemp -d)"
  MOUNT_POINT="${TMPDIR}/mount"
  mkdir -p "${MOUNT_POINT}"
  run hdiutil attach "${DMG}" -readonly -noverify -noautoopen -mountpoint "${MOUNT_POINT}" -quiet
  SOURCE_APP="${MOUNT_POINT}/Locality.app"
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    validate_source_and_target_paths
    return 0
  fi
  [[ -d "${SOURCE_APP}" ]] || fail "DMG did not contain Locality.app: ${DMG}"
  validate_source_and_target_paths
}

reset_existing_test_app() {
  local old_helper="${APP_PATH}/Contents/MacOS/locality-file-providerctl"
  local old_appex="${APP_PATH}/Contents/PlugIns/LocalityFileProvider.appex"

  run_may_fail osascript -e "tell application id \"${BUNDLE_ID}\" to quit"
  run_may_fail pkill -f "${APP_PATH}/Contents"

  if [[ "${RESET_DOMAIN}" == "1" && -x "${old_helper}" ]]; then
    run_may_fail "${old_helper}" reset --json
  fi
  if [[ -d "${old_appex}" ]]; then
    run_may_fail pluginkit -r "${old_appex}"
  fi
}

install_bundle() {
  local appex="${APP_PATH}/Contents/PlugIns/LocalityFileProvider.appex"
  local extension_bundle_id="${BUNDLE_ID}.FileProvider"

  run mkdir -p "$(dirname "${APP_PATH}")"
  run rm -rf "${APP_PATH}"
  run ditto "${SOURCE_APP}" "${APP_PATH}"

  run /usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier ${BUNDLE_ID}" "${APP_PATH}/Contents/Info.plist"
  run /usr/libexec/PlistBuddy -c "Set :CFBundleDisplayName ${DISPLAY_NAME}" "${APP_PATH}/Contents/Info.plist"
  run /usr/libexec/PlistBuddy -c "Set :CFBundleName ${DISPLAY_NAME}" "${APP_PATH}/Contents/Info.plist"
  run /usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier ${extension_bundle_id}" "${appex}/Contents/Info.plist"
  run /usr/libexec/PlistBuddy -c "Set :CFBundleDisplayName ${DISPLAY_NAME}" "${appex}/Contents/Info.plist"
  run /usr/libexec/PlistBuddy -c "Set :CFBundleName LocalityPromptTestFileProvider" "${appex}/Contents/Info.plist"

  run codesign --force --options runtime --timestamp=none --sign "${SIGNING_IDENTITY}" --entitlements "${HOST_ENTITLEMENTS}" --identifier "${BUNDLE_ID}.locality-desktop" "${APP_PATH}/Contents/MacOS/locality-desktop"
  run codesign --force --options runtime --timestamp=none --sign "${SIGNING_IDENTITY}" --entitlements "${HOST_ENTITLEMENTS}" --identifier "${BUNDLE_ID}.loc" "${APP_PATH}/Contents/MacOS/loc"
  run codesign --force --options runtime --timestamp=none --sign "${SIGNING_IDENTITY}" --entitlements "${HOST_ENTITLEMENTS}" --identifier "${BUNDLE_ID}.localityd" "${APP_PATH}/Contents/MacOS/localityd"
  run codesign --force --options runtime --timestamp=none --sign "${SIGNING_IDENTITY}" --entitlements "${HOST_ENTITLEMENTS}" --identifier "${BUNDLE_ID}.file-providerctl" "${APP_PATH}/Contents/MacOS/locality-file-providerctl"
  run codesign --force --options runtime --timestamp=none --sign "${SIGNING_IDENTITY}" --entitlements "${FILE_PROVIDER_ENTITLEMENTS}" --identifier "${extension_bundle_id}.binary" "${appex}/Contents/MacOS/LocalityFileProvider"
  run codesign --force --options runtime --timestamp=none --sign "${SIGNING_IDENTITY}" --entitlements "${FILE_PROVIDER_ENTITLEMENTS}" --identifier "${extension_bundle_id}" "${appex}"
  run codesign --force --options runtime --timestamp=none --sign "${SIGNING_IDENTITY}" --entitlements "${HOST_ENTITLEMENTS}" --identifier "${BUNDLE_ID}" "${APP_PATH}"

  run_may_fail xattr -dr com.apple.quarantine "${APP_PATH}"
  run pluginkit -a "${appex}"
  run codesign --verify --deep --strict --verbose=2 "${APP_PATH}"
  run pluginkit -m -v -i "${extension_bundle_id}"
  run "${APP_PATH}/Contents/MacOS/locality-file-providerctl" register --mount-id loc --display-name "${DISPLAY_NAME}" --json
  run_may_fail "${APP_PATH}/Contents/MacOS/locality-file-providerctl" --json list

  if [[ "${LAUNCH}" == "1" ]]; then
    run open -a "${APP_PATH}"
  fi
}

main() {
  parse_args "$@"
  if [[ -z "${BUNDLE_ID}" ]]; then
    BUNDLE_ID="$(default_bundle_id)"
  fi

  require_execute_environment
  trap cleanup EXIT
  resolve_source_app

  log "source app: ${SOURCE_APP}"
  log "target app: ${APP_PATH}"
  log "bundle id: ${BUNDLE_ID}"
  log "extension bundle id: ${BUNDLE_ID}.FileProvider"
  log "display name: ${DISPLAY_NAME}"
  log "signing identity: ${SIGNING_IDENTITY}"
  log "reset existing domain: ${RESET_DOMAIN}"
  log "launch after install: ${LAUNCH}"
  log "state isolation: prompt-fresh only; this test app still uses the normal Locality state and app-group entitlements."

  reset_existing_test_app
  install_bundle
}

main "$@"
