#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAKEFILE="${ROOT}/Makefile"
PUBLISH_SCRIPT="${ROOT}/scripts/publish-macos.sh"
HOMEBREW_SCRIPT="${ROOT}/scripts/render-homebrew-cask.sh"
TAURI_CONF="${ROOT}/apps/desktop/src-tauri/tauri.conf.json"
DMG_BACKGROUND="${ROOT}/apps/desktop/src-tauri/assets/dmg-background.png"
MOUNT_LOGO_ICNS="${ROOT}/apps/desktop/src-tauri/icons/locality-mount-logo.icns"
MOUNT_LOGO_SVG="${ROOT}/apps/desktop/src-tauri/icons/locality-mount-logo.svg"
FILE_PROVIDER_BUILD_SCRIPT="${ROOT}/platform/macos/LocalityFileProvider/scripts/build-dev-bundle.sh"
FILE_PROVIDER_HOST_PLIST="${ROOT}/platform/macos/LocalityFileProvider/App/Locality.Info.plist"
FILE_PROVIDER_EXTENSION_PLIST="${ROOT}/platform/macos/LocalityFileProvider/App/LocalityFileProvider.Info.plist"

fail() {
  printf 'macos publish config test: %s\n' "$*" >&2
  exit 1
}

grep -q '^publish: setup' "${MAKEFILE}" \
  || fail "Makefile is missing publish target"
grep -q '^publish-unnotarized: setup' "${MAKEFILE}" \
  || fail "Makefile is missing publish-unnotarized target"
grep -q 'PUBLISH_SKIP_NOTARIZATION=1 scripts/publish-macos.sh' "${MAKEFILE}" \
  || fail "publish-unnotarized must run publish-macos with PUBLISH_SKIP_NOTARIZATION=1"
grep -q '^test-macos-publish-config:' "${MAKEFILE}" \
  || fail "Makefile is missing test-macos-publish-config target"

grep -q 'skip_notarization()' "${PUBLISH_SCRIPT}" \
  || fail "publish-macos must define skip_notarization"
grep -q 'optional_signing_identity()' "${PUBLISH_SCRIPT}" \
  || fail "publish-macos must allow unnotarized builds without Developer ID signing"
grep -F -q 'signing_identity="$(optional_signing_identity)"' "${PUBLISH_SCRIPT}" \
  || fail "unnotarized publish must use optional signing identity detection"
grep -q 'require_developer_id="0"' "${PUBLISH_SCRIPT}" \
  || fail "unnotarized publish must not require Developer ID signatures"
grep -q 'require_developer_id="1"' "${PUBLISH_SCRIPT}" \
  || fail "notarized publish must still require Developer ID signatures"
grep -q 'PUBLISH_SKIP_NOTARIZATION' "${PUBLISH_SCRIPT}" \
  || fail "publish-macos must read PUBLISH_SKIP_NOTARIZATION"
grep -q 'notary_args' "${PUBLISH_SCRIPT}" \
  || fail "publish-macos must keep notarized publish support"
grep -q 'skipping notarization and stapling' "${PUBLISH_SCRIPT}" \
  || fail "publish-macos must skip notarization and stapling in unnotarized mode"
grep -q 'dmg_status="unnotarized"' "${PUBLISH_SCRIPT}" \
  || fail "unnotarized artifacts must be named distinctly"
grep -q 'dmg_status="notarized"' "${PUBLISH_SCRIPT}" \
  || fail "notarized artifacts must keep the notarized naming suffix"
grep -q 'make new alias file to POSIX file "/Applications"' "${ROOT}/apps/desktop/scripts/postprocess-dmg-volume-icon.sh" \
  || fail "DMG postprocess must replace the Applications symlink with a Finder alias"
grep -F -q 'set background picture of iconOptions to backgroundFile' "${ROOT}/apps/desktop/scripts/postprocess-dmg-volume-icon.sh" \
  || fail "DMG postprocess must actively set the instructional Finder background"
grep -F -q 'set bounds of container window' "${ROOT}/apps/desktop/scripts/postprocess-dmg-volume-icon.sh" \
  || fail "DMG postprocess must actively set the installer Finder window bounds"
grep -F -q 'DMG Finder layout did not create metadata' "${ROOT}/apps/desktop/scripts/postprocess-dmg-volume-icon.sh" \
  || fail "DMG postprocess must fail when Finder does not create layout metadata"
grep -F -q 'strings "${MOUNTPOINT}/.DS_Store" | grep -F -q "${DMG_BACKGROUND_NAME}"' "${ROOT}/apps/desktop/scripts/postprocess-dmg-volume-icon.sh" \
  || fail "DMG postprocess must verify the instructional background metadata is present"

[[ -f "${DMG_BACKGROUND}" ]] \
  || fail "DMG installer background asset is missing"
[[ -f "${MOUNT_LOGO_ICNS}" ]] \
  || fail "mount root ICNS logo asset is missing"
[[ -f "${MOUNT_LOGO_SVG}" ]] \
  || fail "mount root SVG symbol asset is missing"
grep -q '<key>CFBundleIconFile</key>' "${FILE_PROVIDER_HOST_PLIST}" \
  || fail "File Provider host plist must declare CFBundleIconFile"
grep -q '<key>CFBundleIconFile</key>' "${FILE_PROVIDER_EXTENSION_PLIST}" \
  || fail "File Provider extension plist must declare CFBundleIconFile"
grep -q '<key>CFBundleIconName</key>' "${FILE_PROVIDER_HOST_PLIST}" \
  || fail "File Provider host plist must declare CFBundleIconName"
grep -q '<key>CFBundleIconName</key>' "${FILE_PROVIDER_EXTENSION_PLIST}" \
  || fail "File Provider extension plist must declare CFBundleIconName"
grep -q '<key>CFBundleIcons</key>' "${FILE_PROVIDER_HOST_PLIST}" \
  || fail "File Provider host plist must declare CFBundleIcons"
grep -q '<key>CFBundleIcons</key>' "${FILE_PROVIDER_EXTENSION_PLIST}" \
  || fail "File Provider extension plist must declare CFBundleIcons"
grep -q '<key>CFBundleSymbolName</key>' "${FILE_PROVIDER_HOST_PLIST}" \
  || fail "File Provider host plist must declare CFBundleSymbolName for Finder sidebar glyphs"
grep -q '<key>CFBundleSymbolName</key>' "${FILE_PROVIDER_EXTENSION_PLIST}" \
  || fail "File Provider extension plist must declare CFBundleSymbolName for Finder sidebar glyphs"
grep -q '<string>square.stack.3d.up</string>' "${FILE_PROVIDER_HOST_PLIST}" \
  || fail "File Provider host plist must use a tintable Finder sidebar symbol"
grep -q '<string>square.stack.3d.up</string>' "${FILE_PROVIDER_EXTENSION_PLIST}" \
  || fail "File Provider extension plist must use a tintable Finder sidebar symbol"
grep -q '<string>locality-mount-logo</string>' "${FILE_PROVIDER_HOST_PLIST}" \
  || fail "File Provider host plist must use the mount logo icon"
grep -q '<string>locality-mount-logo</string>' "${FILE_PROVIDER_EXTENSION_PLIST}" \
  || fail "File Provider extension plist must use the mount logo icon"
grep -F -q 'Contents/Resources/locality-mount-logo.icns' "${FILE_PROVIDER_BUILD_SCRIPT}" \
  || fail "File Provider build must copy mount logo ICNS into bundle resources"
grep -F -q 'Contents/Resources/locality-mount-logo.svg' "${FILE_PROVIDER_BUILD_SCRIPT}" \
  || fail "File Provider build must copy mount logo SVG into bundle resources"
jq -e '.bundle.macOS.files["Resources/locality-mount-logo.icns"] == "icons/locality-mount-logo.icns"' "${TAURI_CONF}" >/dev/null \
  || fail "Tauri macOS host app must package the mount logo ICNS resource"
jq -e '.bundle.macOS.dmg.background == "assets/dmg-background.png"' "${TAURI_CONF}" >/dev/null \
  || fail "DMG must use the instructional installer background"
jq -e '.bundle.macOS.dmg.windowSize.width >= 720 and .bundle.macOS.dmg.windowSize.height >= 420' "${TAURI_CONF}" >/dev/null \
  || fail "DMG window must leave room for standard install instructions"
jq -e '.bundle.macOS.dmg.appPosition.x < .bundle.macOS.dmg.applicationFolderPosition.x' "${TAURI_CONF}" >/dev/null \
  || fail "DMG app icon must stay left of the Applications shortcut"
jq -e '.bundle.macOS.dmg.appPosition.y == .bundle.macOS.dmg.applicationFolderPosition.y' "${TAURI_CONF}" >/dev/null \
  || fail "DMG app and Applications icons should be horizontally aligned"

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-macos-publish-config.XXXXXX")"
cleanup() {
  rm -rf "${tmp_root}"
}
trap cleanup EXIT

dmg_dir="${tmp_root}/dmg"
cask_output="${tmp_root}/loc.rb"
mkdir -p "${dmg_dir}"
printf 'old notarized dmg\n' >"${dmg_dir}/Locality-release-20260619-abcdefg-notarized-aarch64.dmg"
printf 'new unnotarized dmg\n' >"${dmg_dir}/Locality-release-20260620-abcdefg-unnotarized-aarch64.dmg"

HOMEBREW_DMG_DIR="${dmg_dir}" \
  HOMEBREW_CASK_OUTPUT="${cask_output}" \
  HOMEBREW_RELEASE_TAG="v0.1.0" \
  HOMEBREW_VERSION="0.1.0" \
  "${HOMEBREW_SCRIPT}" >/dev/null

grep -q 'Locality-release-20260619-abcdefg-notarized-aarch64.dmg' "${cask_output}" \
  || fail "Homebrew cask should auto-select notarized DMGs"
if grep -q 'Locality-release-20260620-abcdefg-unnotarized-aarch64.dmg' "${cask_output}"; then
  fail "Homebrew cask must not auto-select unnotarized DMGs"
fi
