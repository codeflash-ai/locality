#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
FILE_PROVIDER_ROOT="${ROOT}/platform/macos/LocalityFileProvider"
OUT="${ROOT}/apps/desktop/src-tauri/macos/LocalityFileProvider"
LOCALITYD_OUT="${ROOT}/apps/desktop/src-tauri/macos/localityd"
LOCALITY_OUT="${ROOT}/apps/desktop/src-tauri/macos/loc"

(
  cd "${ROOT}"
  cargo build -p loc-cli -p localityd --release
)
APP="$("${FILE_PROVIDER_ROOT}/scripts/build-dev-bundle.sh")"

rm -rf "${OUT}"
mkdir -p "${OUT}"
cp -R "${APP}/Contents/PlugIns/LocalityFileProvider.appex" "${OUT}/LocalityFileProvider.appex"
cp "${APP}/Contents/MacOS/locality-file-providerctl" "${OUT}/locality-file-providerctl"
cp "${ROOT}/target/release/localityd" "${LOCALITYD_OUT}"
cp "${ROOT}/target/release/loc" "${LOCALITY_OUT}"
if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  codesign --force --sign "${APPLE_SIGNING_IDENTITY}" --options runtime "${LOCALITYD_OUT}"
  codesign --force --sign "${APPLE_SIGNING_IDENTITY}" --options runtime "${LOCALITY_OUT}"
fi

echo "Prepared macOS File Provider bundle files in ${OUT}"
echo "Prepared localityd sidecar in ${LOCALITYD_OUT}"
echo "Prepared loc CLI in ${LOCALITY_OUT}"
