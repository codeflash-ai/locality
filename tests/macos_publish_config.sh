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
FILE_PROVIDER_BUILD_TEST="${ROOT}/platform/macos/LocalityFileProvider/scripts/build-dev-bundle.test.sh"
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
bash "${FILE_PROVIDER_BUILD_TEST}" >/dev/null
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

source_guard_bin="${tmp_root}/source-guard-bin"
mkdir -p "${source_guard_bin}"
cat >"${source_guard_bin}/uname" <<'STUB'
#!/usr/bin/env bash
printf 'Linux\n'
STUB
chmod +x "${source_guard_bin}/uname"

source_status=0
source_output="$(
  PATH="${source_guard_bin}:${PATH}" source "${PUBLISH_SCRIPT}"
)" || source_status=$?
[[ "${source_status}" == "0" ]] \
  || fail "sourcing publish-macos.sh must not execute main"
[[ -z "${source_output}" ]] \
  || fail "sourcing publish-macos.sh should not produce publish output"

publish_stub_bin="${tmp_root}/publish-stub-bin"
mkdir -p "${publish_stub_bin}"
cat >"${publish_stub_bin}/codesign" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

target="${!#}"
args=" $* "

if [[ "${args}" == *" --entitlements "* ]]; then
  if [[ "${LOCALITY_CODESIGN_ENTITLEMENTS_FAIL:-0}" == "1" ]]; then
    printf 'codesign entitlement inspection failed for %s\n' "${target}" >&2
    exit 42
  fi

  if [[ -n "${LOCALITY_CODESIGN_TESTING_PATH_SUFFIX:-}" && "${target}" == *"${LOCALITY_CODESIGN_TESTING_PATH_SUFFIX}" ]]; then
    cat <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.developer.fileprovider.testing-mode</key>
  <true/>
</dict>
</plist>
PLIST
    exit 0
  fi

  cat <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict/>
</plist>
PLIST
  exit 0
fi

if [[ "${args}" == *" -dv "* || "${args}" == *" --verbose=4 "* ]]; then
  identifier_file="${target}.identifier"
  if [[ -f "${identifier_file}" ]]; then
    identifier="$(<"${identifier_file}")"
  elif [[ "${target}" == *"LocalityFileProvider.appex" ]]; then
    identifier="ai.codeflash.locality.Locality.FileProvider"
  elif [[ "${target}" == *"locality-file-providerctl" ]]; then
    identifier="ai.codeflash.locality.Locality.file-providerctl"
  elif [[ "${target}" == *".app" ]]; then
    identifier="ai.codeflash.locality.Locality"
  else
    identifier="ai.codeflash.locality.Locality.helper"
  fi
  printf 'Identifier=%s\n' "${identifier}" >&2
  exit 0
fi

exit 0
STUB
chmod +x "${publish_stub_bin}/codesign"

run_publish_function_tests() (
  set -euo pipefail
  PATH="${publish_stub_bin}:${PATH}"
  source "${PUBLISH_SCRIPT}"

  test_fail() {
    printf 'macos publish config test: %s\n' "$*" >&2
    exit 1
  }

  expect_publish_failure() {
    local description="$1"
    shift
    local output status
    status=0
    output="$(
      ( "$@" ) 2>&1
    )" || status=$?
    [[ "${status}" != "0" ]] || test_fail "${description}"
    [[ -n "${output}" ]] || test_fail "${description} should explain the failure"
  }

  make_file_provider_fixture() {
    local fixture_root="$1"
    local app="${fixture_root}/Locality.app"
    local appex="${app}/Contents/PlugIns/LocalityFileProvider.appex"
    mkdir -p \
      "${app}/Contents/MacOS" \
      "${app}/Contents/PlugIns" \
      "${appex}/Contents" \
      "${appex}/Contents/MacOS"
    touch \
      "${app}/Contents/MacOS/loc" \
      "${app}/Contents/MacOS/localityd" \
      "${app}/Contents/MacOS/locality-file-providerctl" \
      "${appex}/Contents/MacOS/LocalityFileProvider"
    chmod +x \
      "${app}/Contents/MacOS/loc" \
      "${app}/Contents/MacOS/localityd" \
      "${app}/Contents/MacOS/locality-file-providerctl" \
      "${appex}/Contents/MacOS/LocalityFileProvider"
    printf 'ai.codeflash.locality.Locality\n' >"${app}.identifier"
    printf 'ai.codeflash.locality.Locality.file-providerctl\n' >"${app}/Contents/MacOS/locality-file-providerctl.identifier"
    printf 'ai.codeflash.locality.Locality.FileProvider\n' >"${appex}.identifier"
    cat >"${app}/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>ai.codeflash.locality.Locality</string>
</dict>
</plist>
PLIST
    cat >"${appex}/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>BuildMachineOSBuild</key>
  <string>23F79</string>
  <key>CFBundleIdentifier</key>
  <string>ai.codeflash.locality.Locality.FileProvider</string>
  <key>CFBundleSupportedPlatforms</key>
  <array>
    <string>MacOSX</string>
  </array>
  <key>DTCompiler</key>
  <string>com.apple.compilers.llvm.clang.1_0</string>
  <key>DTPlatformBuild</key>
  <string>24F74</string>
  <key>DTPlatformName</key>
  <string>macosx</string>
  <key>DTPlatformVersion</key>
  <string>15.5</string>
  <key>DTSDKBuild</key>
  <string>24F74</string>
  <key>DTSDKName</key>
  <string>macosx15.5</string>
</dict>
</plist>
PLIST
    printf '%s\n' "${app}"
  }

  good_app="$(make_file_provider_fixture "${tmp_root}/publish-good")"
  assert_file_provider_bundle_metadata "${good_app}"
  assert_no_file_provider_testing_mode "${good_app}"

  bad_containment_app="$(make_file_provider_fixture "${tmp_root}/publish-bad-containment")"
  /usr/libexec/PlistBuddy \
    -c 'Set :CFBundleIdentifier ai.codeflash.locality.FileProvider' \
    "${bad_containment_app}/Contents/PlugIns/LocalityFileProvider.appex/Contents/Info.plist"
  expect_publish_failure \
    "malformed File Provider bundle identifier containment should fail" \
    assert_file_provider_bundle_metadata "${bad_containment_app}"

  missing_sdk_app="$(make_file_provider_fixture "${tmp_root}/publish-missing-sdk")"
  /usr/libexec/PlistBuddy \
    -c 'Delete :DTSDKBuild' \
    "${missing_sdk_app}/Contents/PlugIns/LocalityFileProvider.appex/Contents/Info.plist"
  expect_publish_failure \
    "missing File Provider SDK metadata should fail" \
    assert_file_provider_bundle_metadata "${missing_sdk_app}"

  bad_platform_app="$(make_file_provider_fixture "${tmp_root}/publish-bad-platform")"
  /usr/libexec/PlistBuddy \
    -c 'Set :CFBundleSupportedPlatforms:0 NotMacOSX' \
    "${bad_platform_app}/Contents/PlugIns/LocalityFileProvider.appex/Contents/Info.plist"
  expect_publish_failure \
    "File Provider supported platforms must require an exact MacOSX entry" \
    assert_file_provider_bundle_metadata "${bad_platform_app}"

  entitlement_inspection_failure() {
    LOCALITY_CODESIGN_ENTITLEMENTS_FAIL=1 assert_no_file_provider_testing_mode "$1"
  }

  expect_publish_failure \
    "File Provider entitlement inspection failure should fail closed" \
    entitlement_inspection_failure "${good_app}"

  testing_entitlement_failure() {
    LOCALITY_CODESIGN_TESTING_PATH_SUFFIX='LocalityFileProvider.appex' assert_no_file_provider_testing_mode "$1"
  }

  expect_publish_failure \
    "File Provider testing mode entitlement should fail publish validation" \
    testing_entitlement_failure "${good_app}"
)
run_publish_function_tests

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
