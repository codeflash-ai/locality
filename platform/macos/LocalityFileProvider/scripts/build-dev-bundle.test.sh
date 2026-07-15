#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "${ROOT}/../../.." && pwd)"
BUILD_SCRIPT="${ROOT}/scripts/build-dev-bundle.sh"
PLISTBUDDY="/usr/libexec/PlistBuddy"
TESTING_ENTITLEMENT="com.apple.developer.fileprovider.testing-mode"

fail() {
  printf 'build dev bundle test: %s\n' "$*" >&2
  exit 1
}

assert_file_exists() {
  [[ -e "$1" ]] || fail "expected file to exist: $1"
}

assert_contains() {
  local value="$1"
  local expected="$2"
  local description="$3"
  if ! grep -F -q -- "${expected}" <<<"${value}"; then
    fail "${description}: expected to find ${expected}"
  fi
}

assert_not_contains() {
  local value="$1"
  local unexpected="$2"
  local description="$3"
  if grep -F -q -- "${unexpected}" <<<"${value}"; then
    fail "${description}: did not expect to find ${unexpected}"
  fi
}

plist_print() {
  "${PLISTBUDDY}" -c "Print :$2" "$1"
}

swiftc_invocation_containing() {
  local needle="$1"
  awk -v needle="${needle}" '
    /^BEGIN$/ { block = ""; next }
    /^END$/ {
      if (index(block, needle) > 0) {
        printf "%s", block
      }
      block = ""
      next
    }
    { block = block $0 "\n" }
  ' "${SWIFTC_LOG}"
}

codesign_invocation_for_target() {
  local target="$1"
  awk -v target="${target}" '
    /^BEGIN$/ { block = ""; last = ""; next }
    /^END$/ {
      if (last == target) {
        printf "%s", block
      }
      block = ""
      last = ""
      next
    }
    {
      block = block $0 "\n"
      last = $0
    }
  ' "${CODESIGN_LOG}"
}

path_snapshot() {
  local path="$1"
  if [[ ! -e "${path}" ]]; then
    printf 'missing\n'
    return 0
  fi

  find "${path}" -print | LC_ALL=C sort | while IFS= read -r item; do
    stat -f '%N|%HT|%i|%m|%c|%z' "${item}"
  done
}

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-build-dev-bundle-test.XXXXXX")"
default_build_root="${ROOT}/.build/dev-bundle"
default_app="${default_build_root}/Locality.app"
default_build_root_existed=0
default_app_existed=0
default_build_root_snapshot="$(path_snapshot "${default_build_root}")"
cleanup() {
  rm -rf "${tmp_root}"
}
trap cleanup EXIT

if [[ -e "${default_build_root}" ]]; then
  default_build_root_existed=1
fi
if [[ -e "${default_app}" ]]; then
  default_app_existed=1
fi

stub_bin="${tmp_root}/bin"
sdk_root="${tmp_root}/MacOSX.sdk"
build_root="${tmp_root}/custom-build-root"
log_root="${tmp_root}/logs"
mkdir -p "${stub_bin}" "${sdk_root}" "${log_root}"
SWIFTC_LOG="${log_root}/swiftc.log"
CODESIGN_LOG="${log_root}/codesign.log"
export SWIFTC_LOG CODESIGN_LOG

cat >"${sdk_root}/SDKSettings.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CanonicalName</key>
  <string>macosx15.5</string>
  <key>ProductBuildVersion</key>
  <string>24F74</string>
  <key>Version</key>
  <string>15.5</string>
</dict>
</plist>
PLIST

cat >"${stub_bin}/swiftc" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf 'BEGIN\n' >>"${SWIFTC_LOG}"
for arg in "$@"; do
  printf '%s\n' "${arg}" >>"${SWIFTC_LOG}"
done
printf 'END\n' >>"${SWIFTC_LOG}"

output=""
module=""
previous=""
for arg in "$@"; do
  if [[ "${previous}" == "-o" ]]; then
    output="${arg}"
  elif [[ "${previous}" == "-emit-module-path" ]]; then
    module="${arg}"
  fi
  previous="${arg}"
done

if [[ -n "${module}" ]]; then
  mkdir -p "$(dirname "${module}")"
  printf 'stub module\n' >"${module}"
fi
if [[ -n "${output}" ]]; then
  mkdir -p "$(dirname "${output}")"
  printf '#!/usr/bin/env bash\nexit 0\n' >"${output}"
  chmod +x "${output}"
fi
STUB
chmod +x "${stub_bin}/swiftc"

cat >"${stub_bin}/codesign" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf 'BEGIN\n' >>"${CODESIGN_LOG}"
for arg in "$@"; do
  printf '%s\n' "${arg}" >>"${CODESIGN_LOG}"
done
printf 'END\n' >>"${CODESIGN_LOG}"
STUB
chmod +x "${stub_bin}/codesign"

cat >"${stub_bin}/sw_vers" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "-buildVersion" ]]; then
  printf '23F79\n'
  exit 0
fi
printf 'unexpected sw_vers arguments: %s\n' "$*" >&2
exit 2
STUB
chmod +x "${stub_bin}/sw_vers"

cat >"${stub_bin}/xcrun" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == "--sdk macosx --show-sdk-path" ]]; then
  printf '%s\n' "${LOCALITY_TEST_SDK_ROOT}"
  exit 0
fi
printf 'unexpected xcrun arguments: %s\n' "$*" >&2
exit 2
STUB
chmod +x "${stub_bin}/xcrun"

PATH="${stub_bin}:${PATH}" \
  LOCALITY_TEST_SDK_ROOT="${sdk_root}" \
  LOCALITY_FILE_PROVIDER_BUILD_ROOT="${build_root}" \
  APPLE_SIGNING_IDENTITY="-" \
  bash "${BUILD_SCRIPT}" >/dev/null

if [[ "${default_build_root_existed}" == "0" ]]; then
  [[ ! -e "${default_build_root}" ]] \
    || fail "default build root was created despite LOCALITY_FILE_PROVIDER_BUILD_ROOT"
fi
if [[ "${default_app_existed}" == "0" ]]; then
  [[ ! -e "${default_app}" ]] \
    || fail "default build root app was created despite LOCALITY_FILE_PROVIDER_BUILD_ROOT"
fi
[[ "$(path_snapshot "${default_build_root}")" == "${default_build_root_snapshot}" ]] \
  || fail "default build root changed despite LOCALITY_FILE_PROVIDER_BUILD_ROOT"

app="${build_root}/Locality.app"
appex="${app}/Contents/PlugIns/LocalityFileProvider.appex"
app_plist="${app}/Contents/Info.plist"
appex_plist="${appex}/Contents/Info.plist"
helper_plist="${ROOT}/App/LocalityFileProviderCtl.Info.plist"

assert_file_exists "${app_plist}"
assert_file_exists "${appex_plist}"

app_identifier="$(plist_print "${app_plist}" CFBundleIdentifier)"
appex_identifier="$(plist_print "${appex_plist}" CFBundleIdentifier)"
helper_identifier="$(plist_print "${helper_plist}" CFBundleIdentifier)"
[[ "${app_identifier}" == "ai.codeflash.locality.Locality" ]] \
  || fail "unexpected host app bundle identifier: ${app_identifier}"
[[ "${appex_identifier}" == "${app_identifier}."* ]] \
  || fail "appex bundle identifier must be contained by host app identifier"
[[ "${helper_identifier}" == "${app_identifier}."* ]] \
  || fail "helper bundle identifier must be contained by host app identifier"

extension_invocation="$(swiftc_invocation_containing "${appex}/Contents/MacOS/LocalityFileProvider")"
host_invocation="$(swiftc_invocation_containing "${app}/Contents/MacOS/Locality")"
helper_invocation="$(swiftc_invocation_containing "${app}/Contents/MacOS/locality-file-providerctl")"

assert_contains "${extension_invocation}" "-application-extension" "extension swiftc invocation"
assert_contains "${extension_invocation}" "-Xcc" "extension swiftc invocation"
assert_contains "${extension_invocation}" "-fapplication-extension" "extension swiftc invocation"
assert_not_contains "${host_invocation}" "-application-extension" "host swiftc invocation"
assert_not_contains "${host_invocation}" "-fapplication-extension" "host swiftc invocation"
assert_not_contains "${helper_invocation}" "-application-extension" "helper swiftc invocation"
assert_not_contains "${helper_invocation}" "-fapplication-extension" "helper swiftc invocation"

for key in BuildMachineOSBuild DTCompiler DTPlatformBuild DTPlatformName DTPlatformVersion DTSDKBuild DTSDKName; do
  value="$(plist_print "${appex_plist}" "${key}")"
  [[ -n "${value}" ]] || fail "appex Info.plist metadata key ${key} is empty"
done
[[ "$(plist_print "${appex_plist}" BuildMachineOSBuild)" == "23F79" ]] \
  || fail "BuildMachineOSBuild should come from sw_vers"
[[ "$(plist_print "${appex_plist}" DTCompiler)" == "com.apple.compilers.llvm.clang.1_0" ]] \
  || fail "DTCompiler should describe the Apple clang compiler family"
[[ "$(plist_print "${appex_plist}" DTPlatformBuild)" == "24F74" ]] \
  || fail "DTPlatformBuild should come from SDKSettings.plist"
[[ "$(plist_print "${appex_plist}" DTPlatformName)" == "macosx" ]] \
  || fail "DTPlatformName should be macosx"
[[ "$(plist_print "${appex_plist}" DTPlatformVersion)" == "15.5" ]] \
  || fail "DTPlatformVersion should come from SDKSettings.plist"
[[ "$(plist_print "${appex_plist}" DTSDKBuild)" == "24F74" ]] \
  || fail "DTSDKBuild should come from SDKSettings.plist"
[[ "$(plist_print "${appex_plist}" DTSDKName)" == "macosx15.5" ]] \
  || fail "DTSDKName should come from SDKSettings.plist"
[[ "$(plist_print "${appex_plist}" CFBundleSupportedPlatforms:0)" == "MacOSX" ]] \
  || fail "appex Info.plist should declare MacOSX as a supported platform"

app_verify="$(codesign_invocation_for_target "${app}")"
appex_verify="$(codesign_invocation_for_target "${appex}")"
helper_verify="$(codesign_invocation_for_target "${app}/Contents/MacOS/locality-file-providerctl")"
assert_contains "${app_verify}" "--verify" "app codesign verification"
assert_contains "${app_verify}" "--strict" "app codesign verification"
assert_contains "${app_verify}" "--deep" "app codesign verification"
assert_contains "${appex_verify}" "--verify" "appex codesign verification"
assert_contains "${appex_verify}" "--strict" "appex codesign verification"
assert_contains "${appex_verify}" "--deep" "appex codesign verification"
assert_contains "${helper_verify}" "--verify" "helper codesign verification"
assert_contains "${helper_verify}" "--strict" "helper codesign verification"

if grep -R -F -q "${TESTING_ENTITLEMENT}" "${ROOT}/App"/*.entitlements; then
  fail "production/dev bundle entitlements must not contain ${TESTING_ENTITLEMENT}"
fi

printf 'build dev bundle test: ok\n'
