#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DESKTOP_DIR="${ROOT}/apps/desktop"
DMG_DIR="${ROOT}/target/release/bundle/dmg"
UPDATER_DIR="${ROOT}/target/release/bundle/updater"
PRODUCT_NAME="${PUBLISH_PRODUCT_NAME:-Locality}"
CHANNEL="${PUBLISH_CHANNEL:-beta}"
DATE_STAMP="${PUBLISH_DATE:-$(date +%Y%m%d)}"
NOTARY_PROFILE="${APPLE_NOTARY_KEYCHAIN_PROFILE:-${NOTARY_KEYCHAIN_PROFILE:-loc-notary}}"
UPDATER_ENDPOINT="${TAURI_UPDATER_ENDPOINT:-https://github.com/codeflash-ai/locality/releases/latest/download/latest-macos.json}"
HOST_ENTITLEMENTS="${ROOT}/platform/macos/LocalityFileProvider/App/LocalityDeveloperId.entitlements"
HOST_APP_GROUP="C484HB7Q6S.group.ai.codeflash.locality"
PLISTBUDDY="/usr/libexec/PlistBuddy"
FILE_PROVIDER_TESTING_ENTITLEMENT="com.apple.developer.fileprovider.testing-mode"

log() {
  printf 'publish: %s\n' "$*"
}

fail() {
  printf 'publish: error: %s\n' "$*" >&2
  exit 1
}

skip_notarization() {
  [[ "${PUBLISH_SKIP_NOTARIZATION:-0}" == "1" ]]
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

require_plistbuddy() {
  [[ -x "${PLISTBUDDY}" ]] || fail "missing required command: ${PLISTBUDDY}"
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

optional_signing_identity() {
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

  printf '%s\n' "-"
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

  return 1
}

notary_credentials_error() {
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

assert_arm64_host() {
  if [[ "${PUBLISH_ALLOW_INTEL:-0}" == "1" ]]; then
    return 0
  fi
  case "$(uname -m)" in
    arm64|aarch64) ;;
    *) fail "macOS publishing is Apple Silicon-only; set PUBLISH_ALLOW_INTEL=1 for a local unsupported Intel build" ;;
  esac
}

latest_tauri_dmg() {
  find "${DMG_DIR}" -maxdepth 1 -type f -name "${PRODUCT_NAME}_*.dmg" | sort | tail -n 1
}

updater_enabled() {
  [[ -n "${TAURI_UPDATER_PUBKEY:-}" ]]
}

build_config_json() {
  local signing_identity="$1"
  local escaped_identity escaped_entitlements
  escaped_identity="$(json_escape "${signing_identity}")"
  escaped_entitlements="$(json_escape "${HOST_ENTITLEMENTS}")"

  if updater_enabled; then
    [[ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" ]] \
      || fail "TAURI_UPDATER_PUBKEY is set but TAURI_SIGNING_PRIVATE_KEY is missing"
    printf '{"bundle":{"createUpdaterArtifacts":true,"macOS":{"signingIdentity":"%s","entitlements":"%s"}},"plugins":{"updater":{"pubkey":"%s","endpoints":["%s"]}}}' \
      "${escaped_identity}" \
      "${escaped_entitlements}" \
      "$(json_escape "${TAURI_UPDATER_PUBKEY}")" \
      "$(json_escape "${UPDATER_ENDPOINT}")"
    return 0
  fi

  printf '{"bundle":{"macOS":{"signingIdentity":"%s","entitlements":"%s"}}}' \
    "${escaped_identity}" \
    "${escaped_entitlements}"
}

latest_updater_archive() {
  find "${ROOT}/target/release/bundle" -type f -name '*.app.tar.gz' | sort | tail -n 1
}

copy_updater_artifacts() {
  local archive base arch output_name final_archive
  archive="$(latest_updater_archive)"
  [[ -n "${archive}" && -f "${archive}" ]] || fail "Tauri did not produce a macOS updater archive"
  [[ -f "${archive}.sig" ]] || fail "Tauri did not produce ${archive}.sig"

  base="$(basename "${archive}" .app.tar.gz)"
  if [[ "${base}" == *_* ]]; then
    arch="${base##*_}"
  else
    arch="$(uname -m)"
  fi
  output_name="${PUBLISH_UPDATER_NAME:-${PRODUCT_NAME}-${CHANNEL}-${DATE_STAMP}-${commit_short}-macos-${arch}.app.tar.gz}"
  final_archive="${UPDATER_DIR}/${output_name}"
  mkdir -p "${UPDATER_DIR}"
  cp "${archive}" "${final_archive}"
  cp "${archive}.sig" "${final_archive}.sig"
  printf '\nPublished updater archive: %s\n' "${final_archive}"
  printf 'Published updater signature: %s.sig\n' "${final_archive}"
}

assert_app_group_entitlement() {
  local path="$1"
  local entitlements
  entitlements="$(codesign -d --entitlements - "${path}" 2>/dev/null || true)"
  [[ "${entitlements}" == *"com.apple.security.application-groups"* ]] \
    || fail "${path} is missing com.apple.security.application-groups entitlement"
  [[ "${entitlements}" == *"${HOST_APP_GROUP}"* ]] \
    || fail "${path} is missing ${HOST_APP_GROUP} entitlement"
}

plist_print() {
  "${PLISTBUDDY}" -c "Print :$2" "$1" 2>/dev/null
}

assert_plist_string() {
  local plist="$1"
  local key="$2"
  local value
  value="$(plist_print "${plist}" "${key}")" \
    || fail "${plist} is missing ${key}"
  [[ -n "${value}" ]] || fail "${plist} has empty ${key}"
  printf '%s\n' "${value}"
}

assert_identifier_contained_by_app() {
  local child_identifier="$1"
  local app_identifier="$2"
  local label="$3"
  [[ "${child_identifier}" == "${app_identifier}."* ]] \
    || fail "${label} bundle identifier ${child_identifier} is not contained by ${app_identifier}"
}

signed_code_identifier() {
  local path="$1"
  local signature identifier
  signature="$(codesign -dv --verbose=4 "${path}" 2>&1)" \
    || fail "could not inspect code signature identifier for ${path}"
  identifier="$(sed -n 's/^Identifier=//p' <<<"${signature}" | head -n 1)"
  [[ -n "${identifier}" ]] \
    || fail "could not determine code signature identifier for ${path}"
  printf '%s\n' "${identifier}"
}

assert_supported_platforms_include_macos() {
  local plist="$1"
  local index platform
  plist_print "${plist}" CFBundleSupportedPlatforms >/dev/null \
    || fail "${plist} is missing CFBundleSupportedPlatforms"
  index=0
  while platform="$(plist_print "${plist}" "CFBundleSupportedPlatforms:${index}")"; do
    if [[ "${platform}" == "MacOSX" ]]; then
      return 0
    fi
    index=$((index + 1))
  done
  fail "${plist} does not declare MacOSX support"
}

assert_file_provider_bundle_metadata() {
  local app="$1"
  local appex="${app}/Contents/PlugIns/LocalityFileProvider.appex"
  local helper="${app}/Contents/MacOS/locality-file-providerctl"
  local app_plist="${app}/Contents/Info.plist"
  local appex_plist="${appex}/Contents/Info.plist"
  local app_identifier appex_identifier helper_identifier compiler platform_name platform_version sdk_name

  require_plistbuddy
  [[ -f "${app_plist}" ]] || fail "${app} is missing Contents/Info.plist"
  [[ -f "${appex_plist}" ]] || fail "${appex} is missing Contents/Info.plist"
  [[ -e "${helper}" ]] || fail "${app} is missing locality-file-providerctl"

  app_identifier="$(assert_plist_string "${app_plist}" CFBundleIdentifier)"
  appex_identifier="$(assert_plist_string "${appex_plist}" CFBundleIdentifier)"
  helper_identifier="$(signed_code_identifier "${helper}")"
  assert_identifier_contained_by_app "${appex_identifier}" "${app_identifier}" "LocalityFileProvider.appex"
  assert_identifier_contained_by_app "${helper_identifier}" "${app_identifier}" "locality-file-providerctl"

  assert_supported_platforms_include_macos "${appex_plist}"

  assert_plist_string "${appex_plist}" BuildMachineOSBuild >/dev/null
  assert_plist_string "${appex_plist}" DTPlatformBuild >/dev/null
  assert_plist_string "${appex_plist}" DTSDKBuild >/dev/null
  compiler="$(assert_plist_string "${appex_plist}" DTCompiler)"
  platform_name="$(assert_plist_string "${appex_plist}" DTPlatformName)"
  platform_version="$(assert_plist_string "${appex_plist}" DTPlatformVersion)"
  sdk_name="$(assert_plist_string "${appex_plist}" DTSDKName)"

  [[ "${compiler}" == "com.apple.compilers.llvm.clang.1_0" ]] \
    || fail "LocalityFileProvider.appex has unexpected DTCompiler ${compiler}"
  [[ "${platform_name}" == "macosx" ]] \
    || fail "LocalityFileProvider.appex has unexpected DTPlatformName ${platform_name}"
  [[ "${platform_version}" =~ ^[0-9]+([.][0-9]+)*$ ]] \
    || fail "LocalityFileProvider.appex has invalid DTPlatformVersion ${platform_version}"
  [[ "${sdk_name}" == macosx* ]] \
    || fail "LocalityFileProvider.appex has invalid DTSDKName ${sdk_name}"
}

assert_no_file_provider_testing_mode_for_path() {
  local path="$1"
  local entitlements
  entitlements="$(codesign -d --entitlements - "${path}" 2>/dev/null)" \
    || fail "could not inspect entitlements for ${path}"
  [[ "${entitlements}" != *"${FILE_PROVIDER_TESTING_ENTITLEMENT}"* ]] \
    || fail "${path} carries ${FILE_PROVIDER_TESTING_ENTITLEMENT}"
}

assert_no_file_provider_testing_mode() {
  local app="$1"
  local appex="${app}/Contents/PlugIns/LocalityFileProvider.appex"
  local helper="${app}/Contents/MacOS/locality-file-providerctl"
  local path
  [[ -e "${app}" ]] || fail "missing app for entitlement inspection: ${app}"
  [[ -e "${appex}" ]] || fail "missing File Provider appex for entitlement inspection: ${appex}"
  [[ -e "${helper}" ]] || fail "missing File Provider helper for entitlement inspection: ${helper}"

  local -a paths=("${app}" "${helper}" "${appex}")
  for path in "${app}/Contents/MacOS/loc" "${app}/Contents/MacOS/localityd"; do
    if [[ -e "${path}" ]]; then
      paths+=("${path}")
    fi
  done

  for path in "${paths[@]}"; do
    assert_no_file_provider_testing_mode_for_path "${path}"
  done
}

verify_signed_app_in_dmg() (
  local dmg="$1"
  local expected_build="$2"
  local require_developer_id="$3"
  local tmpdir mountpoint app
  tmpdir="$(mktemp -d)"
  mountpoint="${tmpdir}/mount"
  mkdir -p "${mountpoint}"

  cleanup() {
    hdiutil detach "${mountpoint}" -quiet >/dev/null 2>&1 \
      || hdiutil detach "${mountpoint}" -force -quiet >/dev/null 2>&1 \
      || true
    rm -rf "${tmpdir}" >/dev/null 2>&1 || true
  }
  trap cleanup EXIT

  hdiutil attach "${dmg}" -readonly -noverify -noautoopen -mountpoint "${mountpoint}" -quiet
  app="${mountpoint}/${PRODUCT_NAME}.app"
  codesign --verify --deep --strict --verbose=2 "${app}"
  [[ -x "${app}/Contents/MacOS/loc" ]] \
    || fail "${PRODUCT_NAME}.app does not include an executable loc CLI"
  [[ -x "${app}/Contents/MacOS/localityd" ]] \
    || fail "${PRODUCT_NAME}.app does not include an executable localityd sidecar"
  codesign --verify --strict --verbose=2 "${app}/Contents/MacOS/locality-file-providerctl"
  codesign --verify --deep --strict --verbose=2 "${app}/Contents/PlugIns/LocalityFileProvider.appex"
  local app_signature appex_signature
  app_signature="$(codesign -dv --verbose=4 "${app}" 2>&1)"
  appex_signature="$(codesign -dv --verbose=4 "${app}/Contents/PlugIns/LocalityFileProvider.appex" 2>&1)"
  if [[ "${require_developer_id}" == "1" ]]; then
    [[ "${app_signature}" == *"Developer ID Application"* ]] \
      || fail "${PRODUCT_NAME}.app is not signed with a Developer ID Application identity"
    [[ "${appex_signature}" == *"Developer ID Application"* ]] \
      || fail "LocalityFileProvider.appex is not signed with a Developer ID Application identity"
    assert_app_group_entitlement "${app}"
    assert_app_group_entitlement "${app}/Contents/MacOS/loc"
    assert_app_group_entitlement "${app}/Contents/MacOS/localityd"
    assert_app_group_entitlement "${app}/Contents/MacOS/locality-file-providerctl"
  fi
  assert_file_provider_bundle_metadata "${app}"
  assert_no_file_provider_testing_mode "${app}"
  grep -a -F -q "${expected_build}" "${app}/Contents/MacOS/localityd"
  smoke_test_desktop_app "${app}"
)

smoke_test_desktop_app() (
  local app="$1"
  local tmpdir stdout stderr pid status
  tmpdir="$(mktemp -d)"
  stdout="${tmpdir}/stdout.log"
  stderr="${tmpdir}/stderr.log"

  cleanup() {
    if [[ -n "${pid:-}" ]] && kill -0 "${pid}" >/dev/null 2>&1; then
      kill "${pid}" >/dev/null 2>&1 || true
      wait "${pid}" >/dev/null 2>&1 || true
    fi
    rm -rf "${tmpdir}"
  }
  trap cleanup EXIT

  LOCALITY_DESKTOP_SMOKE_TEST=1 "${app}/Contents/MacOS/locality-desktop" >"${stdout}" 2>"${stderr}" &
  pid="$!"

  for _ in {1..20}; do
    if ! kill -0 "${pid}" >/dev/null 2>&1; then
      if wait "${pid}"; then
        return 0
      fi
      status="$?"
      cat "${stdout}" >&2 || true
      cat "${stderr}" >&2 || true
      fail "${PRODUCT_NAME}.app launch smoke test failed with exit code ${status}"
    fi
    sleep 0.5
  done

  cat "${stdout}" >&2 || true
  cat "${stderr}" >&2 || true
  fail "${PRODUCT_NAME}.app launch smoke test did not exit"
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
      hdiutil detach "${mountpoint}" -quiet >/dev/null 2>&1 \
        || hdiutil detach "${mountpoint}" -force -quiet >/dev/null 2>&1 \
        || true
      rm -rf "${tmpdir}" >/dev/null 2>&1 || true
    }
    trap cleanup EXIT

    hdiutil attach "${dmg}" -readonly -noverify -noautoopen -mountpoint "${mountpoint}" -quiet
    app="${mountpoint}/${PRODUCT_NAME}.app"
    codesign --verify --deep --strict --verbose=2 "${app}"
    [[ -x "${app}/Contents/MacOS/loc" ]] \
      || fail "${PRODUCT_NAME}.app does not include an executable loc CLI"
    [[ -x "${app}/Contents/MacOS/localityd" ]] \
      || fail "${PRODUCT_NAME}.app does not include an executable localityd sidecar"
    spctl --assess --type execute --verbose "${app}"
  )
}

main() {
  [[ "$(uname -s)" == "Darwin" ]] || fail "macOS publishing must run on macOS"
  assert_arm64_host
  require_command git
  require_command npm
  require_command cargo
  require_command hdiutil
  require_command codesign
  require_command security
  require_command strings
  require_plistbuddy
  if ! skip_notarization; then
    require_command xcrun
    require_command spctl
  fi

  assert_clean_tree

  local signing_identity commit_short commit_full config_json dmg arch output_name final_dmg sha dmg_status require_developer_id
  if skip_notarization; then
    signing_identity="$(optional_signing_identity)"
    require_developer_id="0"
  else
    signing_identity="$(detect_signing_identity)"
    require_developer_id="1"
  fi
  commit_short="$(git -C "${ROOT}" rev-parse --short=7 HEAD)"
  commit_full="$(git -C "${ROOT}" rev-parse --short=12 HEAD)"
  config_json="$(build_config_json "${signing_identity}")"

  local -a submit_args=()
  if ! skip_notarization; then
    while IFS= read -r -d '' arg; do
      submit_args+=("${arg}")
    done < <(notary_args)
    [[ "${#submit_args[@]}" -gt 0 ]] || notary_credentials_error
  fi

  log "commit ${commit_full}"
  if [[ "${signing_identity}" == "-" ]]; then
    log "signing identity: ad-hoc"
  else
    log "signing identity: ${signing_identity}"
  fi
  if skip_notarization; then
    log "notarization disabled"
  else
    log "notary profile: ${NOTARY_PROFILE}"
  fi
  if updater_enabled; then
    log "updater endpoint: ${UPDATER_ENDPOINT}"
  else
    log "updater artifacts disabled; set TAURI_UPDATER_PUBKEY and TAURI_SIGNING_PRIVATE_KEY to enable"
  fi

  mkdir -p "${DMG_DIR}"
  rm -f "${DMG_DIR}/${PRODUCT_NAME}_"*.dmg
  rm -rf "${UPDATER_DIR}"
  rm -rf "${ROOT}/target/release/bundle/macos/${PRODUCT_NAME}.app"

  local bundle_targets
  bundle_targets="dmg"
  if updater_enabled; then
    bundle_targets="app,dmg"
  fi

  log "building signed Tauri bundle targets: ${bundle_targets}"
  APPLE_SIGNING_IDENTITY="${signing_identity}" \
    npm --prefix "${DESKTOP_DIR}" run tauri -- build --bundles "${bundle_targets}" --config "${config_json}"

  dmg="$(latest_tauri_dmg)"
  [[ -n "${dmg}" && -f "${dmg}" ]] || fail "Tauri did not produce a ${PRODUCT_NAME}_*.dmg artifact"

  log "applying installer disk icon"
  APPLE_SIGNING_IDENTITY="${signing_identity}" \
    bash "${DESKTOP_DIR}/scripts/postprocess-dmg-volume-icon.sh" "${dmg}"

  log "verifying app signatures"
  verify_signed_app_in_dmg "${dmg}" "${commit_full}" "${require_developer_id}"

  if skip_notarization; then
    log "skipping notarization and stapling"
    dmg_status="unnotarized"
  else
    log "submitting for notarization"
    xcrun notarytool submit "${dmg}" --wait "${submit_args[@]}"

    log "stapling notarization ticket"
    xcrun stapler staple "${dmg}"
    dmg_status="notarized"
  fi

  arch="$(basename "${dmg}" .dmg)"
  arch="${arch##*_}"
  output_name="${PUBLISH_DMG_NAME:-${PRODUCT_NAME}-${CHANNEL}-${DATE_STAMP}-${commit_short}-${dmg_status}-${arch}.dmg}"
  final_dmg="${DMG_DIR}/${output_name}"
  cp "${dmg}" "${final_dmg}"

  if skip_notarization; then
    log "validating DMG integrity"
    hdiutil verify "${final_dmg}"
  else
    log "validating notarized DMG"
    validate_notarized_dmg "${final_dmg}"
  fi

  sha="$(shasum -a 256 "${final_dmg}" | awk '{print $1}')"
  printf '\nPublished DMG: %s\n' "${final_dmg}"
  printf 'SHA256: %s\n' "${sha}"

  if updater_enabled; then
    copy_updater_artifacts
  fi
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  main "$@"
fi
