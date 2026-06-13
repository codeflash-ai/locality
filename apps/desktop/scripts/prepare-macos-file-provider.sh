#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
FILE_PROVIDER_ROOT="${ROOT}/platform/macos/AgentFSFileProvider"
OUT="${ROOT}/apps/desktop/src-tauri/macos/AgentFSFileProvider"
AFSD_OUT="${ROOT}/apps/desktop/src-tauri/macos/afsd"

(
  cd "${ROOT}"
  cargo build -p afsd --release
)
APP="$("${FILE_PROVIDER_ROOT}/scripts/build-dev-bundle.sh")"

rm -rf "${OUT}"
mkdir -p "${OUT}"
cp -R "${APP}/Contents/PlugIns/AgentFSFileProvider.appex" "${OUT}/AgentFSFileProvider.appex"
cp "${APP}/Contents/MacOS/agentfs-file-providerctl" "${OUT}/agentfs-file-providerctl"
cp "${ROOT}/target/release/afsd" "${AFSD_OUT}"
if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  codesign --force --sign "${APPLE_SIGNING_IDENTITY}" --options runtime "${AFSD_OUT}"
fi

echo "Prepared macOS File Provider bundle files in ${OUT}"
echo "Prepared afsd sidecar in ${AFSD_OUT}"
