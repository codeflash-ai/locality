#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_ROOT="${ROOT}/.build/dev-bundle"
APP="${BUILD_ROOT}/Locality.app"
APPEX="${APP}/Contents/PlugIns/LocalityFileProvider.appex"
ARCH="$(uname -m)"
TARGET="${ARCH}-apple-macos14.0"
SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:--}"

"${ROOT}/scripts/unmount-existing-domains.sh"

rm -rf "${APP}" "${BUILD_ROOT}/Locality.app"
mkdir -p \
  "${APP}/Contents/MacOS" \
  "${APP}/Contents/PlugIns" \
  "${APPEX}/Contents/MacOS"

cp "${ROOT}/App/Locality.Info.plist" "${APP}/Contents/Info.plist"
cp "${ROOT}/App/LocalityFileProvider.Info.plist" "${APPEX}/Contents/Info.plist"

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
  "${ROOT}/Sources/LocalityFileProviderCtl/main.swift" \
  -o "${APP}/Contents/MacOS/locality-file-providerctl"

swiftc \
  -target "${TARGET}" \
  -emit-executable \
  -emit-module \
  -emit-module-path "${BUILD_ROOT}/LocalityFileProvider.swiftmodule" \
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
codesign --force --sign "${SIGNING_IDENTITY}" --options runtime \
  --entitlements "${ROOT}/App/LocalityFileProvider.entitlements" \
  "${APPEX}/Contents/MacOS/LocalityFileProvider"
codesign --force --sign "${SIGNING_IDENTITY}" --options runtime \
  --entitlements "${ROOT}/App/LocalityFileProvider.entitlements" \
  "${APPEX}"
codesign --force --sign "${SIGNING_IDENTITY}" --options runtime \
  --entitlements "${ROOT}/App/Locality.entitlements" \
  "${APP}"

echo "${APP}"
