#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
OUT="${ROOT}/apps/desktop/src-tauri/linux"

(
  cd "${ROOT}"
  cargo build -p loc-cli -p localityd -p locality-fuse --release
)

mkdir -p "${OUT}"
cp "${ROOT}/target/release/loc" "${OUT}/loc"
cp "${ROOT}/target/release/localityd" "${OUT}/localityd"
cp "${ROOT}/target/release/locality-fuse" "${OUT}/locality-fuse"
chmod 755 "${OUT}/loc" "${OUT}/localityd" "${OUT}/locality-fuse"

echo "Prepared Linux CLI in ${OUT}/loc"
echo "Prepared Linux daemon in ${OUT}/localityd"
echo "Prepared Linux FUSE helper in ${OUT}/locality-fuse"
