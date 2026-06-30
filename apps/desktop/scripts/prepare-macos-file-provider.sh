#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
FILE_PROVIDER_ROOT="${ROOT}/platform/macos/LocalityFileProvider"
HOST_ENTITLEMENTS="${FILE_PROVIDER_ROOT}/App/LocalityDeveloperId.entitlements"
OUT="${ROOT}/apps/desktop/src-tauri/macos/LocalityFileProvider"
LOCALITYD_OUT="${ROOT}/apps/desktop/src-tauri/macos/localityd"
LOCALITY_OUT="${ROOT}/apps/desktop/src-tauri/macos/loc"

(
  cd "${ROOT}"
  cargo build -p loc-cli -p localityd --release
)
node "${ROOT}/apps/desktop/scripts/stop-daemon-for-build.mjs" --loc "${ROOT}/target/release/loc"
APP="$("${FILE_PROVIDER_ROOT}/scripts/build-dev-bundle.sh")"

rm -rf "${OUT}"
mkdir -p "${OUT}"
cp -R "${APP}/Contents/PlugIns/LocalityFileProvider.appex" "${OUT}/LocalityFileProvider.appex"
cp "${APP}/Contents/MacOS/locality-file-providerctl" "${OUT}/locality-file-providerctl"
cp "${ROOT}/target/release/localityd" "${LOCALITYD_OUT}"
cp "${ROOT}/target/release/loc" "${LOCALITY_OUT}"
if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  codesign --force --sign "${APPLE_SIGNING_IDENTITY}" --options runtime \
    --entitlements "${HOST_ENTITLEMENTS}" \
    "${LOCALITYD_OUT}"
  codesign --force --sign "${APPLE_SIGNING_IDENTITY}" --options runtime \
    --entitlements "${HOST_ENTITLEMENTS}" \
    "${LOCALITY_OUT}"
fi

echo "Prepared macOS File Provider bundle files in ${OUT}"
echo "Prepared localityd sidecar in ${LOCALITYD_OUT}"
echo "Prepared loc CLI in ${LOCALITY_OUT}"
