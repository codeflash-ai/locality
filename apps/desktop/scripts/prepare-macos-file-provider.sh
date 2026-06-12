#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
FILE_PROVIDER_ROOT="${ROOT}/platform/macos/AgentFSFileProvider"
OUT="${ROOT}/apps/desktop/src-tauri/macos/AgentFSFileProvider"

APP="$("${FILE_PROVIDER_ROOT}/scripts/build-dev-bundle.sh")"

rm -rf "${OUT}"
mkdir -p "${OUT}"
cp -R "${APP}/Contents/PlugIns/AgentFSFileProvider.appex" "${OUT}/AgentFSFileProvider.appex"
cp "${APP}/Contents/MacOS/agentfs-file-providerctl" "${OUT}/agentfs-file-providerctl"

echo "Prepared macOS File Provider bundle files in ${OUT}"
