#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
DMG="${1:-}"
ICON="${ROOT}/apps/desktop/src-tauri/icons/dmg-icon.icns"

if [[ -z "${DMG}" ]]; then
  DMG="$(find "${ROOT}/target/release/bundle/dmg" -maxdepth 1 -type f -name 'LOCALITY_*.dmg' | sort | tail -n 1)"
fi

if [[ -z "${DMG}" || ! -f "${DMG}" ]]; then
  echo "No Locality DMG found to post-process." >&2
  exit 1
fi
if [[ ! -f "${ICON}" ]]; then
  echo "Missing DMG volume icon: ${ICON}" >&2
  exit 1
fi

TMPDIR="$(mktemp -d)"
MOUNTPOINT="${TMPDIR}/mount"
RW_DMG="${TMPDIR}/loc-installer-rw.dmg"
FINAL_DMG="${TMPDIR}/loc-installer-final.dmg"

cleanup() {
  if [[ -d "${MOUNTPOINT}" ]]; then
    hdiutil detach "${MOUNTPOINT}" -quiet >/dev/null 2>&1 || true
  fi
  rm -rf "${TMPDIR}"
}
trap cleanup EXIT

hdiutil convert "${DMG}" -format UDRW -o "${RW_DMG}" -quiet
mkdir -p "${MOUNTPOINT}"
hdiutil attach "${RW_DMG}" -readwrite -noverify -noautoopen -mountpoint "${MOUNTPOINT}" -quiet

cp "${ICON}" "${MOUNTPOINT}/.VolumeIcon.icns"
if command -v SetFile >/dev/null 2>&1; then
  SetFile -a C "${MOUNTPOINT}"
elif [[ -x /Applications/Xcode.app/Contents/Developer/usr/bin/SetFile ]]; then
  /Applications/Xcode.app/Contents/Developer/usr/bin/SetFile -a C "${MOUNTPOINT}"
else
  xattr -wx com.apple.FinderInfo \
    "0000000000000000040000000000000000000000000000000000000000000000" \
    "${MOUNTPOINT}"
fi

hdiutil detach "${MOUNTPOINT}" -quiet
hdiutil convert "${RW_DMG}" -format UDZO -imagekey zlib-level=9 -o "${FINAL_DMG}" -quiet
mv "${FINAL_DMG}" "${DMG}"
if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  codesign --force --sign "${APPLE_SIGNING_IDENTITY}" "${DMG}"
fi

echo "Applied installer disk icon to ${DMG}"
