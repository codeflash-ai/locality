#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_ROOT="${ROOT}/.build/dev-bundle"
APP="${BUILD_ROOT}/AgentFS.app"
APPEX="${APP}/Contents/PlugIns/AgentFSFileProvider.appex"
ARCH="$(uname -m)"
TARGET="${ARCH}-apple-macos14.0"
SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:--}"

rm -rf "${APP}"
mkdir -p \
  "${APP}/Contents/MacOS" \
  "${APP}/Contents/PlugIns" \
  "${APPEX}/Contents/MacOS"

cp "${ROOT}/App/AgentFS.Info.plist" "${APP}/Contents/Info.plist"
cp "${ROOT}/App/AgentFSFileProvider.Info.plist" "${APPEX}/Contents/Info.plist"

swiftc \
  -target "${TARGET}" \
  -framework AppKit \
  "${ROOT}/App/AgentFSHost.swift" \
  -o "${APP}/Contents/MacOS/AgentFS"

swiftc \
  -target "${TARGET}" \
  -parse-as-library \
  -framework AppKit \
  -framework FileProvider \
  -framework Foundation \
  -Xlinker -sectcreate \
  -Xlinker __TEXT \
  -Xlinker __info_plist \
  -Xlinker "${ROOT}/App/AgentFSFileProviderCtl.Info.plist" \
  "${ROOT}/Sources/AgentFSFileProviderCtl/main.swift" \
  -o "${APP}/Contents/MacOS/agentfs-file-providerctl"

swiftc \
  -target "${TARGET}" \
  -emit-executable \
  -emit-module \
  -emit-module-path "${BUILD_ROOT}/AgentFSFileProvider.swiftmodule" \
  -parse-as-library \
  -module-name AgentFSFileProvider \
  -framework FileProvider \
  -framework Foundation \
  -framework UniformTypeIdentifiers \
  "${ROOT}/App/AgentFSFileProviderMain.c" \
  "${ROOT}"/Sources/AgentFSFileProvider/*.swift \
  -o "${APPEX}/Contents/MacOS/AgentFSFileProvider"

codesign --force --sign "${SIGNING_IDENTITY}" --options runtime \
  --entitlements "${ROOT}/App/AgentFS.entitlements" \
  "${APP}/Contents/MacOS/agentfs-file-providerctl"
codesign --force --sign "${SIGNING_IDENTITY}" --options runtime \
  --entitlements "${ROOT}/App/AgentFSFileProvider.entitlements" \
  "${APPEX}/Contents/MacOS/AgentFSFileProvider"
codesign --force --sign "${SIGNING_IDENTITY}" --options runtime \
  --entitlements "${ROOT}/App/AgentFSFileProvider.entitlements" \
  "${APPEX}"
codesign --force --sign "${SIGNING_IDENTITY}" --options runtime \
  --entitlements "${ROOT}/App/AgentFS.entitlements" \
  "${APP}"

echo "${APP}"
