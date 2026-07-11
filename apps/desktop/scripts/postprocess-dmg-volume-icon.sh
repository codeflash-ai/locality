#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
DMG="${1:-}"
ICON="${ROOT}/apps/desktop/src-tauri/icons/dmg-icon.icns"
APPLICATIONS_ICON="/System/Library/CoreServices/CoreTypes.bundle/Contents/Resources/ApplicationsFolderIcon.icns"
APPLICATIONS_ICON_X=550
APPLICATIONS_ICON_Y=240
DMG_BACKGROUND_NAME="dmg-background.png"
WINDOW_LEFT=100
WINDOW_TOP=100
WINDOW_RIGHT=860
WINDOW_BOTTOM=540

if [[ -z "${DMG}" ]]; then
  DMG="$(find "${ROOT}/target/release/bundle/dmg" -maxdepth 1 -type f \( -name 'Locality_*.dmg' -o -name 'LOCALITY_*.dmg' \) | sort | tail -n 1)"
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
MOUNTPOINT=""
RW_DMG="${TMPDIR}/loc-installer-rw.dmg"
FINAL_DMG="${TMPDIR}/loc-installer-final.dmg"
DS_STORE_BACKUP="${TMPDIR}/DS_Store"
ATTACH_OUTPUT="${TMPDIR}/attach.txt"

cleanup() {
  if [[ -n "${MOUNTPOINT}" && -d "${MOUNTPOINT}" ]]; then
    hdiutil detach "${MOUNTPOINT}" -quiet >/dev/null 2>&1 || true
  fi
  rm -rf "${TMPDIR}"
}
trap cleanup EXIT

hdiutil convert "${DMG}" -format UDRW -o "${RW_DMG}" -quiet
hdiutil attach "${RW_DMG}" -readwrite -noverify -noautoopen >"${ATTACH_OUTPUT}"
MOUNTPOINT="$(awk -F '\t' '/\/Volumes\// { print $NF; exit }' "${ATTACH_OUTPUT}")"
if [[ -z "${MOUNTPOINT}" || ! -d "${MOUNTPOINT}" ]]; then
  echo "Could not determine mounted DMG path." >&2
  exit 1
fi

if [[ ! -f "${MOUNTPOINT}/.background/${DMG_BACKGROUND_NAME}" ]]; then
  echo "DMG is missing installer background: ${MOUNTPOINT}/.background/${DMG_BACKGROUND_NAME}" >&2
  exit 1
fi
if [[ -f "${MOUNTPOINT}/.DS_Store" ]] \
  && strings "${MOUNTPOINT}/.DS_Store" | grep -F -q "${DMG_BACKGROUND_NAME}"; then
  cp -p "${MOUNTPOINT}/.DS_Store" "${DS_STORE_BACKUP}"
else
  rm -f "${MOUNTPOINT}/.DS_Store"
fi

if [[ -L "${MOUNTPOINT}/Applications" || ! -e "${MOUNTPOINT}/Applications" ]]; then
  rm -f "${MOUNTPOINT}/Applications"
  osascript >/dev/null <<OSA
set mountFolder to POSIX file "${MOUNTPOINT}" as alias
tell application "Finder"
  make new alias file to POSIX file "/Applications" at mountFolder with properties {name:"Applications"}
end tell
OSA
fi
if [[ -f "${MOUNTPOINT}/Applications" && -f "${APPLICATIONS_ICON}" ]]; then
  applications_icon_copy="${TMPDIR}/ApplicationsFolderIcon.icns"
  applications_icon_resource="${TMPDIR}/ApplicationsFolderIcon.rsrc"
  cp "${APPLICATIONS_ICON}" "${applications_icon_copy}"
  sips -i "${applications_icon_copy}" >/dev/null
  DeRez -only icns "${applications_icon_copy}" >"${applications_icon_resource}"
  Rez -append "${applications_icon_resource}" -o "${MOUNTPOINT}/Applications"
  SetFile -a C "${MOUNTPOINT}/Applications"
fi
if [[ -f "${DS_STORE_BACKUP}" ]]; then
  cp -p "${DS_STORE_BACKUP}" "${MOUNTPOINT}/.DS_Store"
fi

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

if [[ ! -f "${DS_STORE_BACKUP}" ]]; then
  VOLUME_NAME="$(diskutil info "${MOUNTPOINT}" | sed -n 's/.*Volume Name:[[:space:]]*//p' | head -n 1)"
  if [[ -z "${VOLUME_NAME}" ]]; then
    echo "Could not determine mounted DMG volume name for Finder layout." >&2
    exit 1
  fi
  osascript >/dev/null <<OSA
set backgroundFile to POSIX file "${MOUNTPOINT}/.background/${DMG_BACKGROUND_NAME}" as alias
tell application "Finder"
  tell disk "${VOLUME_NAME}"
    open
    set current view of container window to icon view
    set bounds of container window to {${WINDOW_LEFT}, ${WINDOW_TOP}, ${WINDOW_RIGHT}, ${WINDOW_BOTTOM}}
    set iconOptions to icon view options of container window
    set arrangement of iconOptions to not arranged
    set icon size of iconOptions to 96
    set background picture of iconOptions to backgroundFile
    set position of item "Locality.app" to {210, 240}
    set position of item "Applications" to {${APPLICATIONS_ICON_X}, ${APPLICATIONS_ICON_Y}}
    update without registering applications
  end tell
  delay 3
end tell
OSA
fi

if [[ ! -f "${MOUNTPOINT}/.DS_Store" ]]; then
  echo "DMG Finder layout did not create metadata: ${MOUNTPOINT}/.DS_Store" >&2
  exit 1
fi
if ! strings "${MOUNTPOINT}/.DS_Store" | grep -F -q "${DMG_BACKGROUND_NAME}"; then
  echo "DMG Finder layout does not reference ${DMG_BACKGROUND_NAME}; refusing to ship a generic installer window." >&2
  exit 1
fi

hdiutil detach "${MOUNTPOINT}" -quiet
hdiutil convert "${RW_DMG}" -format UDZO -imagekey zlib-level=9 -o "${FINAL_DMG}" -quiet
mv "${FINAL_DMG}" "${DMG}"
if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  codesign --force --sign "${APPLE_SIGNING_IDENTITY}" "${DMG}"
fi

echo "Applied installer disk icon to ${DMG}"
