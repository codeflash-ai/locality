#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "${ROOT}/../../.." && pwd)"
BUILD_ROOT="${LOCALITY_FILE_PROVIDER_BUILD_ROOT:-${ROOT}/.build/dev-bundle}"
APP="${BUILD_ROOT}/Locality.app"
APPEX="${APP}/Contents/PlugIns/LocalityFileProvider.appex"
APPEX_PLIST="${APPEX}/Contents/Info.plist"
MOUNT_LOGO_ICNS="${REPO_ROOT}/apps/desktop/src-tauri/icons/locality-mount-logo.icns"
MOUNT_LOGO_SVG="${REPO_ROOT}/apps/desktop/src-tauri/icons/locality-mount-logo.svg"
ARCH="$(uname -m)"
TARGET="${ARCH}-apple-macos14.0"
SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:--}"
PLISTBUDDY="/usr/libexec/PlistBuddy"

fail() {
  printf 'build-dev-bundle: error: %s\n' "$*" >&2
  exit 1
}

plist_print() {
  "${PLISTBUDDY}" -c "Print :$2" "$1" 2>/dev/null
}

plist_set_string() {
  local plist="$1"
  local key="$2"
  local value="$3"
  "${PLISTBUDDY}" -c "Delete :${key}" "${plist}" >/dev/null 2>&1 || true
  "${PLISTBUDDY}" -c "Add :${key} string ${value}" "${plist}" >/dev/null
}

sdk_setting() {
  local sdk_settings="$1"
  local key="$2"
  plist_print "${sdk_settings}" "${key}" \
    || plist_print "${sdk_settings}" "DefaultProperties:${key}"
}

stage_appex_metadata() {
  local plist="$1"
  local sdk_path sdk_settings build_machine_os_build sdk_version sdk_name sdk_build

  [[ -x "${PLISTBUDDY}" ]] || fail "missing required command: ${PLISTBUDDY}"
  build_machine_os_build="$(sw_vers -buildVersion)"
  sdk_path="$(xcrun --sdk macosx --show-sdk-path)"
  sdk_settings="${sdk_path}/SDKSettings.plist"
  [[ -f "${sdk_settings}" ]] || fail "missing SDKSettings.plist at ${sdk_settings}"

  sdk_version="$(sdk_setting "${sdk_settings}" Version || true)"
  [[ -n "${sdk_version}" ]] || sdk_version="$(xcrun --sdk macosx --show-sdk-version)"
  sdk_name="$(sdk_setting "${sdk_settings}" CanonicalName || true)"
  [[ -n "${sdk_name}" ]] || sdk_name="macosx${sdk_version}"
  sdk_build="$(sdk_setting "${sdk_settings}" ProductBuildVersion || true)"
  [[ -n "${sdk_build}" ]] || sdk_build="$(xcrun --sdk macosx --show-sdk-build-version)"
  [[ -n "${sdk_build}" ]] || fail "could not determine macOS SDK build version"

  plist_set_string "${plist}" BuildMachineOSBuild "${build_machine_os_build}"
  plist_set_string "${plist}" DTCompiler "com.apple.compilers.llvm.clang.1_0"
  plist_set_string "${plist}" DTPlatformBuild "${sdk_build}"
  plist_set_string "${plist}" DTPlatformName "macosx"
  plist_set_string "${plist}" DTPlatformVersion "${sdk_version}"
  plist_set_string "${plist}" DTSDKBuild "${sdk_build}"
  plist_set_string "${plist}" DTSDKName "${sdk_name}"
}

rm -rf "${APP}"
mkdir -p \
  "${APP}/Contents/MacOS" \
  "${APP}/Contents/Resources" \
  "${APP}/Contents/PlugIns" \
  "${APPEX}/Contents/MacOS" \
  "${APPEX}/Contents/Resources"

cp "${ROOT}/App/Locality.Info.plist" "${APP}/Contents/Info.plist"
cp "${ROOT}/App/LocalityFileProvider.Info.plist" "${APPEX_PLIST}"
cp "${MOUNT_LOGO_ICNS}" "${APP}/Contents/Resources/locality-mount-logo.icns"
cp "${MOUNT_LOGO_ICNS}" "${APPEX}/Contents/Resources/locality-mount-logo.icns"
cp "${MOUNT_LOGO_SVG}" "${APP}/Contents/Resources/locality-mount-logo.svg"
cp "${MOUNT_LOGO_SVG}" "${APPEX}/Contents/Resources/locality-mount-logo.svg"
stage_appex_metadata "${APPEX_PLIST}"

swiftc \
  -target "${TARGET}" \
  -framework AppKit \
  "${ROOT}/App/LocalityHost.swift" \
  -o "${APP}/Contents/MacOS/Locality"

swiftc \
  -target "${TARGET}" \
  -parse-as-library \
  -framework AppKit \
  -framework FileProvider \
  -framework Foundation \
  -Xlinker -sectcreate \
  -Xlinker __TEXT \
  -Xlinker __info_plist \
  -Xlinker "${ROOT}/App/LocalityFileProviderCtl.Info.plist" \
  "${ROOT}"/Sources/LocalityFileProviderCtl/*.swift \
  -o "${APP}/Contents/MacOS/locality-file-providerctl"

swiftc \
  -target "${TARGET}" \
  -emit-executable \
  -emit-module \
  -emit-module-path "${BUILD_ROOT}/LocalityFileProvider.swiftmodule" \
  -application-extension \
  -Xcc -fapplication-extension \
  -parse-as-library \
  -module-name LocalityFileProvider \
  -framework FileProvider \
  -framework Foundation \
  -framework UniformTypeIdentifiers \
  "${ROOT}/App/LocalityFileProviderMain.c" \
  "${ROOT}"/Sources/LocalityFileProvider/*.swift \
  -o "${APPEX}/Contents/MacOS/LocalityFileProvider"

codesign --force --sign "${SIGNING_IDENTITY}" --options runtime \
  --entitlements "${ROOT}/App/Locality.entitlements" \
  "${APP}/Contents/MacOS/locality-file-providerctl"
codesign --verify --strict --verbose=2 \
  "${APP}/Contents/MacOS/locality-file-providerctl"
codesign --force --sign "${SIGNING_IDENTITY}" --options runtime \
  --entitlements "${ROOT}/App/LocalityFileProvider.entitlements" \
  "${APPEX}/Contents/MacOS/LocalityFileProvider"
codesign --verify --strict --verbose=2 \
  "${APPEX}/Contents/MacOS/LocalityFileProvider"
codesign --force --sign "${SIGNING_IDENTITY}" --options runtime \
  --entitlements "${ROOT}/App/LocalityFileProvider.entitlements" \
  "${APPEX}"
codesign --verify --deep --strict --verbose=2 \
  "${APPEX}"
codesign --force --sign "${SIGNING_IDENTITY}" --options runtime \
  --entitlements "${ROOT}/App/Locality.entitlements" \
  "${APP}"
codesign --verify --deep --strict --verbose=2 \
  "${APP}"

echo "${APP}"
